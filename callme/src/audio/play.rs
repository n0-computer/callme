use anyhow::Result;
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, Sample, SampleFormat};
use fixed_resample::{FixedResampler, ResampleQuality};
use ringbuf::traits::{Consumer as _, Producer as _, Split};
use ringbuf::{HeapCons as Consumer, HeapProd as Producer};
use std::num::NonZeroUsize;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, trace, warn};

use super::device::{find_output_device, output_stream_config};
use super::processor::Processor;
use super::OPUS_STREAM_PARAMS;
use super::{
    device::StreamInfo, InboundAudio, StreamParams, DURATION_10MS, DURATION_20MS, SAMPLE_RATE,
};

#[derive(Debug)]
pub struct AudioPlayer {
    sender: async_channel::Sender<InboundAudio>,
    // closed: Arc<AtomicBool>,
}

impl AudioPlayer {
    pub fn build(host: &cpal::Host, device: Option<&str>, processor: Processor) -> Result<Self> {
        let device = find_output_device(host, device)?;
        let params = OPUS_STREAM_PARAMS;
        let stream_info = output_stream_config(&device, &params)?;
        let (sender, receiver) = async_channel::bounded(128);
        let closed = Arc::new(AtomicBool::new(false));
        std::thread::spawn(move || {
            info!(
                "spawn playback worker: device={} stream_info={stream_info:?}",
                device.name().unwrap()
            );
            if let Err(err) = run(&device, &stream_info, receiver, closed, processor) {
                error!("playback worker thread failed: {err:?}");
            }
        });
        let handle = AudioPlayer { sender };
        Ok(handle)
    }

    pub async fn send(&self, frame: InboundAudio) -> Result<()> {
        self.sender.send(frame).await?;
        Ok(())
    }
}

fn run(
    device: &Device,
    stream_info: &StreamInfo,
    receiver: async_channel::Receiver<InboundAudio>,
    closed: Arc<AtomicBool>,
    processor: Processor,
) -> Result<()> {
    let params = StreamParams::new(SAMPLE_RATE, stream_info.config.channels);
    let buffer_size = params.buffer_size(DURATION_20MS) * 16;
    let (producer, consumer) = ringbuf::HeapRb::<f32>::new(buffer_size).split();
    let config = &stream_info.config;
    let stream = match stream_info.sample_format {
        SampleFormat::I8 => build_output_stream::<i8>(&device, &config, consumer),
        SampleFormat::I16 => build_output_stream::<i16>(&device, &config, consumer),
        SampleFormat::I32 => build_output_stream::<i32>(&device, &config, consumer),
        SampleFormat::F32 => build_output_stream::<f32>(&device, &config, consumer),
        sample_format => {
            tracing::error!("Unsupported sample format '{sample_format}'");
            Err(cpal::BuildStreamError::StreamConfigNotSupported)
        }
    }?;
    stream.play()?;
    let worker = PlaybackWorker::new(params, stream, closed, producer, receiver, processor);
    // this blocks.
    worker.run()?;
    Ok(())
}

fn build_output_stream<S: dasp_sample::FromSample<f32> + cpal::SizedSample + Default>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut consumer: Consumer<f32>,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let mut cnt = 0;
    // let mut last_warning = Instant::now();
    let mut fell_behind_count = 0;
    device.build_output_stream::<S, _, _>(
        config,
        move |data: &mut [S], _: &_| {
            if cnt < 5 {
                info!("player-cb[{cnt}] stream started. len={}", data.len());
            }

            let mut output_fell_behind = None;
            for (i, sample) in data.iter_mut().enumerate() {
                *sample = match consumer.try_pop() {
                    Some(s) => s.to_sample(),
                    None => {
                        if output_fell_behind.is_none() {
                            output_fell_behind = Some(i);
                        }
                        Default::default()
                    }
                }
            }
            if let Some(count) = output_fell_behind {
                warn!(
                    "player-cb[{cnt}] underflow: missing {} of {} (+ {fell_behind_count} previous)",
                    data.len() - count,
                    data.len()
                );
                // let now = Instant::now();
                // if now.duration_since(last_warning) > Duration::from_secs(1) {
                //     warn!("player-cb[{cnt}] underflow: empty after {} of {} (+ {fell_behind_count} previous)", count, data.len());
                //     fell_behind_count = 0;
                //     last_warning = now;
                // }
                fell_behind_count += 1;
            }
            cnt += 1;
        },
        |err| {
            error!("an error occurred on output stream: {}", err);
        },
        None,
    )
}

#[allow(unused)]
struct PlaybackWorker {
    stream: cpal::Stream,
    closed: Arc<AtomicBool>,
    producer: Producer<f32>,
    receiver: async_channel::Receiver<InboundAudio>,
    params: StreamParams,
    resampler: FixedResampler<f32, 2>,
    // resampler: Option<CpalResampler>,
    opus_decoder: opus::Decoder,
    audio_buf: Vec<f32>,
    processor: Processor,
}

impl PlaybackWorker {
    pub fn new(
        playback_params: StreamParams,
        stream: cpal::Stream,
        closed: Arc<AtomicBool>,
        producer: Producer<f32>,
        receiver: async_channel::Receiver<InboundAudio>,
        processor: Processor,
    ) -> Self {
        let resampler = FixedResampler::new(
            NonZeroUsize::new(playback_params.channel_count as usize).unwrap(),
            SAMPLE_RATE.0,
            playback_params.sample_rate.0,
            ResampleQuality::High,
            true,
        );
        let opus_decoder = opus::Decoder::new(SAMPLE_RATE.0, opus::Channels::Mono).unwrap();
        let buffer_size = OPUS_STREAM_PARAMS.buffer_size(DURATION_20MS);
        let audio_buf = vec![0.; buffer_size];
        Self {
            params: playback_params,
            stream,
            closed,
            producer,
            receiver,
            resampler,
            opus_decoder,
            audio_buf,
            processor,
        }
    }
    pub fn run(mut self) -> Result<()> {
        let mut tick = 0;
        loop {
            let Ok(frame) = self.receiver.recv_blocking() else {
                debug!("stopping encoder thread: channel closed");
                return Ok(());
            };
            // send some silence
            if tick == 0 {
                let silence = vec![0f32; self.params.buffer_size(Duration::from_millis(20))];
                let n = self.producer.push_slice(&silence);
                info!("player[{tick}] pushed {n} samples of silence");
            }
            self.process(tick, frame)?;
            tick += 1;
        }
    }

    pub fn process(&mut self, tick: usize, frame: InboundAudio) -> Result<()> {
        let InboundAudio::Opus {
            payload,
            skipped_frames,
        } = frame;

        for _ in 0..skipped_frames.unwrap_or(0) {
            let n = self
                .opus_decoder
                .decode_float(&[], &mut self.audio_buf, false)?;
            trace!("player[{tick}]: {n} from skipped");
            self.process_samples(n)?;
        }

        let n = self
            .opus_decoder
            .decode_float(&payload, &mut self.audio_buf, false)?;
        trace!("player[{tick}]: {n} from data");
        self.process_samples(n)?;
        Ok(())
    }

    pub fn process_samples(&mut self, n: usize) -> Result<()> {
        let samples = &mut self.audio_buf[..n];

        // echo cancellation
        let chunk_size = OPUS_STREAM_PARAMS.buffer_size(DURATION_10MS);
        for (_i, frame) in samples.chunks_mut(chunk_size).enumerate() {
            if frame.len() == chunk_size {
                self.processor.process_render_frame(frame)?;
            } else {
                warn!(
                    "skipped echo processing: received invalid frame len of {}",
                    frame.len()
                );
            }
        }

        let samples = &self.audio_buf[..n];

        // Do nothing if there are no samples sent..
        if samples.is_empty() {
            return Ok(());
        }

        // TODO: Echo cancellation.

        self.resampler.process_interleaved(
            samples,
            |samples| {
                let n = self.producer.push_slice(samples);
                if n < samples.len() {
                    warn!(
                        "player worker dropping samples: channel full after {} of {}",
                        n,
                        samples.len()
                    );
                }
            },
            None,
            false,
        );
        Ok(())
    }
}
