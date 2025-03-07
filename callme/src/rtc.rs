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

impl OpusChannels {
    // fn from_channel_count(count: usize) -> Option<Self> {
    //     match count {
    //         1 => Some(Self::Mono),
    //         2 => Some(Self::Stereo),
    //         _ => None,
    //     }
    // }
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

pub async fn handle_connection(audio_ctx: AudioContext, conn: Connection) -> Result<()> {
    let conn = RtcConnection::new(conn);
    let mic_track = audio_ctx.capture()?;
    conn.send_track(mic_track).await?;
    info!("added mic track");
    loop {
        let remote_track = conn.recv_track().await?;
        info!(
            "new remote track: {:?} {:?}",
            remote_track.kind(),
            remote_track.codec()
        );
        match remote_track.kind() {
            TrackKind::Audio => {
                audio_ctx.playback(remote_track)?;
            }
            TrackKind::Video => todo!(),
        }
    }
}

// // impl  Codec {
// //     fn kind(&self) -> RtpCodecKind {
// //         match self {

// //         }
// //     }
// // }

// trait TrackLocal: Stream<Item = OutboundFrame> {
//     fn kind(&self) -> TrackKind;
// }

// struct OutboundFrame {
//     data: Bytes,
// }

// // enum RtpCodec {
// //     typ: RtpCodecType,
// //     // Opus {
// //     //     channels: usize
// //     // }
// // }

// // struct TrackLocal{
// //     packetizer: Packetizer
// // }

// // struct TrackRemote {}

// // impl TrackLocal {
// //     fn push_frame()
// // }

// struct MediaTrack {}

// impl RemoteMediaTrack {

// }

// impl MediaTrack for LocalMediaTrack {
//     async fn recv(&mut self) -> Result<MediaFrame> {
//         todo!()
//     }

//     fn kind(&self) -> TrackKind {
//         todo!()
//     }

//     fn codec(&self) -> Codec {
//         todo!()
//     }
//     fn as_local_track(&self) -> Option<&LocalMediaTrack> {
//         Some(self)
//     }
//     fn as_remote_track(&self) -> Option<&RemoteMediaTrack> {
//         None
//     }
// }

// impl MediaTrack for RemoteMediaTrack {
//     async fn recv(&mut self) -> Result<MediaFrame> {
//         let frame = self.receiver.recv().await?;
//         Ok(frame)
//     }

//     fn kind(&self) -> TrackKind {
//         self.kind
//     }

//     fn codec(&self) -> Codec {
//         self.codec
//     }

//     fn as_local_track(&self) -> Option<&LocalMediaTrack> {
//         None
//     }
//     fn as_remote_track(&self) -> Option<&RemoteMediaTrack> {
//         Some(self)
//     }
// }
// trait MediaTrack {
//     fn recv(&self) -> impl Future<Output = Result<MediaFrame>> + Send;
//     fn kind(&self) -> TrackKind;
//     fn codec(&self) -> Codec;
//     fn as_local_track(&self) -> Option<&LocalMediaTrack>;
//     fn as_remote_track(&self) -> Option<&RemoteMediaTrack>;
// }

// use web_audio_api::{
//     context::{AudioContext, AudioContextOptions},
//     media_devices::{self, MediaStreamConstraints, MediaTrackConstraints},
//     media_streams::MediaStream,
//     node::AudioNode,
// };

// const SAMPLE_RATE: u32 = 48_000;

// fn foo() {
//     // Capture microphone input and stream it out to a peer with a processing effect applied to the audio
//     //     navigator.getUserMedia('audio', gotAudio);
//     //     function gotAudio(stream) {
//     //         var microphone = context.createMediaStreamSource(stream);
//     //         var filter = context.createBiquadFilter();
//     //         var peer = context.createMediaStreamDestination();
//     //         microphone.connect(filter);
//     //         filter.connect(peer);
//     //         peerConnection.addStream(peer.stream);
//     //     }

//     let options = AudioContextOptions {
//         sample_rate: Some(SAMPLE_RATE as f32),
//         ..Default::default()
//     };
//     let context = AudioContext::new(options);
//     let mic = {
//         let mut constraints = MediaTrackConstraints::default();
//         // constraints.device_id = source_id;
//         // constraints.channel_count = Some(2);
//         let stream_constraints = MediaStreamConstraints::AudioWithConstraints(constraints);
//         let media = media_devices::get_user_media_sync(stream_constraints);
//         context.create_media_stream_source(&media)
//     };

//     let peer = context.create_media_stream_destination();
//     peer.set_channel_count(1);
//     mic.connect(&peer);
//     let s = peer.stream().clone();
//     send(s);
// }

// fn send(audio: MediaStream) {
//     let track = audio.get_tracks()[0];
// }
