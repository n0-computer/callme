use anyhow::Result;
use bytes::{Bytes, BytesMut};
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, SampleFormat};
use dasp_sample::ToSample;
use fixed_resample::{FixedResampler, ResampleQuality};
use ringbuf::traits::{Consumer as _, Observer, Producer as _, Split};
use ringbuf::{HeapCons as Consumer, HeapProd as Producer};
use std::cmp::Ordering;
use std::num::NonZeroUsize;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tracing::{error, info, span, trace, warn, Level};

use crate::rtc::OpusChannels;

use super::device::Direction;
use super::{
    device::{find_device, input_stream_config, StreamInfo},
    processor::WebrtcAudioProcessor,
    OutboundAudio, StreamParams, DURATION_10MS, DURATION_20MS, OPUS_STREAM_PARAMS, SAMPLE_RATE,
};

#[derive(Debug)]
pub struct AudioRecorder {
    receiver: async_channel::Receiver<OutboundAudio>,
}

impl AudioRecorder {
    pub async fn build(
        host: &cpal::Host,
        device: Option<&str>,
        processor: WebrtcAudioProcessor,
    ) -> Result<Self> {
        let device = find_device(host, Direction::Input, device)?;
        let params = OPUS_STREAM_PARAMS;
        let stream_info = input_stream_config(&device, &params)?;
        let (sender, receiver) = async_channel::bounded(128);

        let capture_params =
            StreamParams::new(stream_info.config.sample_rate, stream_info.config.channels);
        let buffer_size = capture_params.buffer_size(DURATION_20MS) * 16;
        let (producer, consumer) = ringbuf::HeapRb::<f32>::new(buffer_size).split();

        let encoder = OpusEncoderLoop {
            consumer,
            sender,
            opus_encoder: OpusEncoder::new(OPUS_STREAM_PARAMS),
            audio_buf: Vec::with_capacity(capture_params.buffer_size(DURATION_20MS)),
        };
        let (init_tx, init_rx) = oneshot::channel();
        std::thread::spawn(move || {
            let stream = match start_record_stream(
                &device,
                &stream_info,
                &capture_params,
                producer,
                processor,
            ) {
                Ok(stream) => {
                    init_tx.send(Ok(())).unwrap();
                    stream
                }
                Err(err) => {
                    init_tx.send(Err(err)).unwrap();
                    return;
                }
            };
            if let Err(err) = encoder.run() {
                error!("record worker thread failed: {err:?}");
            }
            drop(stream);
        });
        init_rx.await??;
        let handle = AudioRecorder { receiver };
        Ok(handle)
    }

    pub async fn recv(&self) -> Result<OutboundAudio> {
        let frame = self.receiver.recv().await?;
        Ok(frame)
    }
}

fn start_record_stream(
    device: &Device,
    stream_info: &StreamInfo,
    capture_params: &StreamParams,
    producer: Producer<f32>,
    processor: WebrtcAudioProcessor,
) -> Result<cpal::Stream> {
    let config = &stream_info.config;
    let resampler = FixedResampler::new(
        NonZeroUsize::new(capture_params.channel_count as usize).unwrap(),
        capture_params.sample_rate.0,
        SAMPLE_RATE.0,
        ResampleQuality::High,
        true,
    );
    let state = CaptureState {
        params: capture_params.clone(),
        producer,
        processor: processor.clone(),
        resampler,
    };
    let stream = match stream_info.sample_format {
        SampleFormat::I8 => build_input_stream::<i8>(&device, &config, state),
        SampleFormat::I16 => build_input_stream::<i16>(&device, &config, state),
        SampleFormat::I32 => build_input_stream::<i32>(&device, &config, state),
        SampleFormat::F32 => build_input_stream::<f32>(&device, &config, state),
        sample_format => {
            tracing::error!("Unsupported sample format '{sample_format}'");
            Err(cpal::BuildStreamError::StreamConfigNotSupported)
        }
    }?;
    stream.play()?;
    Ok(stream)
}

struct CaptureState {
    params: StreamParams,
    producer: Producer<f32>,
    processor: WebrtcAudioProcessor,
    resampler: FixedResampler<f32, 2>,
}

fn build_input_stream<S: dasp_sample::ToSample<f32> + cpal::SizedSample + Default>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut state: CaptureState,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let mut tick = 0;
    let processor_frame_size = state.params.buffer_size(DURATION_10MS);
    let mut input_buf: Vec<f32> = Vec::with_capacity(processor_frame_size);
    let mut resampled_buf: Vec<f32> = Vec::with_capacity(processor_frame_size);
    device.build_input_stream::<S, _, _>(
        config,
        move |data: &[S], info: &_| {
            if tick < 5 {
                info!("record stream started. len={}", data.len());
            }

            // adjust sample format
            input_buf.extend(data.iter().map(|s| s.to_sample()));
            // resample
            state.resampler.process_interleaved(
                &input_buf[..],
                |samples| {
                    resampled_buf.extend(samples);
                },
                None,
                false,
            );
            input_buf.clear();

            // record delay for processor
            let capture_delay = info
                .timestamp()
                .callback
                .duration_since(&info.timestamp().capture)
                .unwrap_or_default();
            let resampler_delay =
                Duration::from_secs_f32(state.resampler.output_delay() as f32 / 48_000f32);
            // let capture_delay = capture_delay + state.resampler.output_delay();
            state
                .processor
                .set_capture_delay(capture_delay + resampler_delay);
            // process
            let mut chunks = resampled_buf.chunks_exact_mut(processor_frame_size);
            for chunk in &mut chunks {
                state.processor.process_capture_frame(chunk).unwrap();
                let n = state.producer.push_slice(&chunk);
                if n < chunk.len() {
                    warn!(
                        "record xrun: failed to push out {} of {}",
                        chunk.len() - n,
                        chunk.len()
                    );
                }
            }
            // cleanup
            let remainder_len = chunks.into_remainder().len();
            let end = resampled_buf.len() - remainder_len;
            resampled_buf.copy_within(end.., 0);
            resampled_buf.truncate(remainder_len);

            tick += 1;
        },
        |err| {
            error!("an error occurred on output stream: {}", err);
        },
        None,
    )
}

#[allow(unused)]
struct OpusEncoderLoop {
    consumer: Consumer<f32>,
    sender: async_channel::Sender<OutboundAudio>,
    opus_encoder: OpusEncoder,
    audio_buf: Vec<f32>,
}

impl OpusEncoderLoop {
    pub fn run(mut self) -> Result<()> {
        let span = span!(Level::TRACE, "opus-encoder");
        let _guard = span.enter();

        'outer: loop {
            let start = Instant::now();

            for (payload, sample_count) in self.opus_encoder.push_from_consumer(&mut self.consumer)
            {
                let payload_len = payload.len();
                let frame = OutboundAudio::Opus {
                    payload,
                    sample_count,
                };
                match self.sender.force_send(frame) {
                    Ok(None) => {
                        trace!("sent opus {sample_count}S {payload_len}B")
                    }
                    Ok(Some(_)) => warn!("record channel full, dropping oldest frame"),
                    Err(_) => {
                        tracing::info!("closing encoder loop: track receiver closed.");
                        break 'outer;
                    }
                }
            }
            let sleep_time = DURATION_20MS.saturating_sub(start.elapsed());
            std::thread::sleep(sleep_time);
        }
        Ok(())
    }
}

pub struct OpusEncoder {
    encoder: opus::Encoder,
    samples: Vec<f32>,
    out_buf: BytesMut,
    samples_per_frame: usize,
}

impl OpusEncoder {
    pub fn new(params: StreamParams) -> Self {
        let samples_per_frame = params.buffer_size(DURATION_20MS);
        tracing::info!("recorder: opus params {params:?}");
        tracing::info!("recorder: opus samples per frame {samples_per_frame}");
        let encoder = opus::Encoder::new(
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

    pub fn push_from_consumer<'a>(
        &'a mut self,
        consumer: &'a mut Consumer<f32>,
    ) -> impl Iterator<Item = (Bytes, u32)> + 'a {
        std::iter::from_fn(|| {
            if consumer.is_empty() {
                return None;
            }
            for sample in consumer.pop_iter() {
                if let Some((payload, sample_count)) = self.push_sample(sample) {
                    return Some((payload, sample_count));
                }
            }
            None
        })
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
