use std::time::Duration;

use anyhow::Result;
use callme::net::bind_endpoint;
use clap::Parser;
use futures_concurrency::future::TryJoin;
use iroh::endpoint::Connecting;
use iroh_roq::{Session, VarInt, ALPN};
use n0_future::TryFutureExt;
use tracing::{trace, warn};

#[derive(Debug, Parser)]
struct Args {
    #[clap(short, long)]
    delay: Option<u64>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let endpoint = bind_endpoint().await?;
    println!("node id: {}", endpoint.node_id());

    let opts = Opts {
        delay: Duration::from_millis(args.delay.unwrap_or(200)),
    };
    while let Some(incoming) = endpoint.accept().await {
        let Ok(connecting) = incoming.accept() else {
            continue;
        };
        let opts = opts.clone();
        tokio::task::spawn(async move {
            if let Err(err) = handle_connection(connecting, opts).await {
                warn!("conn terminated with error {err:?}");
            }
        });
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct Opts {
    delay: Duration,
}

async fn handle_connection(mut connecting: Connecting, opts: Opts) -> Result<()> {
    if connecting.alpn().await? != ALPN {
        return Ok(());
    }
    let conn = connecting.await?;

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);

    let flow_id = VarInt::from_u32(0);
    let session = Session::new(conn);
    let send_flow = session.new_send_flow(flow_id).await.unwrap();
    let mut recv_flow = session.new_receive_flow(flow_id).await.unwrap();

    let recv_fut = async move {
        loop {
            let packet = match recv_flow.read_rtp().await {
                Ok(packet) => packet,
                Err(err) => break anyhow::Result::<(), _>::Err(err),
            };
            trace!(
                "recv packet len {} seq {} ts {}",
                packet.payload.len(),
                packet.header.sequence_number,
                packet.header.timestamp,
            );
            let tx = tx.clone();
            tokio::task::spawn(async move {
                tokio::time::sleep(opts.delay).await;
                tx.try_send(packet).ok();
            });
        }
    };

    let send_fut = async move {
        while let Some(packet) = rx.recv().await {
            send_flow.send_rtp(&packet)?;
        }
        anyhow::Ok(())
    };
    let send_fut = send_fut.map_err(|err| err.context("rtp sender"));
    let recv_fut = recv_fut.map_err(|err| err.context("rtp receiver"));
    (send_fut, recv_fut).try_join().await?;
    Ok(())
}
