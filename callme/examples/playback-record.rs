use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use callme::{
    codec::{
        opus::{OpusChannels, OpusEncoder, OPUS_SAMPLE_RATE},
        Codec,
    },
    net::bind_endpoint,
    rtc::{MediaFrame, MediaTrack, RtcConnection, RtcProtocol, TrackKind},
};
use clap::Parser;
use cpal::Sample;
use hound::{WavReader, WavWriter};
use iroh::protocol::Router;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

#[derive(Debug, Parser, Clone)]
struct Args {
    #[clap(short, long)]
    playback_file: Option<PathBuf>,
    #[clap(short, long)]
    record_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let endpoint = bind_endpoint().await?;
    println!("node id: {}", endpoint.node_id());

    let rtc = RtcProtocol::new(endpoint.clone());
    let _router = Router::builder(endpoint)
        .accept(RtcProtocol::ALPN, rtc.clone())
        .spawn()
        .await?;

    while let Some(conn) = rtc.accept().await? {
        info!("accepted");
        let remote_node = conn.transport().remote_node_id()?;
        let now = Instant::now();
        let args = args.clone();
        info!(?remote_node, "connection established");
        tokio::task::spawn(async move {
            if let Err(err) = handle_connection(conn, args).await {
                let elapsed = now.elapsed();
                info!(?remote_node, ?err, "connection closed after {elapsed:?}",);
            }
        });
    }

    Ok(())
}

async fn handle_connection(conn: RtcConnection, args: Args) -> Result<()> {
    if let Some(file_path) = args.playback_file {
        let (sender, receiver) = broadcast::channel(2);
        let track = MediaTrack::new(
            receiver,
            Codec::Opus {
                channels: OpusChannels::Mono,
            },
            TrackKind::Audio,
        );
        std::thread::spawn({
            move || {
                if let Err(err) = stream_wav(file_path, sender) {
                    tracing::error!("stream thread failed: {err:?}");
                } else {
                    tracing::info!("stream thread closed");
                }
            }
        });
        conn.send_track(track).await?;
    }
    // let file_track = build_file_track(file).await?;
    // conn.send_track(file_track).await?;
    let mut id = 0;
    while let Some(mut track) = conn.recv_track().await? {
        info!("incoming track");
        if let Some(dir) = &args.record_dir {
            tokio::fs::create_dir_all(&dir).await?;
            let node_id = conn.transport().remote_node_id()?.fmt_short();
            let suffix = id;
            let file_name = format!("{node_id}-{suffix}.wav");
            let file_path = dir.join(&file_name);
            tokio::task::spawn(async move {
                if let Err(err) = record_wav(file_path, track).await {
                    warn!("failed to record {file_name}: {err:?}");
                } else {
                    info!("recorded {file_name}");
                }
            });
        } else {
            info!("skip track");
            #[allow(clippy::redundant_pattern_matching)]
            tokio::task::spawn(async move { while let Ok(_) = track.recv().await {} });
        }
        id += 1;
    }
    Ok(())
}

async fn record_wav(file_path: PathBuf, mut track: MediaTrack) -> Result<()> {
    let channels = match track.codec() {
        Codec::Opus { channels } => channels,
        _ => bail!("only opus tracks are supported"),
    };
    info!("start recording {file_path:?} with {channels:?}");
    let mut decoder = opus::Decoder::new(48_000, channels.into())?;
    let mut buf = vec![0f32; 960 * channels as usize];
    let file = std::fs::File::create(file_path)?;
    let spec = hound::WavSpec {
        channels: channels as u16,
        sample_rate: 48000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = WavWriter::new(file, spec)?;
    while let Ok(frame) = track.recv().await {
        let MediaFrame {
            payload,
            skipped_frames,
            ..
        } = frame;
        for _ in 0..skipped_frames.unwrap_or(0) {
            let block_count = decoder.decode_float(&[], &mut buf, false)?;
            let sample_count = block_count * channels as usize;
            for sample in &buf[..sample_count] {
                let sample: i16 = sample.to_sample();
                writer.write_sample(sample)?;
            }
        }
        let block_count = decoder.decode_float(&payload, &mut buf, false)?;
        let sample_count = block_count * channels as usize;
        for sample in &buf[..sample_count] {
            let sample: i16 = sample.to_sample();
            writer.write_sample(sample)?;
        }
        writer.flush()?;
    }
    writer.finalize()?;
    info!("finalized!");

    Ok(())
}

fn stream_wav(file_path: PathBuf, sender: broadcast::Sender<MediaFrame>) -> Result<()> {
    'outer: loop {
        let file = std::fs::File::open(&file_path)?;
        let mut reader = WavReader::new(&file)?;
        let channels = match reader.spec().channels {
            1 => OpusChannels::Mono,
            2 => OpusChannels::Stereo,
            n => bail!(
                "wav file has unsupported channel count of {}: must be mono or stereo",
                n
            ),
        };
        if reader.spec().sample_rate != OPUS_SAMPLE_RATE {
            bail!(
                "wav file has invalid sample rate: must be {}",
                OPUS_SAMPLE_RATE
            )
        }
        let mut encoder = OpusEncoder::new(channels);
        info!("wav info: {:?}", reader.spec());
        let start = Instant::now();
        let time_per_sample = Duration::from_secs(1) / 48_000;
        for (i, sample) in reader.samples::<i16>().enumerate() {
            let sample = sample.with_context(|| format!("failed to read sample {i}"))?;
            let sample: f32 = sample.to_sample();
            if let Some((payload, sample_count)) = encoder.push_sample(sample) {
                let frame = MediaFrame {
                    payload,
                    sample_count: Some(sample_count),
                    skipped_frames: None,
                    skipped_samples: None,
                };
                if let Err(_err) = sender.send(frame) {
                    tracing::debug!("encoder skipped frame: failed to forward to track");
                    if sender.receiver_count() == 0 {
                        tracing::warn!("track dropped, stop encoder");
                        break 'outer;
                    }
                } else {
                    tracing::trace!("opus encoder: sent {sample_count}");
                }
                let music_time = time_per_sample * i as u32 / channels as u32;
                let actual_time = start.elapsed();
                let sleep_time = music_time - actual_time;
                debug!("sleep {sleep_time:?}");
                std::thread::sleep(sleep_time);
            }
        }
    }
    Ok(())
}
