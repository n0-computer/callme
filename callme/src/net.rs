// use std::str::FromStr;

// use anyhow::{bail, Context, Result};
// use futures_concurrency::future::TryJoin;
// use iroh::{endpoint::Connection, Endpoint, NodeAddr, SecretKey};
// use iroh_roq::{
//     rtp::{
//         self,
//         codecs::opus::OpusPayloader,
//         packetizer::{new_packetizer, Packetizer},
//     },
//     Session, VarInt, ALPN,
// };
// use n0_future::TryFutureExt;
// use tracing::{trace, warn};

// use crate::audio::{AudioStreams, InboundAudio, OutboundAudio};

// /// RTP payload type
// ///
// /// See https://en.wikipedia.org/wiki/RTP_payload_formats
// ///
// /// We use the first number in the dynamic range. It serves no practical purpose for now.
// const RTP_PAYLOAD_TYPE: u8 = 96;

// const CLOCK_RATE: u32 = crate::audio::SAMPLE_RATE.0;

use std::str::FromStr;

use anyhow::{bail, Context, Result};
use iroh::{Endpoint, NodeAddr, SecretKey};
use iroh_roq::ALPN;

use crate::rtc::RtcConnection;

pub async fn bind_endpoint() -> Result<Endpoint> {
    let secret_key = match std::env::var("IROH_SECRET") {
        Ok(secret) => {
            SecretKey::from_str(&secret).expect("failed to parse secret key from IROH_SECRET")
        }
        Err(_) => SecretKey::generate(&mut rand::rngs::OsRng),
    };
    Endpoint::builder()
        .secret_key(secret_key)
        .discovery_n0()
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
}

pub async fn connect(endpoint: &Endpoint, node_addr: impl Into<NodeAddr>) -> Result<RtcConnection> {
    let conn = endpoint.connect(node_addr, ALPN).await?;
    let conn = RtcConnection::new(conn);
    Ok(conn)
}

pub async fn accept(endpoint: &Endpoint) -> Result<RtcConnection> {
    let mut conn = endpoint.accept().await.context("endpoint died")?.accept()?;
    if conn.alpn().await? != ALPN {
        bail!("incoming connection with invalid ALPN");
    }
    let conn = conn.await?;
    let conn = RtcConnection::new(conn);
    Ok(conn)
}

// pub async fn handle_connection(conn: Connection, audio_streams: AudioStreams) -> Result<()> {
//     let AudioStreams { player, recorder } = audio_streams;
//     let flow_id = VarInt::from_u32(0);
//     let session = Session::new(conn);
//     let send_flow = session.new_send_flow(flow_id).await.unwrap();
//     let mut recv_flow = session.new_receive_flow(flow_id).await.unwrap();

//     let recv_fut = async move {
//         // let mut last_ts = None;
//         let mut last_seq = None;
//         loop {
//             let packet = recv_flow.read_rtp().await?;
//             trace!(
//                 "recv packet len {} seq {} ts {}",
//                 packet.payload.len(),
//                 packet.header.sequence_number,
//                 packet.header.timestamp,
//             );

//             // let packet_ts = packet.header.timestamp;
//             // let skipped_samples = match last_ts {
//             //     None => None,
//             //     // drop old packets
//             //     // TODO: jitter buffer?
//             //     Some(last_ts) if packet_ts <= last_ts => continue,
//             //     Some(last_ts) => Some(packet_ts - last_ts),
//             // };
//             // last_ts = Some(packet_ts);

//             let packet_seq = packet.header.sequence_number as u32;
//             let skipped_frames = match last_seq {
//                 None => None,
//                 Some(last_seq) => {
//                     let expected = last_seq + 1;
//                     if packet_seq < expected {
//                         continue;
//                     } else if packet_seq > expected {
//                         Some(packet_seq - expected)
//                     } else {
//                         None
//                     }
//                 }
//             };
//             last_seq = Some(packet_seq);

//             if let Err(err) = player
//                 .send(InboundAudio::Opus {
//                     payload: packet.payload,
//                     // skipped_samples,
//                     skipped_frames,
//                 })
//                 .await
//             {
//                 warn!("forwarding opus to player failed: {err:?}");
//                 break;
//             }
//         }
//         anyhow::Ok(())
//     };

//     const MTU: usize = 1100;

//     let send_fut = async move {
//         // Synchronization source, can be used to track different sources within a rtp session
//         // Usually randomized, as we only support a single source it does not matter.
//         let ssrc = 0;
//         let sequencer = Box::new(rtp::sequence::new_random_sequencer());
//         let mut packetizer = new_packetizer(
//             MTU,
//             RTP_PAYLOAD_TYPE,
//             ssrc,
//             Box::new(OpusPayloader),
//             sequencer,
//             CLOCK_RATE,
//         );
//         while let Ok(frame) = recorder.recv().await {
//             let OutboundAudio::Opus {
//                 payload,
//                 sample_count,
//             } = frame;
//             let packets = packetizer.packetize(&payload, sample_count)?;
//             for packet in packets {
//                 trace!(
//                     "send packet len {} seq {} ts {}",
//                     packet.payload.len(),
//                     packet.header.sequence_number,
//                     packet.header.timestamp,
//                 );
//                 send_flow.send_rtp(&packet)?;
//             }
//         }
//         anyhow::Ok(())
//     };
//     let send_fut = send_fut.map_err(|err| err.context("rtp sender"));
//     let recv_fut = recv_fut.map_err(|err| err.context("rtp receiver"));
//     (send_fut, recv_fut).try_join().await?;
//     Ok(())
// }
