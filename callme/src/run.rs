use std::time::Duration;

use anyhow::Result;
use iroh::{Endpoint, NodeId};
use iroh_roq::ALPN;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::{
    audio,
    net::{self, handle_connection},
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
    audio_opts: audio::Opts,
    event_tx: Option<async_channel::Sender<NetEvent>>,
) -> Result<()> {
    let conn = net::accept(ep).await?;
    let node_id = conn.remote_node_id()?;
    info!("accepted connection from {}", node_id.fmt_short());
    send(event_tx.as_ref(), NetEvent::Established(node_id)).await;
    let (opus_streams, audio_state) = audio::start_audio(audio_opts)?;
    handle_connection(conn, opus_streams).await?;
    send(event_tx.as_ref(), NetEvent::Closed(node_id)).await;
    drop(audio_state);
    Ok(())
}

pub async fn connect(
    ep: &Endpoint,
    audio_opts: audio::Opts,
    node_id: NodeId,
    event_tx: Option<async_channel::Sender<NetEvent>>,
) -> Result<()> {
    let conn = net::connect(ep, node_id).await?;
    send(event_tx.as_ref(), NetEvent::Established(node_id)).await;
    info!(
        "established connection to {}",
        conn.remote_node_id()?.fmt_short()
    );
    let (opus_streams, audio_state) = audio::start_audio(audio_opts)?;
    handle_connection(conn, opus_streams).await?;
    send(event_tx.as_ref(), NetEvent::Closed(node_id)).await;
    drop(audio_state);
    Ok(())
}

pub async fn feedback(ep1: Endpoint, audio_opts: audio::Opts) -> anyhow::Result<()> {
    ep1.home_relay().initialized().await?;
    println!("ep 1 spawned, node id {}", ep1.node_id().fmt_short());
    let ep2 = Endpoint::builder()
        .discovery_n0()
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await?;
    ep2.home_relay().initialized().await?;
    println!("ep 2 spawned, node id {}", ep1.node_id().fmt_short());

    n0_future::time::sleep(Duration::from_secs(3)).await;

    let (opus_streams1, audio_state1) = audio::start_audio(audio_opts.clone())?;
    let (opus_streams2, audio_state2) = audio::start_audio(audio_opts)?;

    let ep1_node_id = ep1.node_id();
    let accept_task = n0_future::task::spawn(async move {
        let conn = net::accept(&ep1).await?;
        net::handle_connection(conn, opus_streams1).await?;
        anyhow::Ok(())
    });
    let connect_task = n0_future::task::spawn(async move {
        let conn = net::connect(&ep2, ep1_node_id).await?;
        net::handle_connection(conn, opus_streams2).await?;
        anyhow::Ok(())
    });

    accept_task.await??;
    connect_task.await??;
    drop(audio_state1);
    drop(audio_state2);
    Ok(())
}
