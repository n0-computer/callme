use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use callme::{
    codec::{
        opus::{OpusChannels, OpusEncoder, OPUS_STREAM_PARAMS},
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
use tracing::{info, warn};

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
    while let Ok(mut track) = conn.recv_track().await {
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
            ..
            // sample_count,
            // skipped_frames,
            // skipped_samples,
        } = frame;
        // TODO: handle skipped frames
        // for _ in 0..skipped_frame
        let count = decoder.decode_float(&payload, &mut buf, false)?;
        for sample in &buf[..count] {
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
        let mut encoder = OpusEncoder::new(OPUS_STREAM_PARAMS);
        let mut reader = WavReader::new(&file)?;
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
                let music_time = time_per_sample * i as u32;
                let actual_time = start.elapsed();
                let sleep_time = music_time - actual_time;
                println!("sleep {sleep_time:?}");
                std::thread::sleep(sleep_time);
            }
        }
    }
    Ok(())
}
// async fn build_file_track(path: PathBuf) -> Result<MediaTrack> {
//     let (tx, rx) = async_channel::bounded(960);
//     std::thread::spawn(move || read_file(path, tx).unwrap());
//     let (sender, receiver) = broadcast::channel(16);
//     std::thread::spawn(move || {
//         let params = StreamParams {
//             channel_count: 1,
//             sample_rate: SAMPLE_RATE,
//         };
//         let mut framer = OpusFramer::new(params);
//         for i in 0.. {
//             let Ok(sample) = rx.recv_blocking() else {
//                 break;
//             };
//             info!("recv sample");
//             if i % 2 == 0 {
//                 if let Some((payload, count)) = framer.push_sample(sample) {
//                     let frame = MediaFrame {
//                         payload,
//                         sample_count: Some(count),
//                         skipped_frames: None,
//                         skipped_samples: None,
//                     };
//                     sender.send(frame).unwrap();
//                 }
//             }
//         }
//     });
//     let track = MediaTrack::new(
//         receiver,
//         callme::rtc::Codec::Opus,
//         callme::rtc::TrackKind::Audio,
//     );
//     Ok(track)
// }

// fn read_file(path: PathBuf, chan: async_channel::Sender<f32>) -> Result<()> {
//     // Open the media source.
//     let src = std::fs::File::open(path)?;

//     // Create the media source stream.
//     let mss = MediaSourceStream::new(Box::new(src), Default::default());

//     // Probe the media source.
//     let probed = symphonia::default::get_probe().format(
//         &Default::default(),
//         mss,
//         &Default::default(),
//         &Default::default(),
//     )?;

//     // Get the instantiated format reader.
//     let mut format = probed.format;

//     // Find the first audio track with a known (decodeable) codec.
//     let track = format
//         .tracks()
//         .iter()
//         .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
//         .unwrap();
//     // Use the default options for the decoder.

//     // Create a decoder for the track.
//     let mut decoder =
//         symphonia::default::get_codecs().make(&track.codec_params, &Default::default())?;

//     // Store the track identifier, it will be used to filter packets.
//     let track_id = track.id;

//     if track.codec_params.channels.unwrap().count() != 2 {
//         return Err(anyhow::anyhow!("audio needs to be two channels"));
//     }

//     // let resampler = FixedResampler::new(2, in_sample_rate, out_sample_rate, quality, interleaved)
//     //
//     let mut resampler = None;

//     // The decode loop.
//     loop {
//         // Get the next packet from the media format.
//         let packet = match format.next_packet() {
//             Ok(packet) => packet,
//             Err(err) => return Err(err.into()),
//         };

//         // Consume any new metadata that has been read since the last packet.
//         while !format.metadata().is_latest() {
//             // Pop the old head of the metadata queue.
//             format.metadata().pop();

//             // Consume the new metadata at the head of the metadata queue.
//         }

//         // If the packet does not belong to the selected track, skip over it.
//         if packet.track_id() != track_id {
//             continue;
//         }

//         // Decode the packet into audio samples.
//         match decoder.decode(&packet) {
//             Ok(decoded) => {
//                 if resampler.is_none() {
//                     resampler = Some(FixedResampler::<f32, 2>::new(
//                         NonZeroUsize::new(2).unwrap(),
//                         decoded.spec().rate,
//                         48_000,
//                         fixed_resample::ResampleQuality::High,
//                         true,
//                     ));
//                 }
//                 let resampler = resampler.as_mut().unwrap();
//                 let buf = decoded.make_equivalent::<f32>();
//                 let mut sample_buf =
//                     SampleBuffer::<f32>::new(decoded.frames() as u64, buf.spec().clone());
//                 sample_buf.copy_interleaved_typed(&buf);
//                 resampler.process_interleaved(
//                     sample_buf.samples(),
//                     |samples| {
//                         for sample in samples {
//                             chan.send_blocking(*sample).expect("failed to send");
//                         }
//                         // TODO: Encode and send
//                     },
//                     None,
//                     true,
//                 );
//                 // Consume the decoded audio samples (see below).
//             }
//             Err(symphonia::core::errors::Error::IoError(_)) => {
//                 // The packet failed to decode due to an IO error, skip the packet.
//                 continue;
//             }
//             Err(symphonia::core::errors::Error::DecodeError(_)) => {
//                 // The packet failed to decode due to invalid data, skip the packet.
//                 continue;
//             }
//             Err(err) => return Err(err.into()),
//         }
//     }
// }
