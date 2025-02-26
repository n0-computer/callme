use anyhow::Context;
use clap::Parser;

use callme::{
    audio::{self, start_audio},
    net, run, NodeId,
};

#[derive(Parser, Debug)]
#[command(about = "Call me iroh", long_about = None)]
struct Args {
    /// The audio device to use
    #[arg(short, long)]
    device: Option<String>,
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
    FeedbackRoq,
    /// Create a debug feedback loop through an in-memory channel.
    FeedbackDirect,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let audio_opts = audio::Opts {
        device: args.device,
    };
    let ep = net::bind_endpoint().await?;
    match args.command {
        Command::Accept => {
            println!("our node id:\n{}", ep.node_id());
            run::accept(&ep, audio_opts, None)
                .await
                .context("accept failed")?;
        }
        Command::Connect { node_id } => {
            run::connect(&ep, audio_opts, node_id, None)
                .await
                .context("connect failed")?;
        }
        Command::FeedbackRoq => {
            run::feedback(ep, audio_opts)
                .await
                .context("feedback failed")?;
        }
        Command::FeedbackDirect => {
            let (streams, _audio_state) = start_audio(Default::default())?;
            loop {
                let item = streams.input_consumer.recv().await?;
                streams.output_producer.send(item).await?;
            }
        }
    }
    Ok(())
}
