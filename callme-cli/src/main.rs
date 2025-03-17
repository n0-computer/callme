use callme::{
    audio::{AudioConfig, AudioContext},
    net,
    rtc::{handle_connection_with_audio_context, RtcConnection, RtcProtocol},
    NodeId,
};
use clap::Parser;
use dialoguer::Confirm;
use iroh::protocol::Router;
use tokio::task::JoinSet;
use tracing::{error, info, warn};

#[derive(Parser, Debug)]
#[command(about = "Call me iroh", long_about = None)]
struct Args {
    /// The audio input device to use.
    #[arg(short, long)]
    input_device: Option<String>,
    /// The audio output device to use.
    #[arg(short, long)]
    output_device: Option<String>,
    /// If set, audio processing and echo cancellation will be disabled.
    #[arg(long)]
    disable_processing: bool,
    #[clap(subcommand)]
    command: Command,
}

#[derive(Debug, Parser)]
enum Command {
    /// Accept calls from remote nodes.
    Accept {
        /// Accept more than one call.
        #[clap(long)]
        many: bool,
        /// Auto-accept calls without confirmation.
        #[clap(long)]
        auto: bool,
    },
    /// Make calls to remote nodes.
    Connect { node_id: Vec<NodeId> },
    /// Create a debug feedback loop through an in-memory channel.
    Feedback { mode: Option<FeedbackMode> },
    /// List the available audio devices
    ListDevices,
}

#[derive(Debug, Clone, clap::ValueEnum, Default)]
enum FeedbackMode {
    #[default]
    Raw,
    Encoded,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let audio_config = AudioConfig {
        input_device: args.input_device,
        output_device: args.output_device,
        processing_enabled: !args.disable_processing,
    };
    let mut endpoint_shutdown = None;
    let fut = async {
        match args.command {
            Command::Accept { many, auto } => {
                let endpoint = net::bind_endpoint().await?;
                let proto = RtcProtocol::new(endpoint.clone());
                let _router = Router::builder(endpoint.clone())
                    .accept(RtcProtocol::ALPN, proto.clone())
                    .spawn()
                    .await?;

                endpoint_shutdown = Some(endpoint.clone());
                println!("our node id:\n{}", endpoint.node_id());

                let audio_ctx = AudioContext::new(audio_config).await?;

                while let Some(conn) = proto.accept().await? {
                    if !many {
                        handle_connection(audio_ctx, conn).await;
                        break;
                    } else {
                        let peer = conn.transport().remote_node_id()?.fmt_short();
                        let accept =
                            auto || confirm(format!("Incoming call from {peer}. Accept?")).await;
                        if accept {
                            n0_future::task::spawn(handle_connection(audio_ctx.clone(), conn));
                        } else {
                            info!("reject connection from {peer}");
                            conn.transport().close(0u32.into(), b"bye");
                        }
                    }
                }
            }
            Command::Connect { node_id } => {
                let endpoint = net::bind_endpoint().await?;
                endpoint_shutdown = Some(endpoint.clone());

                let proto = RtcProtocol::new(endpoint);
                let audio_ctx = AudioContext::new(audio_config).await?;

                let mut join_set = JoinSet::new();

                for node_id in node_id {
                    info!("connecting to {}", node_id.fmt_short());
                    let audio_ctx = audio_ctx.clone();
                    let proto = proto.clone();
                    join_set.spawn(async move {
                        let fut = async {
                            let conn = proto.connect(node_id).await?;
                            info!("established connection to {}", node_id.fmt_short());
                            handle_connection(audio_ctx, conn).await;
                            anyhow::Ok(())
                        };
                        (node_id, fut.await)
                    });
                }

                while let Some(res) = join_set.join_next().await {
                    let (node_id, res) = res.expect("task panicked");
                    if let Err(err) = res {
                        warn!("failed to connect to {}: {err:?}", node_id.fmt_short())
                    }
                }
            }
            Command::Feedback { mode } => {
                let ctx = AudioContext::new(audio_config).await?;
                let mode = mode.unwrap_or_default();
                println!("start feedback loop for 5 seconds (mode {mode:?}");
                match mode {
                    FeedbackMode::Raw => ctx.feedback_raw().await?,
                    FeedbackMode::Encoded => ctx.feedback_encoded().await?,
                }
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                println!("closing");
            }
            Command::ListDevices => {
                let devices = AudioContext::list_devices().await?;
                println!("{devices:?}");
            }
        }
        anyhow::Ok(())
    };

    tokio::select! {
        res = fut => res?,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutting down");
            if let Some(endpoint) = endpoint_shutdown {
                endpoint.close().await;
            }
        }
    }
    Ok(())
}

async fn handle_connection(audio_ctx: AudioContext, conn: RtcConnection) {
    let peer = conn.transport().remote_node_id().unwrap().fmt_short();
    if let Err(err) = handle_connection_with_audio_context(audio_ctx, conn).await {
        error!("connection from {peer} closed with error: {err:?}",)
    } else {
        info!("connection from {peer} closed")
    }
}

async fn confirm(msg: String) -> bool {
    tokio::task::spawn_blocking(move || Confirm::new().with_prompt(msg).interact().unwrap())
        .await
        .unwrap()
}
