use anyhow::Result;
use iroh::{Endpoint, NodeId};
use iroh_roq::ALPN;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::{
    audio::{start_audio, AudioConfig},
    audio2::AudioContext,
    net, rtc,
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
    ep: &Endpoint,
    audio_config: AudioConfig,
    event_tx: Option<async_channel::Sender<NetEvent>>,
) -> Result<()> {
    let conn = net::accept(ep).await?;
    let node_id = conn.remote_node_id()?;
    info!("accepted connection from {}", node_id.fmt_short());
    send(event_tx.as_ref(), NetEvent::Established(node_id)).await;
    // let (audio_streams, audio_state) = start_audio(audio_config)?;
    // if let Err(err) = net::handle_connection(conn, audio_streams).await {
    //     tracing::warn!("connection closed: {err:?}");
    // }
    let audio_ctx = AudioContext::new(audio_config).await?;
    if let Err(err) = rtc::handle_connection(audio_ctx, conn).await {
        tracing::warn!("connection closed: {err:?}");
    }
    send(event_tx.as_ref(), NetEvent::Closed(node_id)).await;
    // drop(audio_state);
    Ok(())
}

pub async fn connect(
    ep: &Endpoint,
    audio_config: AudioConfig,
    node_id: NodeId,
    event_tx: Option<async_channel::Sender<NetEvent>>,
) -> Result<()> {
    info!("creating audio context");
    let audio_ctx = AudioContext::new(audio_config).await?;
    info!("audio context created");
    let conn = net::connect(ep, node_id).await?;
    send(event_tx.as_ref(), NetEvent::Established(node_id)).await;
    info!(
        "established connection to {}",
        conn.remote_node_id()?.fmt_short()
    );
    // let (audio_streams, audio_state) = start_audio(audio_config)?;
    // net::handle_connection(conn, audio_streams).await?;
    rtc::handle_connection(audio_ctx, conn).await?;
    send(event_tx.as_ref(), NetEvent::Closed(node_id)).await;
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
        let (audio_streams, audio_state) = start_audio(audio_config)?;
        net::handle_connection(conn, audio_streams).await?;
        drop(audio_state);
        anyhow::Ok(())
    });
    let connect_task = n0_future::task::spawn(async move {
        let conn = net::connect(&ep2, ep1_addr).await?;
        let (audio_streams, audio_state) = start_audio(audio_config_2)?;
        net::handle_connection(conn, audio_streams).await?;
        drop(audio_state);
        anyhow::Ok(())
    });

    accept_task.await??;
    connect_task.await??;
    Ok(())
}
