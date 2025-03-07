use std::{
    num::NonZeroUsize,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use callme::{
    audio3::audio_to_opus_track,
    net::bind_endpoint,
    rtc::{RtcConnection, RtcProtocol},
};
use clap::Parser;
use iroh::protocol::Router;
use tracing::info;
use web_audio_api::{
    context::{AudioContext as WebAudioContext, BaseAudioContext},
    node::{AudioNode, AudioScheduledSourceNode},
};

#[derive(Debug, Parser)]
struct Args {
    file: PathBuf,
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

    let options = web_audio_api::context::AudioContextOptions {
        // sink_id: "none".to_string(),
        ..Default::default()
    };
    // let ctx = Arc::new(OfflineAudioContext::new(2, 100, 48_000.));
    println!("create ctx");
    let ctx = Arc::new(WebAudioContext::new(options));
    println!("ctx created");

    // let file = std::fs::File::open(&args.file).unwrap();
    // println!("file opened");
    // let buffer = ctx
    //     .decode_audio_data(file)
    //     .await
    //     .map_err(|e| anyhow!("{e:?}"))?;
    // println!("> playing file: {:?}", args.file);
    // println!("> duration: {:?}", buffer.duration());
    // println!("> length: {:?}", buffer.length());
    // println!("> channels: {:?}", buffer.number_of_channels());
    // println!("> sample rate: {:?}", buffer.sample_rate());
    // println!("> --------------------------------");
    // let mut src = ctx.create_buffer_source();
    // src.connect(&ctx.destination());
    // src.set_buffer(buffer);
    // src.start();
    // let dest = ctx.create_media_stream_destination();
    // let track = audio_to_opus_track(dest);

    while let Some(conn) = rtc.accept().await? {
        info!("accepted");
        let file_path = args.file.clone();
        let ctx = ctx.clone();
        let remote_node = conn.transport().remote_node_id()?;
        let now = Instant::now();
        info!(?remote_node, "connection established");
        tokio::task::spawn(async move {
            if let Err(err) = handle_connection(conn, file_path, ctx).await {
                let elapsed = now.elapsed();
                info!(?remote_node, ?err, "connection closed after {elapsed:?}",);
            }
        });
    }

    Ok(())
}

async fn handle_connection(
    conn: RtcConnection,
    filepath: PathBuf,
    ctx: Arc<WebAudioContext>,
) -> Result<()> {
    let file = std::fs::File::open(filepath).unwrap();
    let buffer = ctx
        .decode_audio_data(file)
        .await
        .map_err(|e| anyhow!("{e:?}"))?;
    tokio::task::spawn({
        let conn = conn.clone();
        async move {
            let dest = ctx.create_media_stream_destination();
            dest.set_channel_count(1);
            let mut src = ctx.create_buffer_source();
            src.set_buffer(buffer);
            src.connect(&dest);
            let track = audio_to_opus_track(dest);
            conn.send_track(track).await?;
            src.start();
            anyhow::Ok(())
        }
    });
    // let file_track = build_file_track(file).await?;
    // conn.send_track(file_track).await?;
    while let Ok(mut track) = conn.recv_track().await {
        info!("incoming track");
        tokio::task::spawn(async move { while let Ok(_) = track.recv().await {} });
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
