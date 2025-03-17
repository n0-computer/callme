use std::str::FromStr;

use anyhow::{bail, Context, Result};
use iroh::{Endpoint, NodeAddr, SecretKey};
pub use iroh_roq::ALPN;

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
