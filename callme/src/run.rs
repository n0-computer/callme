use anyhow::{bail, Context, Result};
use iroh::{Endpoint, NodeAddr, NodeId};
use iroh_roq::ALPN;
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;
use tracing::info;

use crate::{
    audio::{AudioConfig, AudioContext},
    net,
    rtc::{self, RtcConnection},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NetEvent {
    Established(NodeId),
    Closed(NodeId),
}

async fn send<T>(event_tx: Option<&async_channel::Sender<T>>, event: T) {
    if let Some(event_tx) = event_tx {
        if let Err(err) = event_tx.send(event).await {
            tracing::debug!("failed to send event: {err}");
        }
    }
}

pub async fn accept(
    endpoint: &Endpoint,
    audio_config: AudioConfig,
    event_tx: Option<async_channel::Sender<NetEvent>>,
) -> Result<()> {
    let mut conn = endpoint.accept().await.context("endpoint died")?.accept()?;
    if conn.alpn().await? != ALPN {
        bail!("incoming connection with invalid ALPN");
    }
    let conn = conn.await?;
    let conn = RtcConnection::new(conn);
    let node_id = conn.transport().remote_node_id()?;
    send(event_tx.as_ref(), NetEvent::Established(node_id)).await;
    let audio_ctx = AudioContext::new(audio_config).await?;
    if let Err(err) = rtc::handle_connection_with_audio_context(audio_ctx, conn).await {
        tracing::warn!("connection closed: {err:?}");
    }
    send(event_tx.as_ref(), NetEvent::Closed(node_id)).await;
    Ok(())
}

pub async fn connect(
    endpoint: &Endpoint,
    audio_config: AudioConfig,
    node_addr: impl Into<NodeAddr>,
    event_tx: Option<async_channel::Sender<NetEvent>>,
) -> Result<()> {
    let node_addr = node_addr.into();
    let node_id = node_addr.node_id;
    info!("audio context created");
    let conn = endpoint.connect(node_addr, ALPN).await?;
    let conn = RtcConnection::new(conn);
    send(event_tx.as_ref(), NetEvent::Established(node_id)).await;
    info!("established connection to {}", node_id.fmt_short());

    info!("creating audio context");
    let audio_ctx = AudioContext::new(audio_config).await?;

    if let Err(err) = rtc::handle_connection_with_audio_context(audio_ctx, conn).await {
        tracing::warn!("connection closed: {err:?}");
    }

    send(event_tx.as_ref(), NetEvent::Closed(node_id)).await;
    Ok(())
}

pub async fn connect_many(
    ep: &Endpoint,
    audio_config: AudioConfig,
    node_ids: Vec<NodeId>,
    event_tx: Option<async_channel::Sender<NetEvent>>,
) -> Result<()> {
    info!("creating audio context");
    let audio_ctx = AudioContext::new(audio_config).await?;
    info!("audio context created");
    let mut join_set = JoinSet::new();
    for node_id in node_ids {
        let event_tx = event_tx.clone();
        let ep = ep.clone();
        let audio_ctx = audio_ctx.clone();
        join_set.spawn(async move {
            let conn = net::connect(&ep, node_id).await?;
            send(event_tx.as_ref(), NetEvent::Established(node_id)).await;
            info!("established connection to {}", node_id.fmt_short());
            if let Err(err) = rtc::handle_connection_with_audio_context(audio_ctx, conn).await {
                tracing::warn!("connection closed: {err:?}");
            }
            send(event_tx.as_ref(), NetEvent::Closed(node_id)).await;
            anyhow::Ok(())
        });
    }
    while let Some(res) = join_set.join_next().await {
        res??;
    }
    Ok(())
}

pub async fn feedback(
    ep1: Endpoint,
    audio_config: AudioConfig,
    audio_config_2: AudioConfig,
) -> anyhow::Result<()> {
    ep1.home_relay().initialized().await?;
    println!("Endpoint 1 spawned, node id {}", ep1.node_id().fmt_short());
    let ep2 = Endpoint::builder()
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await?;
    println!("Endpoint 2 spawned, node id {}", ep2.node_id().fmt_short());
    let ep1_addr = ep1.node_addr().await?;

    let accept_task = n0_future::task::spawn(async move {
        let conn = net::accept(&ep1).await?;
        let audio_ctx = AudioContext::new(audio_config).await?;
        rtc::handle_connection_with_audio_context(audio_ctx, conn).await?;
        anyhow::Ok(())
    });
    let connect_task = n0_future::task::spawn(async move {
        let conn = net::connect(&ep2, ep1_addr).await?;
        let audio_ctx = AudioContext::new(audio_config_2).await?;
        rtc::handle_connection_with_audio_context(audio_ctx, conn).await?;
        anyhow::Ok(())
    });

    accept_task.await??;
    connect_task.await??;
    Ok(())
}
