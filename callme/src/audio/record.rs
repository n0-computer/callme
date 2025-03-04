use anyhow::Result;
use bytes::{Bytes, BytesMut};
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, SampleFormat};
use fixed_resample::{FixedResampler, ResampleQuality};
use ringbuf::traits::{Consumer as _, Observer, Producer as _, Split};
use ringbuf::{HeapCons as Consumer, HeapProd as Producer};
use std::cmp::Ordering;
use std::num::NonZeroUsize;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, AtomicU64};
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
    let capture_params =
        StreamParams::new(stream_info.config.sample_rate, stream_info.config.channels);
    let buffer_size = capture_params.buffer_size(DURATION_20MS) * 16;
    let (producer, consumer) = ringbuf::HeapRb::<f32>::new(buffer_size).split();
    let config = &stream_info.config;
    let state = CaptureState {
        params: capture_params.clone(),
        producer,
        processor: processor.clone(),
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

    // let worker = RecordWorker::new(stream_params, stream, closed, consumer, sender, p);

    let resampler = FixedResampler::new(
        NonZeroUsize::new(capture_params.channel_count as usize).unwrap(),
        capture_params.sample_rate.0,
        SAMPLE_RATE.0,
        ResampleQuality::High,
        true,
    );
    let worker = RecordWorker {
        stream,
        closed,
        consumer,
        sender,
        resampler,
        opus_encoder: OpusFramer::new(OPUS_STREAM_PARAMS),
        audio_buf: Vec::with_capacity(capture_params.buffer_size(DURATION_20MS)),
        resampled_buf: Vec::with_capacity(OPUS_STREAM_PARAMS.buffer_size(DURATION_20MS)),
        processor,
    };
    // this blocks.
    worker.run()?;
    Ok(())
}

struct CaptureState {
    params: StreamParams,
    producer: Producer<f32>,
    processor: Processor,
}

fn build_input_stream<S: dasp_sample::ToSample<f32> + cpal::SizedSample + Default>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut state: CaptureState,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let mut tick = 0;
    // TODO: remove once resampler is readded
    assert_eq!(state.params.sample_rate, SAMPLE_RATE);
    let frame_size = state.params.buffer_size(DURATION_10MS);
    let mut input_buf: Vec<f32> = Vec::with_capacity(frame_size);
    device.build_input_stream::<S, _, _>(
        config,
        move |data: &[S], info: &_| {
            if tick < 5 {
                info!("record stream started. len={}", data.len());
            }

            let capture_delay = info
                .timestamp()
                .callback
                .duration_since(&info.timestamp().capture)
                .unwrap_or_default();
            state.processor.set_capture_delay(capture_delay);

            for s in data {
                input_buf.push(s.to_sample());
                if input_buf.len() == frame_size {
                    state
                        .processor
                        .process_capture_frame(&mut input_buf[..])
                        .unwrap();
                    let n = state.producer.push_slice(&input_buf);
                    if n < data.len() {
                        warn!(
                            "record overflow: failed to push {} of {}",
                            data.len() - n,
                            data.len()
                        );
                    }
                    input_buf.clear();
                }
            }
            // for (_i, sample) in data.iter().enumerate() {
            //     buf.push(sample.to_sample());
            //     if buf.len() == buf_size {
            //         state
            //             .processor
            //             .process_render_frame(&mut buf)
            //             .expect("record: failed to run processor");
            //         let n = state.producer.push_slice(&buf);
            //         if n < buf.len() {
            //             warn!(
            //                 "record stream underflow at tick {tick}, failed to push {} of {}",
            //                 buf.len() - n,
            //                 buf.len()
            //             );
            //         }
            //         buf.clear();
            //     }
            // }
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
    pub fn run(mut self) -> Result<()> {
        let mut tick = 0;
        loop {
            let start = Instant::now();
            if self.process(tick)?.is_break() {
                tracing::info!("closing recorder: input receiver closed.");
                break;
            }
            if self.consumer.is_empty() {
                let sleep_time = (start + DURATION_10MS).saturating_duration_since(Instant::now());
                std::thread::sleep(sleep_time);
            }
            tick += 1;
        }
        Ok(())
    }

    pub fn process(&mut self, tick: usize) -> Result<ControlFlow<()>> {
        self.audio_buf.clear();
        for sample in self.consumer.pop_iter() {
            self.audio_buf.push(sample);
        }
        if self.audio_buf.is_empty() {
            return Ok(ControlFlow::Continue(()));
        }

        for sample in &self.audio_buf {
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
                    Err(_) => return Ok(ControlFlow::Break(())),
                }
            }
        }

        Ok(ControlFlow::Continue(()))
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

// capture time notes
// let capture_time_ns = info.timestamp().callback.as_nanos();
// let now_host_time = cap::cidre::cv::current_host_time();
// let now_time_ns = host_time_to_stream_instant(now_host_time).as_nanos();
// let device_latency_us = 1e-3 * (now_time_ns - capture_time_ns) as f64;

// let buffer_latency_us = 1.0e6 * self.producer.occupied_len() as f64
//     / self.params.sample_rate.0 as f64
//     / self.params.channel_count as f64
//     + 0.5;

// let capture_total_delay =
//     1e-3 * (buffer_latency_us + device_latency_us as f64) + 0.5;
// self.capture_delay_ms
//     .store(capture_total_delay as u64, Ordering::Relaxed);

// old  worker
// self.resampler.process_interleaved(
//     &self.audio_buf,
//     |samples| {
//         self.resampled_buf.extend(samples);
//     },
//     None,
//     false,
// );

// let chunk_size = OPUS_STREAM_PARAMS.buffer_size(DURATION_10MS);
// let mut stopped_after = None;
// for (i, frame) in self.resampled_buf.chunks_mut(chunk_size).enumerate() {
//     if frame.len() == chunk_size {
//         self.processor.process_capture_frame(frame)?;
//     } else {
//         stopped_after = Some(i);
//         break;
//     }

//     for sample in frame {
//         if let Some((payload, sample_count)) = self.opus_encoder.push_sample(*sample) {
//             let payload_len = payload.len();
//             let frame = OutboundAudio::Opus {
//                 payload,
//                 sample_count,
//             };
//             match self.sender.force_send(frame) {
//                 Ok(None) => {
//                     trace!("recorder[{tick}]: sent opus {sample_count}S {payload_len}B")
//                 }
//                 Ok(Some(_)) => warn!("record channel full, dropping oldest frame"),
//                 Err(_) => return Ok(ProcessOutcome::ChannelClosed),
//             }
//         }
//     }
// }

// if let Some(i) = stopped_after {
//     let reminder_start = chunk_size * i;
//     let reminder_len = self.resampled_buf.len() - reminder_start;
//     self.resampled_buf.copy_within(reminder_start.., 0);
//     self.resampled_buf.truncate(reminder_len);
// } else {
//     self.resampled_buf.clear();
// }
// trace!(
//     "recorder[{tick}]: processing {} resampled {} ",
//     self.audio_buf.len(),
//     self.resampled_buf.len()
// );
