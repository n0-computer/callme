use anyhow::Context;
use clap::Parser;

use callme::{audio::AudioConfig, audio::AudioContext, net, run, NodeId};

#[derive(Parser, Debug)]
#[command(about = "Call me iroh", long_about = None)]
struct Args {
    /// The audio input device to use.
    #[arg(short, long)]
    input_device: Option<String>,
    /// The audio output device to use.
    #[arg(short, long)]
    output_device: Option<String>,
    #[arg(long)]
    disable_processing: bool,
    #[clap(subcommand)]
    command: Command,
}

#[derive(Debug, Parser)]
enum Command {
    /// Accept a call from a remote node.
    Accept,
    /// Make a call to a remote node.
    Connect { node_id: NodeId },
    /// Make a call to many remote nodes.
    ConnectMany { node_id: Vec<NodeId> },
    /// Create a debug feedback loop through iroh-roq.
    FeedbackRoq {
        /// The second audio input device to use.
        #[arg(short, long)]
        input_device_2: Option<String>,
        /// The second audio output device to use.
        #[arg(short, long)]
        output_device_2: Option<String>,
    },
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
            Command::Accept => {
                let endpoint = net::bind_endpoint().await?;
                endpoint_shutdown = Some(endpoint.clone());
                println!("our node id:\n{}", endpoint.node_id());
                run::accept(&endpoint, audio_config, None)
                    .await
                    .context("accept failed")?;
            }
            Command::Connect { node_id } => {
                let endpoint = net::bind_endpoint().await?;
                endpoint_shutdown = Some(endpoint.clone());
                run::connect(&endpoint, audio_config, node_id, None)
                    .await
                    .context("connect failed")?;
            }
            Command::ConnectMany { node_id } => {
                let endpoint = net::bind_endpoint().await?;
                endpoint_shutdown = Some(endpoint.clone());
                run::connect_many(&endpoint, audio_config, node_id, None)
                    .await
                    .context("connect failed")?;
            }
            Command::FeedbackRoq {
                input_device_2,
                output_device_2,
            } => {
                let audio_config_2 = AudioConfig {
                    input_device: input_device_2,
                    output_device: output_device_2,
                    processing_enabled: audio_config.processing_enabled,
                };
                let endpoint = net::bind_endpoint().await?;
                endpoint_shutdown = Some(endpoint.clone());
                run::feedback(endpoint, audio_config, audio_config_2)
                    .await
                    .context("feedback failed")?;
            }
            Command::Feedback { mode } => {
                // let ctx = AudioContext::new(audio_config).await?;
                let ctx = AudioContext::new(audio_config).await?;
                let mode = mode.unwrap_or_default();
                println!("start feedback loop for 5 seconds (mode {mode:?}");
                match mode {
                    FeedbackMode::Raw => ctx.feedback_raw().await?,
                    FeedbackMode::Encoded => ctx.feedback_encoded().await?,
                }
                // // std::future::pending::<()>().await;
                // std::thread::sleep(std::time::Duration::from_secs(5));
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                println!("closing");
                // ctx.ctx().close().await;
            }
            Command::ListDevices => {
                // AudioContext::log_devices();
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
