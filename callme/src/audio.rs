use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use cpal::{ChannelCount, SampleRate};

pub mod debug;
mod device;
pub mod play;
mod processor;
pub mod record;

pub use self::device::AudioConfig;
pub use self::play::AudioPlayer;
pub use self::processor::WebrtcAudioProcessor;
pub use self::record::AudioRecorder;
pub use device::{list_input_devices, list_output_devices};

pub const SAMPLE_RATE: SampleRate = SampleRate(48_000);
pub const DURATION_10MS: Duration = Duration::from_millis(10);
pub const DURATION_20MS: Duration = Duration::from_millis(20);

pub const OPUS_STREAM_PARAMS: StreamParams = StreamParams::new(SAMPLE_RATE, 1);

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
    pub const fn new(sample_rate: SampleRate, channel_count: ChannelCount) -> Self {
        Self {
            sample_rate,
            channel_count,
        }
    }

    pub const fn frame_duration_in_samples(&self, duration: Duration) -> usize {
        (self.sample_rate.0 as usize / 1000) * duration.as_millis() as usize
    }

    pub const fn frame_buffer_size(&self, duration: Duration) -> usize {
        self.frame_duration_in_samples(duration) * self.channel_count as usize
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
