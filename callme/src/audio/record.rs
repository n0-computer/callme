use anyhow::Result;
use bytes::{Bytes, BytesMut};
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, SampleFormat};
use fixed_resample::{FixedResampler, ResampleQuality};
use rb::{Consumer, Producer, RbConsumer, RbProducer, RB};
use std::num::NonZeroUsize;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tracing::{error, info, warn};

use super::device::input_stream_config;
use super::OPUS_STREAM_PARAMS;
use super::{
    device::find_input_device, device::StreamInfo, OutboundAudio, StreamParams, DURATION_10MS,
    DURATION_20MS, SAMPLE_RATE,
};

#[derive(Debug)]
pub struct AudioRecorder {
    receiver: async_channel::Receiver<OutboundAudio>,
    // closed: Arc<AtomicBool>,
}

impl AudioRecorder {
    pub fn build(host: &cpal::Host, device: Option<&str>) -> Result<Self> {
        let device = find_input_device(host, device)?;
        let params = StreamParams::new(SAMPLE_RATE, 2);
        let stream_info = input_stream_config(&device, params.buffer_size(DURATION_10MS))?;
        let (sender, receiver) = async_channel::bounded(128);
        let closed = Arc::new(AtomicBool::new(false));
        std::thread::spawn(move || {
            info!(
                "spawn record worker: device={} stream_info={stream_info:?}",
                device.name().unwrap()
            );
            if let Err(err) = run_worker(&device, &stream_info, sender, closed) {
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

fn run_worker(
    device: &Device,
    stream_info: &StreamInfo,
    sender: async_channel::Sender<OutboundAudio>,
    closed: Arc<AtomicBool>,
) -> Result<()> {
    let stream_params =
        StreamParams::new(stream_info.config.sample_rate, stream_info.config.channels);
    let buffer_size = stream_params.buffer_size(DURATION_20MS) * 4;
    let rb = rb::SpscRb::new(buffer_size);
    let producer = rb.producer();
    let consumer = rb.consumer();
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
    let worker = RecordWorker::new(stream_params, stream, closed, consumer, sender);
    // this blocks.
    worker.run()?;
    Ok(())
}

fn build_input_stream<S: dasp_sample::ToSample<f32> + cpal::SizedSample + Default>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    buffer_producer: Producer<f32>,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let mut buffer: Vec<f32> = Vec::new();
    let mut cnt = 0;
    device.build_input_stream::<S, _, _>(
        config,
        move |data: &[S], _: &_| {
            if cnt == 0 {
                info!("record stream started. len={}", data.len());
            }
            let len = data.len();
            buffer.resize(len, 0.);
            for i in 0..len {
                buffer[i] = data[i].to_sample();
            }
            let written = buffer_producer.write(&buffer[..len]).unwrap_or(0);
            if written < len && cnt < 5 {
                warn!(
                    "input processing is too slow: recorder fell behind by {} [{cnt}]",
                    len - written
                );
            }
            cnt = cnt + 1;
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
    // resampler: Option<CpalResampler>,
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
    ) -> Self {
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
        }
    }
    pub fn run(mut self) -> Result<()> {
        loop {
            if let Some(written) = self.consumer.read_blocking(&mut self.audio_buf) {
                self.handle_incoming(written)?;
            }
        }
    }

    pub fn handle_incoming(&mut self, written: usize) -> Result<()> {
        let samples = &self.audio_buf[..written];
        self.resampled_buf.clear();
        self.resampler.process_interleaved(
            samples,
            |samples| {
                self.resampled_buf.extend(samples);
            },
            None,
            false,
        );
        for sample in &self.resampled_buf[..] {
            if let Some((payload, sample_count)) = self.opus_encoder.push_sample(*sample) {
                match self.sender.force_send(OutboundAudio::Opus {
                    payload,
                    sample_count,
                }) {
                    Ok(None) => {}
                    Ok(Some(_)) => {
                        warn!("outbound audio frame channel overflow, dropping oldest frame");
                    }
                    Err(async_channel::SendError(_)) => {
                        tracing::info!("closing audio input thread: input receiver closed.");
                        break;
                    }
                }
            }
        }

        Ok(())
    }
}

pub struct OpusFramer {
    encoder: opus::Encoder,
    samples: Vec<f32>,
    out_buf: BytesMut,
    samples_per_frame: usize,
}

impl OpusFramer {
    pub fn new(params: StreamParams) -> Self {
        let samples_per_frame = params.frame_length(DURATION_20MS);
        let encoder = opus::Encoder::new(
            params.sample_rate.0,
            opus::Channels::Stereo,
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
