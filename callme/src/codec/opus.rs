use std::{ops::ControlFlow, time::Duration};

use anyhow::{bail, Result};
use bytes::{Bytes, BytesMut};
use ringbuf::{
    traits::{Consumer as _, Observer, Producer as _, Split},
    HeapCons as Consumer, HeapProd as Producer,
};
use tokio::sync::broadcast::{self, error::TryRecvError};
use tracing::{debug, info, trace};

use super::Codec;
use crate::{
    audio::{AudioFormat, AudioSink, AudioSource, SAMPLE_RATE},
    rtc::{MediaFrame, MediaTrack, TrackKind},
};

pub const OPUS_SAMPLE_RATE: u32 = 48_000;
pub const OPUS_STREAM_PARAMS: AudioFormat = AudioFormat::new2(OPUS_SAMPLE_RATE, 2);

const DURATION_20MS: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
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
    underflows: usize,
    remaining_silence_ticks: usize,
    audio_format: AudioFormat,
}

impl MediaTrackOpusDecoder {
    pub fn new(track: MediaTrack) -> Result<Self> {
        let channel_count = match track.codec() {
            Codec::Opus { channels } => channels,
        };
        let audio_format = AudioFormat::new2(OPUS_SAMPLE_RATE, channel_count as u16);
        let decoder =
            opus::Decoder::new(OPUS_STREAM_PARAMS.sample_rate.0, channel_count.into()).unwrap();
        let buffer_size = audio_format.sample_count(DURATION_20MS);
        let decode_buf = vec![0.; buffer_size];
        let audio_buf = vec![];
        Ok(Self {
            track,
            decoder,
            audio_buf,
            decode_buf,
            underflows: 0,
            remaining_silence_ticks: 0,
            audio_format,
        })
    }

    pub fn decode(&mut self, buf: &[u8]) -> Result<usize> {
        let block_count = self
            .decoder
            .decode_float(buf, &mut self.decode_buf, false)?;
        let sample_count = block_count * self.audio_format.channel_count as usize;
        let decoded = &self.decode_buf[..sample_count];
        // we need to upscale to two channels, AudioSource tick always expects stereo.
        match self.audio_format.channel_count {
            1 => self.audio_buf.extend(decoded.iter().flat_map(|s| [s, s])),
            2 => self.audio_buf.extend(decoded),
            _ => unreachable!(),
        }
        Ok(sample_count)
    }

    pub fn peek(&self) -> &[f32] {
        &self.audio_buf
    }

    pub fn advance(&mut self, n: usize) {
        if n > self.audio_buf.len() {
            panic!("requested advance further than buffer length");
        }
        self.audio_buf.copy_within(n.., 0);
        self.audio_buf.truncate(self.audio_buf.len() - n);
    }
}

impl AudioSource for MediaTrackOpusDecoder {
    fn tick(&mut self, buf: &mut [f32]) -> Result<ControlFlow<(), usize>> {
        // decode everything that is ready to recv'd on the track channel.
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
                Err(TryRecvError::Empty) => {
                    trace!("opus decoder: mediatrack recv empty");
                    break;
                }
                Err(TryRecvError::Lagged(count)) => {
                    trace!("opus decoder: mediatrack recv lagged {count}");
                    (Some(count as u32), None)
                }
                Err(TryRecvError::Closed) => {
                    info!("stop opus to audio loop: media track sender dropped");
                    return Ok(ControlFlow::Break(()));
                }
            };
            if let Some(skipped_count) = skipped_frames {
                for _ in 0..skipped_count {
                    let sample_count = self.decode(&[])?;
                    trace!(
                        "decoder: {sample_count} samples from skipped frames, now at {}",
                        self.audio_buf.len()
                    );
                }
            }
            if let Some(payload) = payload {
                let sample_count = self.decode(&payload)?;
                trace!(
                    "decoder: {sample_count} samples from payload, now at {}",
                    self.audio_buf.len()
                );
            }
        }

        // TODO: right now a very hacky way to add some latency if we don't get enough packets.
        if self.remaining_silence_ticks > 0 {
            self.remaining_silence_ticks -= 1;
            return Ok(ControlFlow::Continue(0));
        } else if self.audio_buf.len() < buf.len() {
            self.underflows += 1;
            if self.underflows > 2 {
                self.remaining_silence_ticks = 4;
                tracing::debug!("increase silence");
                self.underflows = 0;
            }
            return Ok(ControlFlow::Continue(0));
        }

        // TODO: a very hacky way to decrease latency if we buffered too much
        if self
            .audio_format
            .duration_from_sample_count(self.audio_buf.len())
            > Duration::from_secs(1)
        {
            self.advance(self.audio_format.sample_count(Duration::from_millis(500)));
        }

        let count = buf.len().min(self.audio_buf.len());
        buf.copy_from_slice(&self.audio_buf[..count]);
        self.advance(count);

        Ok(ControlFlow::Continue(count))
    }
}

pub struct MediaTrackOpusEncoder {
    sender: broadcast::Sender<MediaFrame>,
    encoder: OpusEncoder,
}

impl MediaTrackOpusEncoder {
    pub fn new(track_channel_cap: usize, audio_format: AudioFormat) -> Result<(Self, MediaTrack)> {
        debug_assert_eq!(audio_format.sample_rate.0, OPUS_SAMPLE_RATE);
        let (sender, receiver) = broadcast::channel(track_channel_cap);
        let channels = match audio_format.channel_count {
            1 => OpusChannels::Mono,
            2 => OpusChannels::Stereo,
            _ => bail!("unsupported channel count"),
        };
        let track = MediaTrack::new(receiver, Codec::Opus { channels }, TrackKind::Audio);
        let encoder = MediaTrackOpusEncoder {
            sender,
            encoder: OpusEncoder::new(channels),
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
                    info!("closing encoder loop: track receiver closed.");
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
    pub fn new(channels: OpusChannels) -> Self {
        let format = AudioFormat::new2(OPUS_SAMPLE_RATE, channels as u16);
        let mut encoder =
            opus::Encoder::new(OPUS_SAMPLE_RATE, channels.into(), opus::Application::Voip).unwrap();
        debug!(
            "initialized opus encoder: channels {} bitrate {:?} bandwidth {:?}",
            channels as u16,
            encoder.get_bitrate().unwrap(),
            encoder.get_bandwidth()
        );
        let mut out_buf = BytesMut::new();
        let samples_per_frame = format.sample_count(DURATION_20MS);
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
        let mut iter = samples.iter();
        std::iter::from_fn(move || {
            for sample in iter.by_ref() {
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
