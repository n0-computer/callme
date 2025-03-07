use anyhow::Result;
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, Sample, SampleFormat};
use fixed_resample::{FixedResampler, ResampleQuality};
use ringbuf::traits::{Consumer as _, Observer as _, Producer as _, Split};
use ringbuf::{HeapCons as Consumer, HeapProd as Producer};
use std::num::NonZeroUsize;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, error, info, trace, warn, Level};

use crate::rtc::{Codec, MediaFrame, MediaTrack};

use super::device::{find_device, output_stream_config, Direction};
use super::processor::WebrtcAudioProcessor;
use super::OPUS_STREAM_PARAMS;
use super::{
    device::StreamInfo, InboundAudio, StreamParams, DURATION_10MS, DURATION_20MS, SAMPLE_RATE,
};

#[derive(derive_more::Debug, Clone)]
pub struct AudioPlayer {
    track_sender: mpsc::Sender<MediaTrack>,
}

impl AudioPlayer {
    pub async fn build(
        host: &cpal::Host,
        device: Option<&str>,
        processor: WebrtcAudioProcessor,
    ) -> Result<Self> {
        let device = find_device(host, Direction::Output, device)?;
        let stream_info = output_stream_config(&device, &OPUS_STREAM_PARAMS)?;

        let params = StreamParams::new(stream_info.config.sample_rate, stream_info.config.channels);
        let buffer_size = params.buffer_size(DURATION_20MS) * 4;
        let (producer, consumer) = ringbuf::HeapRb::<f32>::new(buffer_size).split();

        let (track_sender, track_receiver) = mpsc::channel(16);
        let (init_tx, init_rx) = oneshot::channel();

        std::thread::spawn(move || {
            let stream =
                match start_playback_stream(&device, &stream_info, params, processor, consumer) {
                    Ok(stream) => {
                        init_tx.send(Ok(())).unwrap();
                        stream
                    }
                    Err(err) => {
                        init_tx.send(Err(err)).unwrap();
                        return;
                    }
                };
            playback_mixer_loop(producer, track_receiver);
            drop(stream);
        });

        init_rx.await??;
        Ok(AudioPlayer { track_sender })
    }

    pub async fn add_track(&self, track: MediaTrack) -> Result<()> {
        self.track_sender.send(track).await?;
        Ok(())
    }
}

fn playback_mixer_loop(
    mut producer: Producer<f32>,
    mut track_receiver: mpsc::Receiver<MediaTrack>,
) {
    let mut decoders = vec![];
    let mut buf = vec![];
    let span = tracing::span!(Level::TRACE, "output-mixer");
    let _guard = span.enter();
    info!("playback mixer loop start");
    loop {
        let start = Instant::now();
        loop {
            match track_receiver.try_recv() {
                Ok(track) => {
                    info!("add new track to decoder");
                    decoders.push(MediaTrackOpusDecoder::new(track));
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    info!("stop playback mixer loop: channel closed");
                    return;
                }
            }
        }

        decoders.retain_mut(|decoder| match decoder.tick() {
            Ok(None) => {
                debug!("remove decoder: track closed");
                false
            }
            Ok(Some(_)) => true,
            Err(err) => {
                debug!("remove decoder: failed {err:?}");
                false
            }
        });

        let max_len = 960;
        buf.clear();
        buf.resize(max_len, 0f32);
        for (i, decoder) in decoders.iter_mut().enumerate() {
            let slice = decoder.samples(max_len);
            trace!("decoder {i}: added {} samples", slice.len());
            for (i, sample) in slice.iter().enumerate() {
                buf[i] += sample;
            }
        }
        let len = producer.push_slice(&buf[..]);
        if len < buf.len() {
            warn!("xrun: failed to push {} of {}", buf.len() - len, buf.len());
        }

        let sleep_time = DURATION_20MS.saturating_sub(start.elapsed());
        trace!("sleep {sleep_time:?}");
        std::thread::sleep(sleep_time);
    }
}

fn start_playback_stream(
    device: &Device,
    stream_info: &StreamInfo,
    params: StreamParams,
    processor: WebrtcAudioProcessor,
    consumer: Consumer<f32>,
) -> Result<cpal::Stream> {
    let config = &stream_info.config;
    let resampler = FixedResampler::new(
        NonZeroUsize::new(params.channel_count as usize).unwrap(),
        SAMPLE_RATE.0,
        params.sample_rate.0,
        ResampleQuality::High,
        true,
    );
    let state = PlaybackState {
        consumer,
        params,
        processor,
        resampler,
    };
    let stream = match stream_info.sample_format {
        SampleFormat::I8 => build_output_stream::<i8>(&device, &config, state),
        SampleFormat::I16 => build_output_stream::<i16>(&device, &config, state),
        SampleFormat::I32 => build_output_stream::<i32>(&device, &config, state),
        SampleFormat::F32 => build_output_stream::<f32>(&device, &config, state),
        sample_format => {
            tracing::error!("Unsupported sample format '{sample_format}'");
            Err(cpal::BuildStreamError::StreamConfigNotSupported)
        }
    }?;
    info!("start playback");
    stream.play()?;
    Ok(stream)
}

struct PlaybackState {
    params: StreamParams,
    resampler: FixedResampler<f32, 2>,
    processor: WebrtcAudioProcessor,
    consumer: Consumer<f32>,
}

fn build_output_stream<S: dasp_sample::FromSample<f32> + cpal::SizedSample + Default>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut state: PlaybackState,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let frame_size = state.params.buffer_size(DURATION_10MS);
    let mut unprocessed: Vec<f32> = Vec::with_capacity(frame_size);
    let mut processed: Vec<f32> = Vec::with_capacity(frame_size);
    let mut resampled: Vec<f32> = Vec::with_capacity(frame_size);
    let mut tick = 0;
    let mut last_warning = Instant::now();
    let mut underflows = 0;

    device.build_output_stream::<S, _, _>(
        config,
        move |data: &mut [S], info: &_| {
            let span = tracing::span!(Level::TRACE, "output-callback");
            let _guard = span.enter();
            if tick < 2 {
                debug!("[{tick}] stream started. len={} inc={}", data.len(), state.consumer.occupied_len());
            }

            let output_delay = info
                .timestamp()
                .callback
                .duration_since(&info.timestamp().playback)
                .unwrap_or_default();
            let resampler_delay = Duration::from_secs_f32(state.resampler.output_delay() as f32 / state.params.sample_rate.0 as f32);
            state.processor.set_playback_delay(output_delay + resampler_delay);

            // pop from channel
            // let len_pre = unprocessed.len();
            // let empty_pre = unprocessed.iter().filter(|x| **x == 0.).count();
            unprocessed.extend(state.consumer.pop_iter());
            // let len_post= unprocessed.len();
            // let empty_post = unprocessed.iter().filter(|x| **x == 0.).count();
            // trace!("data {} len_pre {len_pre} len_post {len_post} empty_pre {empty_pre} empty_post {empty_post}", data.len());
            // process
            let mut chunks = unprocessed.chunks_exact_mut(frame_size);
            for chunk in &mut chunks {
                state.processor.process_render_frame(chunk).unwrap();
                processed.extend_from_slice(chunk);
            }
            // cleanup
            let remainder_len = chunks.into_remainder().len();
            let end = unprocessed.len() - remainder_len;
            unprocessed.copy_within(end.., 0);
            unprocessed.truncate(remainder_len);

            // resample
            state.resampler.process_interleaved(&processed, |samples|{
                resampled.extend_from_slice(samples);
            } , None, false);
            processed.clear();


            // copy to out
            let out_len = resampled.len().min(data.len());
            let resampled_remaining = resampled.len() - out_len;
            for (i, sample) in data[..out_len].iter_mut().enumerate() {
                *sample = resampled[i].to_sample()
            }
            resampled.copy_within(out_len.., 0);
            resampled.truncate(resampled_remaining);

            // trace!("out_len {out_len} resampled_remaining {} processed_remaining {}", resampled.len(), processed.len());
            if out_len < data.len() {
                let now = Instant::now();
                if now.duration_since(last_warning) > Duration::from_secs(1) {
                    warn!(
                        "[tick {tick}] playback xrun: {} of {} samples missing (buffered {}) (+ {} previous)",
                        data.len() - out_len,
                        data.len(),
                        unprocessed.len() + state.consumer.occupied_len(),
                        underflows
                    );
                    underflows += 1;
                    last_warning = now;
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

struct MediaTrackOpusDecoder {
    track: MediaTrack,
    decoder: opus::Decoder,
    audio_buf: Vec<f32>,
    decode_buf: Vec<f32>,
}

impl MediaTrackOpusDecoder {
    pub fn new(track: MediaTrack) -> Self {
        let channels = match track.codec() {
            Codec::Opus { channels } => channels,
        };
        let decoder = opus::Decoder::new(SAMPLE_RATE.0, channels.into()).unwrap();
        let buffer_size = OPUS_STREAM_PARAMS.buffer_size(DURATION_20MS);
        let audio_buf = vec![];
        let decode_buf = vec![0.; buffer_size];
        Self {
            track,
            decoder,
            audio_buf,
            decode_buf,
        }
    }

    /// Should be called in a 20ms interval.
    pub fn tick(&mut self) -> Result<Option<usize>> {
        self.audio_buf.clear();
        loop {
            let (skipped_frames, payload) = match self.track.try_recv() {
                Ok(frame) => {
                    let MediaFrame {
                        payload,
                        skipped_frames,
                        ..
                    } = frame;
                    tracing::trace!("opus decoder: mediatrack recv frame");
                    (skipped_frames, Some(payload))
                }
                Err(broadcast::error::TryRecvError::Empty) => {
                    tracing::trace!("opus decoder: mediatrack recv empty");
                    break;
                }
                Err(broadcast::error::TryRecvError::Lagged(count)) => {
                    tracing::trace!("opus decoder: mediatrack recv lagged {count}");
                    (Some(count as u32), None)
                }
                Err(broadcast::error::TryRecvError::Closed) => {
                    info!("stop opus to audio loop: media track sender dropped");
                    return Ok(None);
                }
            };
            if let Some(skipped_count) = skipped_frames {
                for _ in 0..skipped_count {
                    let count = self
                        .decoder
                        .decode_float(&[], &mut self.decode_buf, false)?;
                    self.audio_buf.extend(&self.decode_buf[..count]);
                    trace!(
                        "decoder: {count} samples from skipped frames, now at {}",
                        self.audio_buf.len()
                    );
                }
            }
            if let Some(payload) = payload {
                let count = self
                    .decoder
                    .decode_float(&payload, &mut self.decode_buf, false)?;
                self.audio_buf.extend(&self.decode_buf[..count]);
                trace!(
                    "decoder: {count} samples from payload, now at {}",
                    self.audio_buf.len()
                );
            }
        }
        Ok(Some(self.audio_buf.len()))
    }

    pub fn samples(&mut self, count: usize) -> &[f32] {
        let len = count.min(self.audio_buf.len());
        &self.audio_buf[..len]
    }
}
