use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use anyhow::Result;
use dasp_sample::ToSample;
use webrtc_audio_processing::{
    Config, EchoCancellation, EchoCancellationSuppressionLevel, InitializationConfig,
    NoiseSuppression, NoiseSuppressionLevel,
};

// pub use webrtc_audio_processing::NUM_SAMPLES_PER_FRAME;

#[derive(Clone, Debug)]
pub struct WebrtcAudioProcessor(Arc<Inner>);

#[derive(derive_more::Debug)]
struct Inner {
    #[debug("Processor")]
    inner: Mutex<webrtc_audio_processing::Processor>,
    config: Mutex<Config>,
    capture_delay: AtomicU64,
    playback_delay: AtomicU64,
    enabled: AtomicBool,
}

impl WebrtcAudioProcessor {
    pub fn new(
        num_capture_channels: i32,
        num_render_channels: i32,
        echo_cancellation_suppression_level: Option<EchoCancellationSuppressionLevel>,
        enabled: bool,
    ) -> Result<Self> {
        let mut processor = webrtc_audio_processing::Processor::new(&InitializationConfig {
            num_capture_channels,
            num_render_channels,
            ..InitializationConfig::default()
        })?;

        let suppression_level = echo_cancellation_suppression_level
            .unwrap_or(EchoCancellationSuppressionLevel::Moderate);
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
        processor.set_config(config.clone());
        tracing::info!("init audio processor (enabled={enabled})");
        Ok(Self(Arc::new(Inner {
            inner: Mutex::new(processor),
            config: Mutex::new(config),
            capture_delay: Default::default(),
            playback_delay: Default::default(),
            enabled: AtomicBool::new(enabled),
        })))
    }

    pub fn is_enabled(&self) -> bool {
        self.0.enabled.load(Ordering::SeqCst)
    }

    pub fn set_enabled(&self, enabled: bool) {
        let _prev = self.0.enabled.swap(enabled, Ordering::SeqCst);
        // if !prev && enabled {
        //     self.0.inner.lock().unwrap().
        // }
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
        self.0.inner.lock().unwrap().process_capture_frame(frame)
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
        self.0.inner.lock().unwrap().process_render_frame(frame)
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
            tracing::info!("changing capture delay from {old_val} to {new_val}");
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
            tracing::info!("changing playback delay from {old_val} to {new_val}");
            self.update_stream_delay();
        }
    }

    fn update_stream_delay(&self) {
        let playback = self.0.playback_delay.load(Ordering::Relaxed);
        let capture = self.0.capture_delay.load(Ordering::Relaxed);
        let total = playback + capture;
        let mut config = self.0.config.lock().unwrap();
        let mut inner = self.0.inner.lock().unwrap();
        config.echo_cancellation.as_mut().unwrap().stream_delay_ms = Some(total as i32);
        inner.set_config(config.clone());
    }
}

// trait InputProcessor {
//     fn process_input(&mut self, input: &[f32]) -> Option<&[f32]>;
// }

// trait OutputProcessor {
//     fn process_output(&mut self, samples: &[f32], out: &mut [f32]);
// }

// struct WebrtcInputProcessor {
//     processor: WebrtcAudioProcessor,
//     channel_count: usize,
//     buf: Vec<f32>,
//     clear: bool
// }

// impl InputProcessor for WebrtcInputProcessor {
//     fn process_input<S: ToSample<f32>(&mut self, input: &[S]) -> Option<&[f32]> {
//         if clear {
//             input_buf.clear()
//         }
//         for s in input {
//             self.buf.push(s.to_sample());
//             if self.input_buf.len() == frame_size {
//                 self
//                     .processor
//                     .process_capture_frame(&mut input_buf[..])
//                     .unwrap();
//                 let n = state.producer.push_slice(&input_buf);
//                 if n < data.len() {
//                     warn!(
//                         "record overflow: failed to push {} of {}",
//                         data.len() - n,
//                         data.len()
//                     );
//                 }
//                 input_buf.clear();
//             }
//         }
//     }
// }
