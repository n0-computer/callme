use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::Result;
use n0_future::task::AbortOnDropHandle;
use tokio::sync::broadcast;

use crate::{
    audio::{AudioConfig, AudioPlayer, AudioRecorder, OutboundAudio, Processor},
    rtc::{Codec, MediaFrame, MediaTrack, OpusChannels, TrackKind},
};

#[derive(Debug, Clone)]
pub struct AudioContext {
    capture_sender: broadcast::Sender<MediaFrame>,
    processor: Processor,
    config: Arc<AudioConfig>,
}

#[derive(Debug, Clone)]
pub enum LocalAudioSource {
    Microphone,
}

impl AudioContext {
    pub async fn new(config: AudioConfig) -> Result<Self> {
        let host = cpal::default_host();
        let processor = Processor::new(1, 1, None)?;
        let recorder =
            AudioRecorder::build(&host, config.input_device.as_deref(), processor.clone())?;
        let (capture_sender, _capture_receiver) = broadcast::channel(8);
        tokio::task::spawn({
            let capture_sender = capture_sender.clone();
            async move {
                while let Ok(frame) = recorder.recv().await {
                    let OutboundAudio::Opus {
                        payload,
                        sample_count,
                    } = frame;
                    let frame = MediaFrame {
                        payload,
                        sample_count: Some(sample_count),
                        skipped_frames: None,
                        skipped_samples: None,
                    };
                    capture_sender.send(frame).ok();
                }

                anyhow::Ok(())
            }
        });
        Ok(Self {
            config: Arc::new(config),
            processor,
            capture_sender, // capture_context: Default::default(),
        })
    }
    pub fn capture(&self) -> Result<MediaTrack> {
        let receiver = self.capture_sender.subscribe();
        let codec = Codec::Opus {
            channels: OpusChannels::Mono,
        };
        let track = MediaTrack::new(receiver, codec, TrackKind::Audio);
        Ok(track)
    }

    pub fn playback(&self, mut track: MediaTrack) -> Result<()> {
        let host = cpal::default_host();
        let player = AudioPlayer::build(
            &host,
            self.config.output_device.as_deref(),
            self.processor.clone(),
        )?;
        tokio::task::spawn(async move {
            while let Ok(frame) = track.recv().await {
                let frame = crate::audio::InboundAudio::Opus {
                    payload: frame.payload,
                    skipped_frames: frame.skipped_frames,
                };
                player.send(frame).await?;
            }
            anyhow::Ok(())
        });
        Ok(())
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
