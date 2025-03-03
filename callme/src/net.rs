use anyhow::{bail, Context, Result};
use futures_concurrency::future::TryJoin;
use iroh::{endpoint::Connection, Endpoint, NodeAddr};
use iroh_roq::{
    rtp::{
        self,
        codecs::opus::OpusPayloader,
        packetizer::{new_packetizer, Packetizer},
    },
    Session, VarInt, ALPN,
};
use n0_future::TryFutureExt;
use tracing::{trace, warn};

use crate::audio::{InboundAudio, MediaStreams, OutboundAudio};

/// RTP payload type
///
/// See https://en.wikipedia.org/wiki/RTP_payload_formats
///
/// We use the first number in the dynamic range. It serves no practical purpose for now.
const RTP_PAYLOAD_TYPE: u8 = 96;

const CLOCK_RATE: u32 = crate::audio::SAMPLE_RATE;

pub async fn bind_endpoint() -> Result<Endpoint> {
    Endpoint::builder()
        .discovery_n0()
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
}

pub async fn connect(endpoint: &Endpoint, node_addr: impl Into<NodeAddr>) -> Result<Connection> {
    let conn = endpoint.connect(node_addr, ALPN).await?;
    Ok(conn)
}

pub async fn accept(endpoint: &Endpoint) -> Result<Connection> {
    let mut conn = endpoint.accept().await.context("endpoint died")?.accept()?;
    if conn.alpn().await? != ALPN {
        bail!("incoming connection with invalid ALPN");
    }
    let conn = conn.await?;
    Ok(conn)
}

pub async fn handle_connection(conn: Connection, audio_streams: MediaStreams) -> Result<()> {
    let MediaStreams {
        outbound_audio_receiver,
        inbound_audio_sender,
    } = audio_streams;
    let flow_id = VarInt::from_u32(0);
    let session = Session::new(conn);
    let send_flow = session.new_send_flow(flow_id).await.unwrap();
    let mut recv_flow = session.new_receive_flow(flow_id).await.unwrap();

    let recv_fut = async move {
        let mut last_ts = None;
        loop {
            let packet = recv_flow.read_rtp().await?;
            trace!(
                "received packet len {}: {:?}",
                packet.payload.len(),
                packet.header
            );
            let packet_ts = packet.header.timestamp;
            let skipped_samples = match last_ts {
                None => None,
                // drop old packets
                // TODO: jitter buffer?
                Some(last_ts) if packet_ts <= last_ts => continue,
                Some(last_ts) => Some(packet_ts - last_ts),
            };
            last_ts = Some(packet_ts);
            if let Err(err) = inbound_audio_sender
                .send(InboundAudio::Opus {
                    payload: packet.payload,
                    skipped_samples,
                })
                .await
            {
                warn!("forwarding opus to player failed: {err:?}");
                break;
            }
        }
        anyhow::Ok(())
    };

    const MTU: usize = 1100;

    let send_fut = async move {
        // Synchronization source, can be used to track different sources within a rtp session
        // Usually randomized, as we only support a single source it does not matter.
        let ssrc = 0;
        let sequencer = Box::new(rtp::sequence::new_random_sequencer());
        let mut packetizer = new_packetizer(
            MTU,
            RTP_PAYLOAD_TYPE,
            ssrc,
            Box::new(OpusPayloader),
            sequencer,
            CLOCK_RATE,
        );
        while let Ok(opus) = outbound_audio_receiver.recv().await {
            let OutboundAudio::Opus {
                payload,
                sample_count,
            } = opus;
            let packets = packetizer.packetize(&payload, sample_count)?;
            for packet in packets {
                trace!("sending {:?}", packet);
                send_flow.send_rtp(&packet)?;
            }
        }
        anyhow::Ok(())
    };
    let send_fut = send_fut.map_err(|err| err.context("rtp sender"));
    let recv_fut = recv_fut.map_err(|err| err.context("rtp receiver"));
    (send_fut, recv_fut).try_join().await?;
    Ok(())
}
