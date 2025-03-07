use std::{ops::ControlFlow, time::Duration};

use anyhow::{bail, Result};
use bytes::{Bytes, BytesMut};
use ringbuf::traits::{Consumer as _, Observer, Producer as _, Split};
use ringbuf::{HeapCons as Consumer, HeapProd as Producer};
use tokio::sync::broadcast;
use tracing::{info, trace};

use crate::{
    audio::{AudioSink, AudioSource, StreamParams, SAMPLE_RATE},
    rtc::{MediaFrame, MediaTrack, TrackKind},
};

use super::Codec;

pub const OPUS_STREAM_PARAMS: StreamParams = StreamParams::new(SAMPLE_RATE, 1);

const DURATION_20MS: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, Copy)]
pub enum OpusChannels {
    Mono = 1,
    Stereo = 2,
}

impl From<OpusChannels> for ::opus::Channels {
    fn from(value: OpusChannels) -> Self {
        match value {
            OpusChannels::Mono => ::opus::Channels::Mono,
            OpusChannels::Stereo => ::opus::Channels::Stereo,
        }
    }
}

pub struct MediaTrackOpusDecoder {
    track: MediaTrack,
    decoder: opus::Decoder,
    audio_buf: Vec<f32>,
    decode_buf: Vec<f32>,
    // channels: OpusChannels,
}

impl MediaTrackOpusDecoder {
    pub fn new(track: MediaTrack) -> Result<Self> {
        let channels = match track.codec() {
            Codec::Opus { channels } => channels,
            // _ => bail!("track is not an opus track"),
        };
        let decoder =
            opus::Decoder::new(OPUS_STREAM_PARAMS.sample_rate.0, channels.into()).unwrap();
        let buffer_size = OPUS_STREAM_PARAMS.frame_buffer_size(DURATION_20MS);
        let decode_buf = vec![0.; buffer_size];
        let audio_buf = vec![];
        Ok(Self {
            track,
            decoder,
            audio_buf,
            decode_buf,
            // channels,
        })
    }
}

impl AudioSource for MediaTrackOpusDecoder {
    /// Should be called in a 20ms interval with a buf of len 960 * channel_count.
    fn tick(&mut self, buf: &mut [f32]) -> Result<ControlFlow<(), usize>> {
        // debug_assert!(buf.len() as u32 >= Self::FRAME_SAMPLE_COUNT + self.channels as u32, "buffer too small");
        self.audio_buf.clear();
        loop {
            let (skipped_frames, payload) = match self.track.try_recv() {
                Ok(frame) => {
                    let MediaFrame {
                        payload,
                        skipped_frames,
                        ..
                    } = frame;
                    trace!("opus decoder: mediatrack recv frame");
                    (skipped_frames, Some(payload))
                }
                Err(broadcast::error::TryRecvError::Empty) => {
                    trace!("opus decoder: mediatrack recv empty");
                    break;
                }
                Err(broadcast::error::TryRecvError::Lagged(count)) => {
                    trace!("opus decoder: mediatrack recv lagged {count}");
                    (Some(count as u32), None)
                }
                Err(broadcast::error::TryRecvError::Closed) => {
                    info!("stop opus to audio loop: media track sender dropped");
                    return Ok(ControlFlow::Break(()));
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
        let len = buf.len().min(self.audio_buf.len());
        buf[..len].copy_from_slice(&self.audio_buf[..len]);
        let end = self.audio_buf.len() - len;
        self.audio_buf.copy_within(end.., 0);
        self.audio_buf.truncate(self.audio_buf.len() - len);

        // when falling out of sync too much, warn and drop old frames?

        Ok(ControlFlow::Continue(len))
    }
}

pub struct MediaTrackOpusEncoder {
    sender: broadcast::Sender<MediaFrame>,
    encoder: OpusEncoder,
}

impl MediaTrackOpusEncoder {
    pub fn new(channel_frame_cap: usize, params: StreamParams) -> Result<(Self, MediaTrack)> {
        let (sender, receiver) = broadcast::channel(channel_frame_cap);
        let channels = match params.channel_count {
            1 => OpusChannels::Mono,
            2 => OpusChannels::Stereo,
            _ => bail!("unsupported channel count"),
        };
        let track = MediaTrack::new(receiver, Codec::Opus { channels }, TrackKind::Audio);
        let encoder = MediaTrackOpusEncoder {
            sender,
            encoder: OpusEncoder::new(params),
        };
        Ok((encoder, track))
    }
}

impl AudioSink for MediaTrackOpusEncoder {
    fn tick(&mut self, buf: &[f32]) -> Result<ControlFlow<(), ()>> {
        for (payload, sample_count) in self.encoder.push_slice(buf) {
            let payload_len = payload.len();
            let frame = MediaFrame {
                payload,
                sample_count: Some(sample_count),
                skipped_frames: None,
                skipped_samples: None,
            };
            match self.sender.send(frame) {
                Err(_) => {
                    tracing::info!("closing encoder loop: track receiver closed.");
                    return Ok(ControlFlow::Break(()));
                }
                Ok(_) => {
                    trace!("sent opus {sample_count}S {payload_len}B")
                }
            }
        }
        Ok(ControlFlow::Continue(()))
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
        let samples_per_frame = params.frame_buffer_size(DURATION_20MS);
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

    pub fn pop_from_consumer<'a>(
        &'a mut self,
        consumer: &'a mut Consumer<f32>,
    ) -> impl Iterator<Item = (Bytes, u32)> + 'a {
        std::iter::from_fn(|| {
            for sample in consumer.pop_iter() {
                if let Some((payload, sample_count)) = self.push_sample(sample) {
                    return Some((payload, sample_count));
                }
            }
            None
        })
    }

    pub fn push_slice<'a>(
        &'a mut self,
        samples: &'a [f32],
    ) -> impl Iterator<Item = (Bytes, u32)> + 'a {
        let mut iter = samples.into_iter();
        std::iter::from_fn(move || {
            while let Some(sample) = iter.next() {
                if let Some((payload, sample_count)) = self.push_sample(*sample) {
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
