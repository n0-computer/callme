use std::{
    collections::HashMap,
    future::Future,
    sync::{
        atomic::{AtomicU32, AtomicU64, Ordering},
        Arc,
    },
};

use anyhow::{anyhow, Result};
use bytes::Bytes;
use iroh::{endpoint::Connection, protocol::ProtocolHandler};
use iroh_roq::{
    rtp::{self, codecs::opus::OpusPayloader, packetizer::Packetizer},
    Session, VarInt,
};
use n0_future::{task, Stream};
use tokio::sync::{broadcast, oneshot};
use tracing::{info, warn};
use webrtc_media::io::sample_builder::SampleBuilder;

use crate::audio2::AudioContext;

pub use self::protocol_handler::RtcProtocol;
use self::rtp_receiver::RtpReceiver;
use self::rtp_sender::RtpSender;

mod protocol_handler;
mod rtp_receiver;
mod rtp_sender;

#[derive(Debug)]
pub struct MediaTrack {
    pub(crate) receiver: broadcast::Receiver<MediaFrame>,
    codec: Codec,
    kind: TrackKind,
}

impl Clone for MediaTrack {
    fn clone(&self) -> Self {
        Self {
            receiver: self.receiver.resubscribe(),
            codec: self.codec,
            kind: self.kind,
        }
    }
}

impl MediaTrack {
    pub fn new(receiver: broadcast::Receiver<MediaFrame>, codec: Codec, kind: TrackKind) -> Self {
        Self {
            receiver,
            codec,
            kind,
        }
    }
    pub async fn recv(&mut self) -> Result<MediaFrame, broadcast::error::RecvError> {
        self.receiver.recv().await
    }

    pub fn try_recv(&mut self) -> Result<MediaFrame, broadcast::error::TryRecvError> {
        self.receiver.try_recv()
    }

    pub fn kind(&self) -> TrackKind {
        self.kind
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }
}

#[derive(Debug, Clone)]
pub struct MediaFrame {
    pub payload: Bytes,
    pub sample_count: Option<u32>,
    pub skipped_frames: Option<u32>,
    pub skipped_samples: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct RtcConnection {
    conn: Connection,
    session: Session,
    next_recv_flow_id: NextId,
    next_send_flow_id: NextId,
}

// #[derive(Debug, Clone)]
// pub struct TrackHandle {
//     closed_rx: oneshot::Receiver<Result<()>>,
// }

impl RtcConnection {
    pub fn new(conn: Connection) -> Self {
        let session = Session::new(conn.clone());
        Self {
            conn,
            session,
            next_recv_flow_id: Default::default(),
            next_send_flow_id: Default::default(),
        }
    }

    pub fn transport(&self) -> &Connection {
        &self.conn
    }

    pub async fn send_track(&self, track: MediaTrack) -> Result<()> {
        let flow_id = self.next_send_flow_id.next();
        let send_flow = self.session.new_send_flow(flow_id.into()).await?;
        let sender = RtpSender { send_flow, track };
        task::spawn(async move {
            if let Err(err) = sender.run().await {
                warn!(flow_id, "send flow failed: {err:?}");
            }
        });
        Ok(())
    }

    pub async fn recv_track(&self) -> Result<MediaTrack> {
        let flow_id = self.next_recv_flow_id.next();
        let recv_flow = self.session.new_receive_flow(flow_id.into()).await?;
        let (track_sender, track_receiver) = broadcast::channel(4);
        let (init_tx, init_rx) = oneshot::channel();
        let receiver = RtpReceiver {
            recv_flow,
            track_sender,
            init_tx: Some(init_tx),
        };
        task::spawn(async move {
            receiver.run().await;
        });
        let codec = init_rx.await??;
        let track = MediaTrack {
            receiver: track_receiver,
            codec,
            kind: codec.kind(),
        };
        Ok(track)
    }
}

#[derive(Debug, Clone, Default)]
struct NextId(Arc<AtomicU32>);

impl NextId {
    fn next(&self) -> u32 {
        self.0.fetch_add(1, Ordering::SeqCst)
    }
}

#[derive(Debug, Clone, Copy)]
pub enum TrackKind {
    Audio,
    Video,
}

#[derive(Debug, Clone, Copy)]
pub enum OpusChannels {
    Mono = 1,
    Stereo = 2,
}

impl From<OpusChannels> for opus::Channels {
    fn from(value: OpusChannels) -> Self {
        match value {
            OpusChannels::Mono => opus::Channels::Mono,
            OpusChannels::Stereo => opus::Channels::Stereo,
        }
    }
}

#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum Codec {
    Opus { channels: OpusChannels },
}

impl Codec {
    /// We use the "dynamic" identifiers 96-127 in a "static" way here
    /// to skip SDP.
    ///
    /// See https://en.wikipedia.org/wiki/RTP_payload_formats
    pub fn rtp_payload_type(&self) -> u8 {
        match self {
            Codec::Opus {
                channels: OpusChannels::Mono,
            } => 96,
            Codec::Opus {
                channels: OpusChannels::Stereo,
            } => 97,
        }
    }

    pub fn try_from_rtp_payload_type(payload_type: u8) -> Option<Self> {
        match payload_type {
            96 => Some(Codec::Opus {
                channels: OpusChannels::Mono,
            }),
            97 => Some(Codec::Opus {
                channels: OpusChannels::Stereo,
            }),
            _ => None,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        48_000
    }

    pub fn kind(&self) -> TrackKind {
        match self {
            Codec::Opus { .. } => TrackKind::Audio,
        }
    }
}

pub async fn handle_connection(audio_ctx: AudioContext, conn: RtcConnection) -> Result<()> {
    let capture_track = audio_ctx.get_track_from_capture()?;
    conn.send_track(capture_track).await?;
    info!("added capture track to rtc connection");
    loop {
        let remote_track = conn.recv_track().await?;
        info!(
            "new remote track: {:?} {:?}",
            remote_track.kind(),
            remote_track.codec()
        );
        match remote_track.kind() {
            TrackKind::Audio => {
                audio_ctx.add_track_to_playback(remote_track).await?;
            }
            TrackKind::Video => unimplemented!(),
        }
    }
}
