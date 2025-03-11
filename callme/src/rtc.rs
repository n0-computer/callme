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

pub use self::{
    protocol_handler::RtcProtocol,
    track::{MediaFrame, MediaTrack, TrackKind},
};
use self::{rtp_receiver::RtpMediaTrackReceiver, rtp_sender::RtpMediaTrackSender};
use crate::audio::AudioContext;

mod protocol_handler;
mod rtp_receiver;
mod rtp_sender;
mod track;

#[derive(Debug, Clone)]
pub struct RtcConnection {
    conn: Connection,
    session: Session,
    next_recv_flow_id: Arc<AtomicU32>,
    next_send_flow_id: Arc<AtomicU32>,
}

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
        let flow_id = self.next_send_flow_id.fetch_add(1, Ordering::SeqCst);
        let send_flow = self.session.new_send_flow(flow_id.into()).await?;
        let sender = RtpMediaTrackSender { send_flow, track };
        task::spawn(async move {
            if let Err(err) = sender.run().await {
                warn!(flow_id, "send flow failed: {err}");
            }
        });
        Ok(())
    }

    pub async fn recv_track(&self) -> Result<Option<MediaTrack>> {
        let flow_id = self.next_recv_flow_id.fetch_add(1, Ordering::SeqCst);
        let recv_flow = self.session.new_receive_flow(flow_id.into()).await?;
        let (track_sender, track_receiver) = broadcast::channel(12);
        let (init_tx, init_rx) = oneshot::channel();
        let receiver = RtpMediaTrackReceiver {
            recv_flow,
            track_sender,
            init_tx: Some(init_tx),
        };
        task::spawn(async move {
            receiver.run().await;
            info!("rtp receiver closed");
        });
        let closed = self.transport().closed();
        let codec = tokio::select! {
            res = init_rx =>  res??,
            err = closed => {
                match err {
                    iroh::endpoint::ConnectionError::LocallyClosed => return Ok(None),
                    err => return Err(err.into())
                }
            }
        };
        let track = MediaTrack {
            receiver: track_receiver,
            codec,
            kind: codec.kind(),
        };
        Ok(Some(track))
    }
}

pub async fn handle_connection_with_audio_context(
    audio_ctx: AudioContext,
    conn: RtcConnection,
) -> Result<()> {
    let capture_track = audio_ctx.capture_track().await?;
    conn.send_track(capture_track).await?;
    info!("added capture track to rtc connection");
    while let Some(remote_track) = conn.recv_track().await? {
        info!(
            "new remote track: {:?} {:?}",
            remote_track.kind(),
            remote_track.codec()
        );
        match remote_track.kind() {
            TrackKind::Audio => {
                audio_ctx.play_track(remote_track).await?;
            }
            TrackKind::Video => unimplemented!(),
        }
    }
    Ok(())
}
