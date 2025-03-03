use anyhow::Context;
use clap::Parser;

use callme::{
    audio::{self, start_audio, AudioConfig},
    net, run, NodeId,
};

#[derive(Parser, Debug)]
#[command(about = "Call me iroh", long_about = None)]
struct Args {
    /// The audio input device to use.
    #[arg(short, long)]
    input_device: Option<String>,
    /// The audio output device to use.
    #[arg(short, long)]
    output_device: Option<String>,
    #[clap(subcommand)]
    command: Command,
}

#[derive(Debug, Parser)]
enum Command {
    /// Accept a call from a remote node.
    Accept,
    /// Make a call to a remote node.
    Connect { node_id: NodeId },
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
    FeedbackDirect,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let audio_config = AudioConfig {
        input_device: args.input_device,
        output_device: args.output_device,
    };
    let ep = net::bind_endpoint().await?;
    // let ep2 = ep.clone();
    let fut = async move {
        match args.command {
            Command::Accept => {
                println!("our node id:\n{}", ep.node_id());
                run::accept(&ep, audio_config, None)
                    .await
                    .context("accept failed")?;
            }
            Command::Connect { node_id } => {
                run::connect(&ep, audio_config, node_id, None)
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
                };
                run::feedback(ep, audio_config, audio_config_2)
                    .await
                    .context("feedback failed")?;
            }
            Command::FeedbackDirect => {
                let (streams, _audio_state) = start_audio(Default::default())?;
                loop {
                    let outbound_item = streams.outbound_audio_receiver.recv().await?;
                    let inbound_item = match outbound_item {
                        audio::OutboundAudio::Opus {
                            payload,
                            sample_count: _,
                        } => audio::InboundAudio::Opus {
                            payload,
                            skipped_samples: None,
                        },
                    };
                    streams.inbound_audio_sender.send(inbound_item).await?;
                }
            }
        }
        anyhow::Ok(())
    };

    fut.await?;
    // tokio::select! {
    //     res = fut => res?,
    //     _ = tokio::signal::ctrl_c() => {
    //         tracing::info!("shutting down");
    //         ep2.close().await;
    //     }
    // }
    Ok(())
}
