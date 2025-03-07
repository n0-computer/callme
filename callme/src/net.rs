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
