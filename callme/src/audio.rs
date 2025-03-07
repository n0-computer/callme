use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use cpal::{ChannelCount, SampleRate};
use device::Devices;

use self::{
    device::list_devices, playback::AudioPlayer, record::AudioRecorder, ringbuf_pipe::ringbuf_pipe,
};
pub use self::{
    device::AudioConfig, playback::AudioSource, processor::WebrtcAudioProcessor, record::AudioSink,
};
use crate::rtc::MediaTrack;

mod device;
mod playback;
mod processor;
mod record;

pub const SAMPLE_RATE: SampleRate = SampleRate(48_000);
const DURATION_10MS: Duration = Duration::from_millis(10);
const DURATION_20MS: Duration = Duration::from_millis(20);

pub use crate::codec::opus::OPUS_STREAM_PARAMS;

#[derive(Debug, Clone)]
pub struct AudioContext {
    player: AudioPlayer,
    recorder: AudioRecorder, // config: Arc<AudioConfig>,
}

impl AudioContext {
    pub async fn list_devices() -> Result<Devices> {
        tokio::task::spawn_blocking(|| list_devices()).await?
    }

    pub async fn new(config: AudioConfig) -> Result<Self> {
        let host = cpal::default_host();
        let processor = WebrtcAudioProcessor::new(1, 1, None, config.processing_enabled)?;
        let recorder =
            AudioRecorder::build(&host, config.input_device.as_deref(), processor.clone()).await?;
        let player =
            AudioPlayer::build(&host, config.output_device.as_deref(), processor.clone()).await?;
        Ok(Self { player, recorder })
    }

    pub async fn capture_track(&self) -> Result<MediaTrack> {
        self.recorder.create_opus_track().await
    }

    pub async fn play_track(&self, track: MediaTrack) -> Result<()> {
        self.player.add_track(track).await?;
        Ok(())
    }

    pub async fn feedback_encoded(&self) -> Result<()> {
        let track = self.capture_track().await?;
        self.play_track(track).await?;
        Ok(())
    }

    pub async fn feedback_raw(&self) -> Result<()> {
        let buffer_size = OPUS_STREAM_PARAMS.frame_buffer_size(DURATION_20MS * 4);
        let (sink, source) = ringbuf_pipe(buffer_size);
        self.recorder.add_sink(sink).await?;
        self.player.add_source(source).await?;
        Ok(())
    }
}

mod ringbuf_pipe {
    use std::ops::ControlFlow;

    use anyhow::Result;
    use ringbuf::{
        traits::{Consumer as _, Observer, Producer as _, Split},
        HeapCons as Consumer, HeapProd as Producer,
    };
    use tracing::warn;

    use super::{AudioSink, AudioSource};

    pub struct RingbufSink(Producer<f32>);
    pub struct RingbufSource(Consumer<f32>);

    pub fn ringbuf_pipe(buffer_size: usize) -> (RingbufSink, RingbufSource) {
        let (producer, consumer) = ringbuf::HeapRb::<f32>::new(buffer_size).split();
        (RingbufSink(producer), RingbufSource(consumer))
    }

    impl AudioSink for RingbufSink {
        fn tick(&mut self, buf: &[f32]) -> Result<ControlFlow<(), ()>> {
            let len = self.0.push_slice(&buf);
            if len < buf.len() {
                warn!("ringbuf sink xrun: failed to send {}", buf.len() - len);
            }
            Ok(ControlFlow::Continue(()))
        }
    }

    impl AudioSource for RingbufSource {
        fn tick(&mut self, buf: &mut [f32]) -> Result<ControlFlow<(), usize>> {
            let len = self.0.pop_slice(buf);
            if len < buf.len() {
                warn!("ringbuf source xrun: failed to recv {}", buf.len() - len);
            }
            Ok(ControlFlow::Continue(len))
        }
    }
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
