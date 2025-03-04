use anyhow::Result;
use bytes::{Bytes, BytesMut};
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, SampleFormat};
use fixed_resample::{FixedResampler, ResampleQuality};
use ringbuf::traits::{Consumer as _, Producer as _, Split};
use ringbuf::{HeapCons as Consumer, HeapProd as Producer};
use std::num::NonZeroUsize;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;
use tracing::{error, info, trace, warn};

use super::device::Direction;
use super::{
    device::{find_device, input_stream_config, StreamInfo},
    processor::Processor,
    OutboundAudio, StreamParams, DURATION_10MS, DURATION_20MS, OPUS_STREAM_PARAMS, SAMPLE_RATE,
};

#[derive(Debug)]
pub struct AudioRecorder {
    receiver: async_channel::Receiver<OutboundAudio>,
    // closed: Arc<AtomicBool>,
}

impl AudioRecorder {
    pub fn build(host: &cpal::Host, device: Option<&str>, processor: Processor) -> Result<Self> {
        let device = find_device(host, Direction::Input, device)?;
        let params = OPUS_STREAM_PARAMS;
        let stream_info = input_stream_config(&device, &params)?;
        let (sender, receiver) = async_channel::bounded(128);
        let closed = Arc::new(AtomicBool::new(false));
        std::thread::spawn(move || {
            info!(
                "spawn record worker: device={} stream_info={stream_info:?}",
                device.name().unwrap()
            );
            if let Err(err) = run(&device, &stream_info, sender, closed, processor) {
                error!("record worker thread failed: {err:?}");
            }
        });
        let handle = AudioRecorder { receiver };
        Ok(handle)
    }

    pub async fn recv(&self) -> Result<OutboundAudio> {
        let frame = self.receiver.recv().await?;
        Ok(frame)
    }
}

fn run(
    device: &Device,
    stream_info: &StreamInfo,
    sender: async_channel::Sender<OutboundAudio>,
    closed: Arc<AtomicBool>,
    processor: Processor,
) -> Result<()> {
    let stream_params =
        StreamParams::new(stream_info.config.sample_rate, stream_info.config.channels);
    let buffer_size = stream_params.buffer_size(DURATION_20MS) * 16;
    let (producer, consumer) = ringbuf::HeapRb::<f32>::new(buffer_size).split();
    let config = &stream_info.config;
    let stream = match stream_info.sample_format {
        SampleFormat::I8 => build_input_stream::<i8>(&device, &config, producer),
        SampleFormat::I16 => build_input_stream::<i16>(&device, &config, producer),
        SampleFormat::I32 => build_input_stream::<i32>(&device, &config, producer),
        SampleFormat::F32 => build_input_stream::<f32>(&device, &config, producer),
        sample_format => {
            tracing::error!("Unsupported sample format '{sample_format}'");
            Err(cpal::BuildStreamError::StreamConfigNotSupported)
        }
    }?;
    stream.play()?;
    let worker = RecordWorker::new(stream_params, stream, closed, consumer, sender, processor);
    // this blocks.
    worker.run()?;
    Ok(())
}

fn build_input_stream<S: dasp_sample::ToSample<f32> + cpal::SizedSample + Default>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut producer: Producer<f32>,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let mut tick = 0;
    device.build_input_stream::<S, _, _>(
        config,
        move |data: &[S], _: &_| {
            if tick == 0 {
                info!("record stream started. len={}", data.len());
            }
            for (i, sample) in data.iter().enumerate() {
                let sample: f32 = sample.to_sample();
                if let Err(_sample) = producer.try_push(sample) {
                    warn!(
                        "record stream underflow at tick {tick}, failed to push {} of {}",
                        data.len() - i,
                        data.len()
                    );
                    break;
                }
            }
            tick += 1;
        },
        |err| {
            error!("an error occurred on output stream: {}", err);
        },
        None,
    )
}

#[allow(unused)]
struct RecordWorker {
    stream: cpal::Stream,
    closed: Arc<AtomicBool>,
    consumer: Consumer<f32>,
    sender: async_channel::Sender<OutboundAudio>,
    resampler: FixedResampler<f32, 2>,
    processor: Processor,
    opus_encoder: OpusFramer,
    audio_buf: Vec<f32>,
    resampled_buf: Vec<f32>,
}

impl RecordWorker {
    pub fn new(
        record_params: StreamParams,
        stream: cpal::Stream,
        closed: Arc<AtomicBool>,
        consumer: Consumer<f32>,
        sender: async_channel::Sender<OutboundAudio>,
        processor: Processor,
    ) -> Self {
        info!("recorder worker init {record_params:?}");
        let resampler = FixedResampler::new(
            NonZeroUsize::new(record_params.channel_count as usize).unwrap(),
            record_params.sample_rate.0,
            SAMPLE_RATE.0,
            ResampleQuality::High,
            true,
        );
        let audio_buf = Vec::with_capacity(record_params.buffer_size(DURATION_20MS));
        let resampled_buf = Vec::with_capacity(OPUS_STREAM_PARAMS.buffer_size(DURATION_20MS));
        Self {
            stream,
            closed,
            consumer,
            sender,
            resampler,
            opus_encoder: OpusFramer::new(OPUS_STREAM_PARAMS),
            audio_buf,
            resampled_buf,
            processor,
        }
    }
    pub fn run(mut self) -> Result<()> {
        let mut tick = 0;
        loop {
            let start = Instant::now();
            match self.process(tick)? {
                ProcessOutcome::Empty => {}
                ProcessOutcome::Sent => {}
                ProcessOutcome::ChannelClosed => {
                    tracing::info!("closing recorder: input receiver closed.");
                    break;
                }
            }
            let sleep_time = (start + DURATION_10MS).saturating_duration_since(Instant::now());
            std::thread::sleep(sleep_time);
            tick += 1;
        }
        Ok(())
    }

    pub fn process(&mut self, tick: usize) -> Result<ProcessOutcome> {
        self.audio_buf.clear();
        for sample in self.consumer.pop_iter() {
            self.audio_buf.push(sample);
        }
        if self.audio_buf.is_empty() {
            return Ok(ProcessOutcome::Empty);
        }

        self.resampler.process_interleaved(
            &self.audio_buf,
            |samples| {
                self.resampled_buf.extend(samples);
            },
            None,
            false,
        );

        let chunk_size = OPUS_STREAM_PARAMS.buffer_size(DURATION_10MS);
        let mut stopped_after = None;
        for (i, frame) in self.resampled_buf.chunks_mut(chunk_size).enumerate() {
            if frame.len() == chunk_size {
                self.processor.process_capture_frame(frame)?;
            } else {
                stopped_after = Some(i);
                break;
            }

            for sample in frame {
                if let Some((payload, sample_count)) = self.opus_encoder.push_sample(*sample) {
                    let payload_len = payload.len();
                    let frame = OutboundAudio::Opus {
                        payload,
                        sample_count,
                    };
                    match self.sender.force_send(frame) {
                        Ok(None) => {
                            trace!("recorder[{tick}]: sent opus {sample_count}S {payload_len}B")
                        }
                        Ok(Some(_)) => warn!("record channel full, dropping oldest frame"),
                        Err(_) => return Ok(ProcessOutcome::ChannelClosed),
                    }
                }
            }
        }

        if let Some(i) = stopped_after {
            let reminder_start = chunk_size * i;
            let reminder_len = self.resampled_buf.len() - reminder_start;
            self.resampled_buf.copy_within(reminder_start.., 0);
            self.resampled_buf.truncate(reminder_len);
        } else {
            self.resampled_buf.clear();
        }
        trace!(
            "recorder[{tick}]: processing {} resampled {} ",
            self.audio_buf.len(),
            self.resampled_buf.len()
        );

        Ok(ProcessOutcome::Sent)
    }
}

enum ProcessOutcome {
    Empty,
    ChannelClosed,
    Sent,
}

pub struct OpusFramer {
    encoder: opus::Encoder,
    samples: Vec<f32>,
    out_buf: BytesMut,
    samples_per_frame: usize,
}

impl OpusFramer {
    pub fn new(params: StreamParams) -> Self {
        let samples_per_frame = params.buffer_size(DURATION_20MS);
        tracing::info!("recorder: opus params {params:?}");
        tracing::info!("recorder: opus samples per frame {samples_per_frame}");
        let encoder = opus::Encoder::new    // resampler: Option<CpalResampler>,
(
            params.sample_rate.0,
            opus::Channels::Mono,
            opus::Application::Voip,
        )
        .unwrap();
        let mut out_buf = BytesMut::new();
        out_buf.resize(samples_per_frame, 0);
        let samples = Vec::new();
        Self {
            encoder,
            out_buf,
            samples,
            samples_per_frame,
        }
    }

    pub fn push_sample(&mut self, sample: f32) -> Option<(Bytes, u32)> {
        self.samples.push(sample);
        if self.samples.len() >= self.samples_per_frame {
            let sample_count = self.samples.len() as u32;
            let size = self
                .encoder
                .encode_float(&self.samples, &mut self.out_buf)
                .expect("failed to encode");
            self.samples.clear();
            let encoded = self.out_buf.split_to(size).freeze();
            self.out_buf.resize(self.samples_per_frame, 0);
            Some((encoded, sample_count))
        } else {
            None
        }
    }
}
