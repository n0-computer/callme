use std::{
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use anyhow::Result;
use n0_future::task::AbortOnDropHandle;
use ringbuf_pipe::ringbuf_pipe;
use tokio::sync::broadcast;
use tracing::trace;

use crate::{
    audio::{
        AudioConfig, AudioPlayer, AudioRecorder, OutboundAudio, WebrtcAudioProcessor,
        DURATION_20MS, OPUS_STREAM_PARAMS,
    },
    rtc::{Codec, MediaFrame, MediaTrack, OpusChannels, TrackKind},
};

pub use crate::audio::play::AudioSource;
pub use crate::audio::record::AudioSink;

#[derive(Debug, Clone)]
pub struct AudioContext {
    player: AudioPlayer,
    recorder: AudioRecorder, // config: Arc<AudioConfig>,
}

#[derive(Debug, Clone)]
pub enum LocalAudioSource {
    Microphone,
}

impl AudioContext {
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

    use super::{AudioSink, AudioSource};
    use anyhow::Result;
    use ringbuf::traits::{Consumer as _, Observer, Producer as _, Split};
    use ringbuf::{HeapCons as Consumer, HeapProd as Producer};
    use tracing::warn;

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
