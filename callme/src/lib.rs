#![allow(unused_imports)]

pub mod audio;
pub mod codec;
pub mod net;
pub mod rtc;

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
    use tokio::sync::{mpsc, oneshot};

    use crate::{
        audio::{AudioSink, AudioSource, ENGINE_FORMAT},
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
        let (router1, rtc1) = build().await?;
        let (router2, rtc2) = build().await?;
        let addr1 = router1.endpoint().node_addr().await?;

        let (conn1, conn2) = (rtc2.connect(addr1), rtc1.accept()).try_join().await?;

        let conn2 = conn2.unwrap();

        let (mut node1, track1) = MediaTrackOpusEncoder::new(4, ENGINE_FORMAT)?;
        conn1.send_track(track1.clone()).await?;

        let sample_count = ENGINE_FORMAT.sample_count(Duration::from_millis(20));
        // start sending audio at node1
        let (abort_tx, mut abort_rx) = mpsc::channel(1);
        let send_task = tokio::task::spawn(async move {
            println!("loop start");
            let fut = async move {
                loop {
                    #[allow(clippy::question_mark)]
                    if let Err(err) = node1.tick(&vec![0.5; sample_count]) {
                        return Err(err);
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            };
            tokio::select! {
                x = abort_rx.recv() => x.unwrap(),
                x = fut => x.unwrap(),
            }
            println!("loop end");
            conn1.transport().close(1u32.into(), b"bye");
            tokio::time::sleep(Duration::from_millis(20)).await;
            anyhow::Ok(())
        });
        let track2 = conn2.recv_track().await?.unwrap();

        assert_eq!(track1.codec(), track2.codec());

        let mut decoder = MediaTrackOpusDecoder::new(track2)?;
        let mut out = vec![0.; sample_count];
        // we need to wait a bit likely.
        let start = Instant::now();
        // wait for some audio to arrive.
        let expected = sample_count * 3;
        let mut total = 0;
        'outer: loop {
            let n = loop {
                if start.elapsed() > Duration::from_secs(2) {
                    panic!("timeout");
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
                match decoder.tick(&mut out)? {
                    ControlFlow::Continue(0) => continue,
                    ControlFlow::Continue(n) => break n,
                    // this signals end of track, triggered when the connection closes.
                    ControlFlow::Break(()) => break 'outer,
                }
            };
            assert!(out[..n].iter().any(|s| *s != 0.));
            out.fill(0.);
            total += n;
            if total >= expected {
                abort_tx.try_send(()).ok();
            }
            println!("received {n} audio frames, total {total}");
        }
        assert_eq!(total, expected);
        send_task.await??;
        router1.shutdown().await?;
        router2.shutdown().await?;
        Ok(())
    }
}
