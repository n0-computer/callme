use anyhow::{anyhow, bail, Result};
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
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, error, info, span, trace, warn, Level};

use crate::codec::opus::MediaTrackOpusEncoder;
use crate::rtc::{MediaFrame, MediaTrack, TrackKind};

use super::device::Direction;
use super::{
    device::{find_device, input_stream_config, StreamInfo},
    processor::WebrtcAudioProcessor,
    StreamParams, DURATION_10MS, DURATION_20MS, OPUS_STREAM_PARAMS, SAMPLE_RATE,
};

pub trait AudioSink: Send + 'static {
    fn tick(&mut self, buf: &[f32]) -> Result<ControlFlow<(), ()>>;
}

#[derive(Debug, Clone)]
pub struct AudioRecorder {
    sink_sender: mpsc::Sender<Box<dyn AudioSink>>,
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

        let capture_params =
            StreamParams::new(stream_info.config.sample_rate, stream_info.config.channels);
        let buffer_size = capture_params.frame_buffer_size(DURATION_20MS) * 16;
        let (producer, consumer) = ringbuf::HeapRb::<f32>::new(buffer_size).split();

        let (sink_sender, sink_receiver) = mpsc::channel(16);

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
            capture_loop(params, consumer, sink_receiver);
            drop(stream);
        });
        init_rx.await??;
        let handle = AudioRecorder { sink_sender };
        Ok(handle)
    }

    pub async fn add_sink(&self, sink: impl AudioSink) -> Result<()> {
        self.sink_sender
            .send(Box::new(sink))
            .await
            .map_err(|_| anyhow!("failed to add captue sink: capture loop dead"))
    }

    pub async fn create_opus_track(&self) -> Result<MediaTrack> {
        let (encoder, track) = MediaTrackOpusEncoder::new(16, OPUS_STREAM_PARAMS)?;
        self.add_sink(encoder).await?;
        Ok(track)
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
    let processor_frame_size = state.params.frame_buffer_size(DURATION_10MS);
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

fn capture_loop(
    params: StreamParams,
    mut consumer: Consumer<f32>,
    mut sink_receiver: mpsc::Receiver<Box<dyn AudioSink>>,
) {
    let span = tracing::span!(Level::TRACE, "capture-loop");
    let _guard = span.enter();
    info!("capture loop start");

    let tick_duration = DURATION_20MS;
    let frame_size = params.frame_buffer_size(tick_duration);
    let mut buf = vec![0.; frame_size];
    let mut sinks = vec![];
    loop {
        let start = Instant::now();

        // poll incoming sources
        loop {
            match sink_receiver.try_recv() {
                Ok(sink) => {
                    info!("add new track to decoder");
                    sinks.push(sink);
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    info!("stop playback mixer loop: channel closed");
                    return;
                }
            }
        }
        let count = consumer.pop_slice(&mut buf);

        sinks.retain_mut(|sink| match sink.tick(&buf[..count]) {
            Ok(ControlFlow::Continue(())) => true,
            Ok(ControlFlow::Break(())) => {
                debug!("remove decoder: closed");
                false
            }
            Err(err) => {
                warn!("remove decoder: failed {err:?}");
                false
            }
        });
        let sleep_time = tick_duration.saturating_sub(start.elapsed());
        trace!("sleep {sleep_time:?}");
        std::thread::sleep(sleep_time);
    }
}
