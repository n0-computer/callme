pub mod wasm;

use callme::{
    audio,
    net::bind_endpoint,
    run::{self, NetEvent},
};
use iroh::{Endpoint, NodeId};

pub struct Node {
    pub(crate) ep: Endpoint,
}

impl Node {
    pub async fn spawn() -> anyhow::Result<Self> {
        let ep = bind_endpoint().await?;
        Ok(Self { ep })
    }

    pub fn accept(&self) -> async_channel::Receiver<NetEvent> {
        let audio_config = audio::AudioConfig::default();
        let (event_tx, event_rx) = async_channel::bounded(128);
        let ep = self.ep.clone();
        n0_future::task::spawn(async move {
            if let Err(err) = run::accept(&ep, audio_config, Some(event_tx)).await {
                tracing::error!("accept failed: {err}");
            }
        });
        event_rx
    }

    pub fn connect(&self, node_id: NodeId) -> async_channel::Receiver<NetEvent> {
        let audio_config = audio::AudioConfig::default();
        let (event_tx, event_rx) = async_channel::bounded(128);
        let ep = self.ep.clone();
        n0_future::task::spawn(async move {
            if let Err(err) = run::connect(&ep, audio_config, node_id, Some(event_tx)).await {
                tracing::error!("connetfailed: {err}");
            }
        });
        event_rx
    }
}
