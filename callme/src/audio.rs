use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use cpal::{ChannelCount, SampleRate};

use self::{
    capture::AudioCapture, device::list_devices, playback::AudioPlayback,
    ringbuf_pipe::ringbuf_pipe,
};
pub use self::{
    capture::AudioSink,
    device::{AudioConfig, Devices, Direction},
    playback::AudioSource,
};
use crate::rtc::MediaTrack;

#[cfg(feature = "audio-processing")]
mod processor;
#[cfg(feature = "audio-processing")]
pub use processor::WebrtcAudioProcessor;

#[cfg(not(feature = "audio-processing"))]
#[derive(Debug, Clone)]
pub struct WebrtcAudioProcessor;

mod capture;
mod device;
mod playback;

pub const SAMPLE_RATE: SampleRate = SampleRate(48_000);
pub const ENGINE_FORMAT: AudioFormat = AudioFormat::new(SAMPLE_RATE, 2);

const DURATION_10MS: Duration = Duration::from_millis(10);
const DURATION_20MS: Duration = Duration::from_millis(20);

#[derive(Debug, Clone)]
pub struct AudioContext {
    playback: AudioPlayback,
    capture: AudioCapture,
}

impl AudioContext {
    pub async fn list_devices() -> Result<Devices> {
        tokio::task::spawn_blocking(list_devices).await?
    }

    pub fn list_devices_sync() -> Result<Devices> {
        list_devices()
    }

    /// Create a new [`AudioContext`].
    pub async fn new(config: AudioConfig) -> Result<Self> {
        let host = cpal::default_host();

        #[cfg(feature = "audio-processing")]
        let processor = WebrtcAudioProcessor::new(config.processing_enabled)?;
        #[cfg(not(feature = "audio-processing"))]
        let processor = WebrtcAudioProcessor;

        let capture =
            AudioCapture::build(&host, config.input_device.as_deref(), processor.clone()).await?;
        let playback =
            AudioPlayback::build(&host, config.output_device.as_deref(), processor.clone()).await?;
        Ok(Self { playback, capture })
    }

    pub async fn capture_track(&self) -> Result<MediaTrack> {
        self.capture.create_opus_track().await
    }

    pub async fn play_track(&self, track: MediaTrack) -> Result<()> {
        self.playback.add_track(track).await?;
        Ok(())
    }

    pub async fn feedback_encoded(&self) -> Result<()> {
        let track = self.capture_track().await?;
        self.play_track(track).await?;
        Ok(())
    }

    pub async fn feedback_raw(&self) -> Result<()> {
        let buffer_size = ENGINE_FORMAT.sample_count(DURATION_20MS * 16);
        let (sink, source) = ringbuf_pipe(buffer_size);
        self.capture.add_sink(sink).await?;
        self.playback.add_source(source).await?;
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
            let len = self.0.push_slice(buf);
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
pub struct AudioFormat {
    pub sample_rate: SampleRate,
    pub channel_count: ChannelCount,
}

impl AudioFormat {
    pub const fn new(sample_rate: SampleRate, channel_count: ChannelCount) -> Self {
        Self {
            sample_rate,
            channel_count,
        }
    }
    pub const fn new2(sample_rate: u32, channel_count: u16) -> Self {
        Self {
            sample_rate: SampleRate(sample_rate),
            channel_count,
        }
    }

    pub fn duration_from_sample_count(&self, sample_count: usize) -> Duration {
        Duration::from_secs_f32(
            (sample_count as f32 / self.channel_count as f32) / self.sample_rate.0 as f32,
        )
    }

    pub const fn block_count(&self, duration: Duration) -> usize {
        (self.sample_rate.0 as usize / 1000) * duration.as_millis() as usize
    }

    pub const fn sample_count(&self, duration: Duration) -> usize {
        self.block_count(duration) * self.channel_count as usize
    }
}
