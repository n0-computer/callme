use anyhow::Result;
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, Sample, SampleFormat};
use fixed_resample::{FixedResampler, ResampleQuality};
use rb::{Consumer, Producer, RbConsumer, RbProducer, RB};
use std::num::NonZeroUsize;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tracing::{debug, error, info};

use super::device::{find_output_device, output_stream_config};
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
    pub fn build(host: &cpal::Host, device: Option<&str>) -> Result<Self> {
        let device = find_output_device(host, device)?;
        let params = StreamParams::new(SAMPLE_RATE, 2);
        let stream_info = output_stream_config(&device, params.buffer_size(DURATION_10MS))?;
        let (sender, receiver) = async_channel::bounded(128);
        let closed = Arc::new(AtomicBool::new(false));
        std::thread::spawn(move || {
            info!(
                "spawn playback worker: device={} stream_info={stream_info:?}",
                device.name().unwrap()
            );
            if let Err(err) = run_worker(&device, &stream_info, receiver, closed) {
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

fn run_worker(
    device: &Device,
    stream_info: &StreamInfo,
    receiver: async_channel::Receiver<InboundAudio>,
    closed: Arc<AtomicBool>,
) -> Result<()> {
    let params = StreamParams::new(SAMPLE_RATE, stream_info.config.channels);
    let buffer_size = params.buffer_size(DURATION_20MS) * 4;
    let rb = rb::SpscRb::new(buffer_size);
    let producer = rb.producer();
    let consumer = rb.consumer();
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
    let worker = PlaybackWorker::new(params, stream, closed, producer, receiver);
    // this blocks.
    worker.run()?;
    Ok(())
}

fn build_output_stream<S: dasp_sample::FromSample<f32> + cpal::SizedSample + Default>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    buffer_consumer: Consumer<f32>,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let mut buffer = Vec::new();
    let mut cnt = 0;
    device.build_output_stream::<S, _, _>(
        config,
        move |data: &mut [S], _: &_| {
            if cnt == 0 {
                info!("playback stream started. len={}", data.len());
            }
            buffer.resize(data.len(), 0.);
            // Read from the ring buffer, and write them to the data array
            let written = buffer_consumer.read(&mut buffer).unwrap_or(0);
            if cnt < 5 {
                info!("  written {written}")
            }
            for i in 0..written {
                data[i] = buffer[i].to_sample();
            }

            // Data expects a certain number of samples, if we didn't get enough from above,
            // mute anything afterwards as we're probably EoS
            data[written..]
                .iter_mut()
                .for_each(|s| *s = Default::default());
            cnt = cnt + 1;
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
}

impl PlaybackWorker {
    pub fn new(
        playback_params: StreamParams,
        stream: cpal::Stream,
        closed: Arc<AtomicBool>,
        producer: Producer<f32>,
        receiver: async_channel::Receiver<InboundAudio>,
    ) -> Self {
        let resampler = FixedResampler::new(
            NonZeroUsize::new(playback_params.channel_count as usize).unwrap(),
            SAMPLE_RATE.0,
            playback_params.sample_rate.0,
            ResampleQuality::High,
            true,
        );
        let opus_decoder = opus::Decoder::new(SAMPLE_RATE.0, opus::Channels::Stereo).unwrap();
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
        }
    }
    pub fn run(mut self) -> Result<()> {
        loop {
            let Ok(frame) = self.receiver.recv_blocking() else {
                debug!("stopping encoder thread: channel closed");
                return Ok(());
            };
            self.handle_incoming(frame)?;
        }
    }

    pub fn handle_incoming(&mut self, frame: InboundAudio) -> Result<()> {
        let InboundAudio::Opus {
            payload,
            skipped_samples: _,
            skipped_frames,
        } = frame;

        for _ in 0..skipped_frames.unwrap_or(0) {
            self.opus_decoder.decode_float(&[], &mut [], false)?;
        }

        let decoded = self
            .opus_decoder
            .decode_float(&payload, &mut self.audio_buf, false)?;
        let samples = &self.audio_buf[..decoded];

        // Do nothing if there are no samples sent..
        if samples.is_empty() {
            return Ok(());
        }

        // TODO: Echo cancellation.

        self.resampler.process_interleaved(
            samples,
            |samples| {
                let mut position = 0;
                while let Some(written) = self.producer.write_blocking(samples.split_at(position).1)
                {
                    position += written;
                }
            },
            None,
            false,
        );
        Ok(())
    }
}
