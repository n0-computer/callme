use anyhow::{bail, Context, Result};
use futures_concurrency::future::TryJoin;
use iroh::{endpoint::Connection, Endpoint, NodeId};
use iroh_roq::{RtpPacket, Session, VarInt, ALPN};
use n0_future::TryFutureExt;
use tracing::{trace, warn};

use crate::audio::{AudioStreams, OpusPacket};

pub async fn bind_endpoint() -> Result<Endpoint> {
    Endpoint::builder()
        .discovery_n0()
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
}

pub async fn connect(endpoint: &Endpoint, node_id: NodeId) -> Result<Connection> {
    let conn = endpoint.connect(node_id, ALPN).await?;
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

pub async fn handle_connection(conn: Connection, audio_streams: AudioStreams) -> Result<()> {
    let AudioStreams {
        output_producer,
        input_consumer,
    } = audio_streams;
    let flow_id = VarInt::from_u32(0);
    let session = Session::new(conn);
    let send_flow = session.new_send_flow(flow_id).await.unwrap();
    let mut recv_flow = session.new_receive_flow(flow_id).await.unwrap();

    let recv_fut = async move {
        loop {
            let packet = recv_flow.read_rtp().await?;
            trace!(
                "received packet len {}: {:?}",
                packet.payload.len(),
                packet.header
            );
            if let Err(err) = output_producer
                .send(OpusPacket::Encoded(packet.payload))
                .await
            {
                warn!("forwarding opus to player failed: {err:?}");
                break;
            }
        }
        anyhow::Ok(())
    };

    let send_fut = async move {
        let mut seq = 0;
        while let Ok(opus) = input_consumer.recv().await {
            let OpusPacket::Encoded(payload) = opus;
            let mut header = iroh_roq::rtp::header::Header::default();
            header.sequence_number = seq as _;
            // TODO: figure out what this should be
            header.timestamp = 0;
            // The standard format of the fixed RTP data header is used (one marker bit).
            header.marker = true;

            let packet = RtpPacket { header, payload };

            trace!("sending {:?}", packet);
            send_flow.send_rtp(&packet)?;
            seq += 1;
        }
        anyhow::Ok(())
    };
    let send_fut = send_fut.map_err(|err| err.context("rtp sender"));
    let recv_fut = recv_fut.map_err(|err| err.context("rtp receiver"));
    (send_fut, recv_fut).try_join().await?;
    Ok(())
}
