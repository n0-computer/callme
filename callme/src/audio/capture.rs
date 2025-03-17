use std::{
    cmp::Ordering,
    num::NonZeroUsize,
    ops::ControlFlow,
    sync::{
        atomic::{AtomicBool, AtomicU64},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use bytes::{Bytes, BytesMut};
use cpal::{
    traits::{DeviceTrait, StreamTrait},
    Device, SampleFormat,
};
use dasp_sample::ToSample;
use fixed_resample::{FixedResampler, ResampleQuality};
use ringbuf::{
    traits::{Consumer as _, Observer, Producer as _, Split},
    HeapCons as Consumer, HeapProd as Producer,
};
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, error, info, span, trace, trace_span, warn, Level};

use super::{
    device::{find_device, find_input_stream_config, Direction, StreamConfigWithFormat},
    AudioFormat, WebrtcAudioProcessor, DURATION_10MS, DURATION_20MS, ENGINE_FORMAT, SAMPLE_RATE,
};
use crate::{
    codec::opus::MediaTrackOpusEncoder,
    rtc::{MediaFrame, MediaTrack, TrackKind},
};

pub trait AudioSink: Send + 'static {
    fn tick(&mut self, buf: &[f32]) -> Result<ControlFlow<(), ()>>;
}

#[derive(Debug, Clone)]
pub struct AudioCapture {
    sink_sender: mpsc::Sender<Box<dyn AudioSink>>,
}

impl AudioCapture {
    pub async fn build(
        host: &cpal::Host,
        device: Option<&str>,
        processor: WebrtcAudioProcessor,
    ) -> Result<Self> {
        let device = find_device(host, Direction::Capture, device)?;

        // find a config for the capture stream. note that the returned config may not
        // match the format. the passed format is a hint as to which stream config
        // to prefer if there are multiple. if no matching format is found, the
        // device's default stream config is used.
        let stream_config = find_input_stream_config(&device, &ENGINE_FORMAT)?;

        let buffer_size = ENGINE_FORMAT.sample_count(DURATION_20MS) * 16;
        let (producer, consumer) = ringbuf::HeapRb::<f32>::new(buffer_size).split();

        // a channel to pass new sinks to the the audio thread.
        let (sink_sender, sink_receiver) = mpsc::channel(16);

        let (init_tx, init_rx) = oneshot::channel();
        std::thread::spawn(move || {
            if let Err(err) = audio_thread_priority::promote_current_thread_to_real_time(
                buffer_size as u32,
                ENGINE_FORMAT.sample_rate.0,
            ) {
                warn!("failed to set capture thread to realtime priority: {err:?}");
            }

            let stream = match start_capture_stream(&device, &stream_config, producer, processor) {
                Ok(stream) => {
                    init_tx.send(Ok(())).unwrap();
                    stream
                }
                Err(err) => {
                    let err = err.context("failed to start capture stream");
                    init_tx.send(Err(err)).unwrap();
                    return;
                }
            };
            capture_loop(consumer, sink_receiver);
            drop(stream);
        });
        init_rx.await??;
        let handle = AudioCapture { sink_sender };
        Ok(handle)
    }

    pub async fn add_sink(&self, sink: impl AudioSink) -> Result<()> {
        self.sink_sender
            .send(Box::new(sink))
            .await
            .map_err(|_| anyhow!("failed to add captue sink: capture loop dead"))
    }

    pub async fn create_opus_track(&self) -> Result<MediaTrack> {
        let (encoder, track) = MediaTrackOpusEncoder::new(16, ENGINE_FORMAT)?;
        self.add_sink(encoder).await?;
        Ok(track)
    }
}

fn start_capture_stream(
    device: &Device,
    stream_config: &StreamConfigWithFormat,
    producer: Producer<f32>,
    processor: WebrtcAudioProcessor,
) -> Result<cpal::Stream> {
    let d = device.name()?;
    let config = &stream_config.config;

    #[cfg(feature = "audio-processing")]
    processor.init_capture(config.channels as usize)?;

    let capture_format = stream_config.audio_format();

    let resampler = FixedResampler::new(
        NonZeroUsize::new(ENGINE_FORMAT.channel_count as usize).unwrap(),
        capture_format.sample_rate.0,
        ENGINE_FORMAT.sample_rate.0,
        ResampleQuality::High,
        true,
    );
    let state = CaptureState {
        format: capture_format,
        producer,
        processor: processor.clone(),
        resampler,
    };
    let stream = match stream_config.sample_format {
        SampleFormat::I8 => build_capture_stream::<i8>(device, config, state),
        SampleFormat::I16 => build_capture_stream::<i16>(device, config, state),
        SampleFormat::I32 => build_capture_stream::<i32>(device, config, state),
        SampleFormat::F32 => build_capture_stream::<f32>(device, config, state),
        sample_format => {
            tracing::error!("Unsupported sample format '{sample_format}'");
            Err(cpal::BuildStreamError::StreamConfigNotSupported)
        }
    }
    .with_context(|| format!("failed to build capture stream on {d} with {capture_format:?}"))?;
    info!("starting capture stream on {d} with {capture_format:?}");
    stream.play()?;
    Ok(stream)
}

struct CaptureState {
    format: AudioFormat,
    producer: Producer<f32>,
    #[allow(unused)]
    processor: WebrtcAudioProcessor,
    resampler: FixedResampler<f32, 2>,
}

fn build_capture_stream<S: dasp_sample::ToSample<f32> + cpal::SizedSample + Default>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut state: CaptureState,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let mut tick = 0;
    let span = trace_span!("capture-cb");

    // if we change this, code in here needs to change, so let's assert it
    debug_assert_eq!(ENGINE_FORMAT.channel_count, 2);
    debug_assert!(matches!(state.format.channel_count, 1 | 2));

    // this needs to be at 10ms = 480 samples per channel, otherwise
    // the WebrtcAudioProcessor panics.
    let processor_chunk_size = ENGINE_FORMAT.sample_count(DURATION_10MS);
    let mut resampled_buf: Vec<f32> = Vec::with_capacity(processor_chunk_size);

    // this will grow as needed and contains samples directly from the input buf
    // (before resampling) but with channels adjusted
    let mut input_buf: Vec<f32> = Vec::with_capacity(processor_chunk_size);

    device.build_input_stream::<S, _, _>(
        config,
        move |data: &[S], info: &_| {
            let _guard = span.enter();
            let start = Instant::now();
            let max_tick_time = state.format.duration_from_sample_count(data.len());

            let delay = {
                let capture_delay = info
                    .timestamp()
                    .callback
                    .duration_since(&info.timestamp().capture)
                    .unwrap_or_default();
                let resampler_delay = Duration::from_secs_f32(
                    state.resampler.output_delay() as f32 / ENGINE_FORMAT.sample_rate.0 as f32,
                );
                capture_delay + resampler_delay
            };

            // adjust sample format and channel count.
            // we convert to ENGINE_FORMAT here which always has two channels (asserted above).
            if state.format.channel_count == 1 {
                input_buf.extend(
                    data.iter()
                        .map(|s| s.to_sample())
                        .flat_map(|s| [s, s].into_iter()),
                );
            } else if state.format.channel_count == 2 {
                input_buf.extend(data.iter().map(|s| s.to_sample()));
            } else {
                // checked above.
                unreachable!()
            };

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

            // update capture delay in processor
            #[cfg(feature = "audio-processing")]
            state.processor.set_capture_delay(delay);

            // process, and push processed chunks to the producer
            let mut chunks = resampled_buf.chunks_exact_mut(processor_chunk_size);
            let mut pushed = 0;
            for chunk in &mut chunks {
                #[cfg(feature = "audio-processing")]
                state.processor.process_capture_frame(chunk).unwrap();

                let n = state.producer.push_slice(chunk);
                pushed += n;

                if n < chunk.len() {
                    warn!(
                        "record xrun: failed to push out {} of {}",
                        chunk.len() - n,
                        chunk.len()
                    );
                    break;
                }
            }

            // cleanup: we need to keep the unprocessed samples that are still in the resampled buf
            let remainder_len = chunks.into_remainder().len();
            let end = resampled_buf.len() - remainder_len;
            resampled_buf.copy_within(end.., 0);
            resampled_buf.truncate(remainder_len);

            trace!(
                "tick {tick}: delay={:?} available={:?} time={:?} / get {} push {} samples",
                delay,
                max_tick_time,
                start.elapsed(),
                data.len(),
                pushed
            );
            tick += 1;
        },
        |err| {
            error!("an error occurred on output stream: {}", err);
        },
        None,
    )
}

fn capture_loop(
    mut consumer: Consumer<f32>,
    mut sink_receiver: mpsc::Receiver<Box<dyn AudioSink>>,
) {
    let span = tracing::span!(Level::TRACE, "capture-loop");
    let _guard = span.enter();
    info!("capture loop start");

    let tick_duration = DURATION_20MS;
    let samples_per_tick = ENGINE_FORMAT.sample_count(tick_duration);
    let mut buf = vec![0.; samples_per_tick];
    let mut sinks = vec![];

    let mut tick = 0;
    loop {
        let start = Instant::now();

        // poll incoming sources
        loop {
            match sink_receiver.try_recv() {
                Ok(sink) => {
                    info!("new sink added to capture loop");
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
        trace!("tick {tick} took {:?} pulled {count}", start.elapsed());
        if start.elapsed() > tick_duration {
            warn!(
                "capture thread tick exceeded interval (took {:?})",
                start.elapsed()
            );
        } else {
            let sleep_time = tick_duration.saturating_sub(start.elapsed());
            spin_sleep::sleep(sleep_time);
        }
        tick += 1;
    }
}
