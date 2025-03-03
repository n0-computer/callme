use std::sync::{Arc, Mutex};

use anyhow::Result;
use webrtc_audio_processing::{
    Config, EchoCancellation, EchoCancellationSuppressionLevel, InitializationConfig,
};

// pub use webrtc_audio_processing::NUM_SAMPLES_PER_FRAME;

#[derive(Clone)]
pub struct Processor(Arc<Mutex<webrtc_audio_processing::Processor>>);

impl Processor {
    pub fn new(
        num_capture_channels: i32,
        num_render_channels: i32,
        echo_cancellation_suppression_level: Option<EchoCancellationSuppressionLevel>,
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
                stream_delay_ms: Some(0),
                enable_delay_agnostic: true,
                enable_extended_filter: true,
            }),
            enable_high_pass_filter: true,
            ..Config::default()
        };
        processor.set_config(config);
        Ok(Self(Arc::new(Mutex::new(processor))))
    }

    /// Processes and modifies the audio frame from a capture device by applying
    /// signal processing as specified in the config. `frame` should hold an
    /// interleaved f32 audio frame, with [`NUM_SAMPLES_PER_FRAME`] samples.
    // webrtc-audio-processing expects a 10ms chunk for each process call.
    pub fn process_capture_frame(
        &self,
        frame: &mut [f32],
    ) -> Result<(), webrtc_audio_processing::Error> {
        self.0.lock().unwrap().process_capture_frame(frame)
    }
    /// Processes and optionally modifies the audio frame from a playback device.
    /// `frame` should hold an interleaved `f32` audio frame, with
    /// [`NUM_SAMPLES_PER_FRAME`] samples.
    pub fn process_render_frame(
        &self,
        frame: &mut [f32],
    ) -> Result<(), webrtc_audio_processing::Error> {
        self.0.lock().unwrap().process_render_frame(frame)
    }
}
