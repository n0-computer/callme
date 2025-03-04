use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use cpal::{ChannelCount, SampleRate};
use device::{list_input_devices, list_output_devices};
use processor::Processor;

pub mod debug;
mod device;
mod play;
mod processor;
mod record;

pub use self::device::AudioConfig;
pub use self::play::AudioPlayer;
pub use self::record::AudioRecorder;

pub const SAMPLE_RATE: SampleRate = SampleRate(48_000);
const DURATION_10MS: Duration = Duration::from_millis(10);
const DURATION_20MS: Duration = Duration::from_millis(20);

pub const OPUS_STREAM_PARAMS: StreamParams = StreamParams::new(SAMPLE_RATE, 1);

#[derive(Debug)]
pub struct AudioStreams {
    pub recorder: AudioRecorder,
    pub player: AudioPlayer,
}

// pub type OutboundAudioStream = async_channel::Receiver<OutboundAudio>;
// pub type InboundAudioStream = async_channel::Sender<InboundAudio>;

#[derive(Debug)]
pub struct AudioState;

pub fn start_audio(config: AudioConfig) -> Result<(AudioStreams, AudioState)> {
    let host = cpal::default_host();
    let processor = Processor::new(1, 1, None)?;
    let player = AudioPlayer::build(&host, config.output_device.as_deref(), processor.clone())?;
    let recorder = AudioRecorder::build(&host, config.input_device.as_deref(), processor)?;
    let streams = AudioStreams { recorder, player };
    let state = AudioState;
    Ok((streams, state))
}

pub fn list_devices() -> Result<Devices> {
    let host = cpal::default_host();
    let input = list_input_devices(&host)?;
    let output = list_output_devices(&host)?;
    Ok(Devices { input, output })
}

#[derive(Debug)]
pub struct Devices {
    pub input: Vec<String>,
    pub output: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct StreamParams {
    pub sample_rate: SampleRate,
    pub channel_count: ChannelCount,
}

impl From<cpal::StreamConfig> for StreamParams {
    fn from(value: cpal::StreamConfig) -> Self {
        Self {
            sample_rate: value.sample_rate,
            channel_count: value.channels,
        }
    }
}

impl StreamParams {
    const fn new(sample_rate: SampleRate, channel_count: ChannelCount) -> Self {
        Self {
            sample_rate,
            channel_count,
        }
    }

    const fn frame_length(&self, duration: Duration) -> usize {
        (self.sample_rate.0 as usize / 1000) * duration.as_millis() as usize
    }

    const fn buffer_size(&self, duration: Duration) -> usize {
        self.frame_length(duration) * self.channel_count as usize
    }
}

pub enum OutboundAudio {
    Opus { payload: Bytes, sample_count: u32 },
}

pub enum InboundAudio {
    Opus {
        payload: Bytes,
        // skipped_samples: Option<u32>,
        skipped_frames: Option<u32>,
    },
}
