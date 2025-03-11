#![allow(unused_imports)]

pub mod audio;
pub mod codec;
pub mod net;
pub mod rtc;
pub mod run;

pub use cpal;
pub use iroh::NodeId;

#[cfg(test)]
mod tests {
    use std::{
        ops::ControlFlow,
        time::{Duration, Instant},
    };

    use futures_concurrency::future::{Join, TryJoin};
    use iroh::protocol::Router;
    use testresult::TestResult;

    use crate::{
        audio::{AudioSink, AudioSource, OPUS_STREAM_PARAMS},
        codec::opus::{MediaTrackOpusDecoder, MediaTrackOpusEncoder},
        net::bind_endpoint,
        rtc::{MediaTrack, RtcProtocol},
    };

    async fn build() -> TestResult<(Router, RtcProtocol)> {
        let endpoint = bind_endpoint().await?;
        let proto = RtcProtocol::new(endpoint.clone());
        let router = Router::builder(endpoint)
            .accept(RtcProtocol::ALPN, proto.clone())
            .spawn()
            .await?;
        Ok((router, proto))
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn smoke() -> TestResult {
        let (r1, p1) = build().await?;
        let (r2, p2) = build().await?;
        let a1 = r1.endpoint().node_addr().await?;

        let (c1, c2) = (p2.connect(a1), p1.accept()).try_join().await?;

        let c2 = c2.unwrap();

        let (mut source, track1) = MediaTrackOpusEncoder::new(4, OPUS_STREAM_PARAMS)?;
        c1.send_track(track1.clone()).await?;

        let sample_count = OPUS_STREAM_PARAMS.sample_count(Duration::from_millis(20));
        let send_task = tokio::task::spawn(async move {
            for _i in 0..1000 {
                source.tick(&vec![0.5; sample_count])?;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            anyhow::Ok(())
        });
        let track2 = c2.recv_track().await?.unwrap();

        assert_eq!(track1.codec(), track2.codec());

        let mut decoder = MediaTrackOpusDecoder::new(track2)?;
        let mut out = vec![0.; sample_count];
        // we need to wait a bit likely.
        let start = Instant::now();
        // wait for some audio to arrive.
        let expected = 1920 * 3;
        let mut total = 0;
        'outer: while total < expected {
            let n = loop {
                if start.elapsed() > Duration::from_secs(2) {
                    panic!("timeout");
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
                match decoder.tick(&mut out)? {
                    ControlFlow::Continue(0) => continue,
                    ControlFlow::Continue(n) => break n,
                    ControlFlow::Break(()) => break 'outer,
                }
            };
            assert!(out[..n].iter().any(|s| *s != 0.));
            out.fill(0.);
            total += n;
            println!("received {n} audio frames, total {total}");
        }
        assert_eq!(total, expected);
        send_task.abort();
        // send_task.await??;
        r1.shutdown().await?;
        r2.shutdown().await?;
        Ok(())
    }
}
