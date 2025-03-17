use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use anyhow::Result;
use dasp_sample::ToSample;
use tracing::{debug, info};
use webrtc_audio_processing::{
    Config, EchoCancellation, EchoCancellationSuppressionLevel, InitializationConfig,
    NoiseSuppression, NoiseSuppressionLevel,
};

#[derive(Clone, Debug)]
pub struct WebrtcAudioProcessor(Arc<Inner>);

#[derive(derive_more::Debug)]
struct Inner {
    #[debug("Processor")]
    inner: Mutex<Option<webrtc_audio_processing::Processor>>,
    config: Mutex<Config>,
    capture_delay: AtomicU64,
    playback_delay: AtomicU64,
    enabled: AtomicBool,
    capture_channels: AtomicUsize,
    playback_channels: AtomicUsize,
}

impl WebrtcAudioProcessor {
    pub fn new(enabled: bool) -> Result<Self> {
        let suppression_level = EchoCancellationSuppressionLevel::Moderate;
        // High pass filter is a prerequisite to running echo cancellation.
        let config = Config {
            echo_cancellation: Some(EchoCancellation {
                suppression_level,
                // stream_delay_ms: Some(20),
                stream_delay_ms: None,
                enable_delay_agnostic: true,
                enable_extended_filter: true,
            }),
            enable_high_pass_filter: true,
            // noise_suppression: Some(NoiseSuppression {
            //     suppression_level: NoiseSuppressionLevel::High,
            // }),
            ..Config::default()
        };
        // processor.set_config(config.clone());
        info!("init audio processor (enabled={enabled})");
        Ok(Self(Arc::new(Inner {
            inner: Mutex::new(None),
            config: Mutex::new(config),
            capture_delay: Default::default(),
            playback_delay: Default::default(),
            enabled: AtomicBool::new(enabled),
            capture_channels: Default::default(),
            playback_channels: Default::default(),
        })))
    }

    pub fn is_enabled(&self) -> bool {
        self.0.enabled.load(Ordering::SeqCst)
    }

    pub fn set_enabled(&self, enabled: bool) {
        let _prev = self.0.enabled.swap(enabled, Ordering::SeqCst);
    }

    pub fn init_capture(&self, channels: usize) -> Result<()> {
        self.0.capture_channels.store(channels, Ordering::SeqCst);
        if self.0.playback_channels.load(Ordering::SeqCst) > 0 {
            self.init()?;
        }
        Ok(())
    }

    pub fn init_playback(&self, channels: usize) -> Result<()> {
        self.0.playback_channels.store(channels, Ordering::SeqCst);
        if self.0.capture_channels.load(Ordering::SeqCst) > 0 {
            self.init()?;
        }
        Ok(())
    }

    fn init(&self) -> Result<()> {
        let playback_channels = self.0.playback_channels.load(Ordering::SeqCst);
        let capture_channels = self.0.playback_channels.load(Ordering::SeqCst);
        let mut processor = webrtc_audio_processing::Processor::new(&InitializationConfig {
            num_capture_channels: capture_channels as i32,
            num_render_channels: playback_channels as i32,
            ..InitializationConfig::default()
        })?;
        processor.set_config(self.0.config.lock().unwrap().clone());
        *self.0.inner.lock().unwrap() = Some(processor);
        Ok(())
    }

    /// Processes and modifies the audio frame from a capture device by applying
    /// signal processing as specified in the config. `frame` should hold an
    /// interleaved f32 audio frame, with [`NUM_SAMPLES_PER_FRAME`] samples.
    // webrtc-audio-processing expects a 10ms chunk for each process call.
    pub fn process_capture_frame(
        &self,
        frame: &mut [f32],
    ) -> Result<(), webrtc_audio_processing::Error> {
        if !self.is_enabled() {
            return Ok(());
        }
        if let Some(processor) = self.0.inner.lock().unwrap().as_mut() {
            processor.process_capture_frame(frame)
        } else {
            Ok(())
        }
    }
    /// Processes and optionally modifies the audio frame from a playback device.
    /// `frame` should hold an interleaved `f32` audio frame, with
    /// [`NUM_SAMPLES_PER_FRAME`] samples.
    pub fn process_render_frame(
        &self,
        frame: &mut [f32],
    ) -> Result<(), webrtc_audio_processing::Error> {
        if !self.is_enabled() {
            return Ok(());
        }
        if let Some(processor) = self.0.inner.lock().unwrap().as_mut() {
            processor.process_render_frame(frame)
        } else {
            Ok(())
        }
    }

    pub fn set_capture_delay(&self, stream_delay: Duration) {
        let new_val = stream_delay.as_millis() as u64;
        if let Ok(old_val) =
            self.0
                .capture_delay
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |val| {
                    if new_val.abs_diff(val) > 1 {
                        Some(new_val)
                    } else {
                        None
                    }
                })
        {
            debug!("changing capture delay from {old_val} to {new_val}");
            self.update_stream_delay();
        }
    }

    pub fn set_playback_delay(&self, stream_delay: Duration) {
        let new_val = stream_delay.as_millis() as u64;
        if let Ok(old_val) =
            self.0
                .playback_delay
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |val| {
                    if new_val.abs_diff(val) > 1 {
                        Some(new_val)
                    } else {
                        None
                    }
                })
        {
            debug!("changing playback delay from {old_val} to {new_val}");
            self.update_stream_delay();
        }
    }

    fn update_stream_delay(&self) {
        let playback = self.0.playback_delay.load(Ordering::Relaxed);
        let capture = self.0.capture_delay.load(Ordering::Relaxed);
        let total = playback + capture;
        let mut config = self.0.config.lock().unwrap();
        config.echo_cancellation.as_mut().unwrap().stream_delay_ms = Some(total as i32);
        if let Some(processor) = self.0.inner.lock().unwrap().as_mut() {
            processor.set_config(config.clone());
        }
    }
}
