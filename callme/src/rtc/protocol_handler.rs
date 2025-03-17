use anyhow::Result;
use iroh::{protocol::ProtocolHandler, Endpoint, NodeAddr};
use iroh_roq::ALPN;
use n0_future::{boxed::BoxFuture, FutureExt};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::RtcConnection;

#[derive(Debug, Clone)]
pub struct RtcProtocol {
    shutdown_token: CancellationToken,
    endpoint: Endpoint,
    sender: async_channel::Sender<RtcConnection>,
    receiver: async_channel::Receiver<RtcConnection>,
}

impl ProtocolHandler for RtcProtocol {
    fn accept(&self, connecting: iroh::endpoint::Connecting) -> BoxFuture<Result<()>> {
        let sender = self.sender.clone();
        async move {
            debug!("ProtocolHandler::accept: connecting");
            let conn = connecting.await?;
            debug!("ProtocolHandler::accept: conn");
            let conn = RtcConnection::new(conn);
            sender.send(conn).await?;
            Ok(())
        }
        .boxed()
    }

    fn shutdown(&self) -> BoxFuture<()> {
        self.shutdown_token.cancel();
        async move {}.boxed()
    }
}

impl RtcProtocol {
    pub const ALPN: &[u8] = ALPN;
    pub fn new(endpoint: Endpoint) -> Self {
        let (sender, receiver) = async_channel::bounded(16);
        Self {
            sender,
            receiver,
            endpoint,
            shutdown_token: CancellationToken::new(),
        }
    }

    pub async fn accept(&self) -> Result<Option<RtcConnection>> {
        tokio::select! {
            _ = self.shutdown_token.cancelled() => {
                Ok(None)
            }
            conn = self.receiver.recv() => {
                let conn = conn?;
                Ok(Some(conn))
            }
        }
    }

    pub async fn connect(&self, node_addr: impl Into<NodeAddr>) -> Result<RtcConnection> {
        let conn = self.endpoint.connect(node_addr, ALPN).await?;
        Ok(RtcConnection::new(conn))
    }
}
