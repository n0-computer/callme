use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::Result;
use n0_future::task::AbortOnDropHandle;
use tokio::sync::broadcast;
use tracing::trace;

use crate::{
    audio::{AudioConfig, AudioPlayer, AudioRecorder, OutboundAudio, WebrtcAudioProcessor},
    rtc::{Codec, MediaFrame, MediaTrack, OpusChannels, TrackKind},
};

#[derive(Debug, Clone)]
pub struct AudioContext {
    capture_sender: broadcast::Sender<MediaFrame>,
    config: Arc<AudioConfig>,
    player: AudioPlayer,
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
        let (capture_sender, _capture_receiver) = broadcast::channel(8);
        tokio::task::spawn({
            let capture_sender = capture_sender.clone();
            async move {
                while let Ok(frame) = recorder.recv().await {
                    let OutboundAudio::Opus {
                        payload,
                        sample_count,
                    } = frame;
                    trace!(
                        "capture sender: sent frame {sample_count} len {}",
                        payload.len()
                    );
                    let frame = MediaFrame {
                        payload,
                        sample_count: Some(sample_count),
                        skipped_frames: None,
                        skipped_samples: None,
                    };
                    if let Err(_err) = capture_sender.send(frame) {
                        trace!("capture sent to black hole: no receivers")
                    }
                }

                anyhow::Ok(())
            }
        });
        let player =
            AudioPlayer::build(&host, config.output_device.as_deref(), processor.clone()).await?;
        Ok(Self {
            config: Arc::new(config),
            capture_sender, // capture_context: Default::default(),
            player,
        })
    }
    pub fn get_track_from_capture(&self) -> Result<MediaTrack> {
        let receiver = self.capture_sender.subscribe();
        let codec = Codec::Opus {
            channels: OpusChannels::Mono,
        };
        let track = MediaTrack::new(receiver, codec, TrackKind::Audio);
        Ok(track)
    }

    pub async fn add_track_to_playback(&self, track: MediaTrack) -> Result<()> {
        self.player.add_track(track).await?;
        Ok(())
    }

    pub async fn feedback_encoded(&self) -> Result<()> {
        let track = self.get_track_from_capture()?;
        self.add_track_to_playback(track).await?;
        Ok(())
    }

    pub fn feedback_raw(&self) -> Result<()> {
        unimplemented!()
    }
    pub fn feedback_processed(&self) -> Result<()> {
        unimplemented!()
    }
}
// }

// #[derive(Debug, Clone)]
// struct CaptureContext {
//     task: Arc<AbortOnDropHandle<Result<()>>>,
// }

// impl CaptureContext {
//     fn new() -> Result<Self> {
//         todo!()
//     }
//     fn subscribe(&self) -> MediaTrack {}
// }

//     let mut guard = self.capture_context.lock().unwrap();
//     let ctx = match guard.as_mut() {
//         None => {
//             let ctx = CaptureContext::new()?;
//             *guard = Some(ctx);
//             guard.as_mut().unwrap()
//         }
//         Some(ctx) => ctx,
//     };
//     Ok(ctx.subscribe())
// }

// pub fn playback(&self, track: MediaTrack) -> Result<()> {
//     todo!()
// }

// fn capture_subscribe(&self) -> broadcast::Receiver<MediaFrame> {}
