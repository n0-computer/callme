use anyhow::{anyhow, Result};
use iroh_roq::{
    rtp,
    rtp::{
        codecs::opus::OpusPayloader,
        packetizer::{new_packetizer, Packetizer},
        sequence::Sequencer,
    },
    SendFlow,
};
use tokio::sync::broadcast::error::RecvError;
use tracing::trace;

use super::{MediaFrame, MediaTrack};
use crate::codec::Codec;

#[derive(Debug)]
pub(crate) struct RtpMediaTrackSender {
    pub(crate) track: MediaTrack,
    pub(crate) send_flow: SendFlow,
}

pub(crate) const MTU: usize = 1100;

pub(crate) const CLOCK_RATE: u32 = crate::audio::SAMPLE_RATE.0;

impl RtpMediaTrackSender {
    pub(crate) async fn run(mut self) -> Result<()> {
        let ssrc = 0;
        let sequencer: Box<dyn Sequencer + Send + Sync> =
            Box::new(rtp::sequence::new_random_sequencer());
        let payloader = match self.track.codec() {
            Codec::Opus { .. } => Box::new(OpusPayloader),
        };
        let payload_type = self.track.codec().rtp_payload_type();
        let mut packetizer = new_packetizer(
            MTU,
            payload_type,
            ssrc,
            payloader,
            sequencer.clone(),
            CLOCK_RATE,
        );
        loop {
            let frame = match self.track.recv().await {
                Ok(frame) => frame,
                Err(RecvError::Lagged(n)) => {
                    // increase sequence number for frames skipped due to lagging
                    for _ in 0..n {
                        sequencer.next_sequence_number();
                    }
                    continue;
                }
                Err(RecvError::Closed) => {
                    break;
                }
            };
            let MediaFrame {
                payload,
                sample_count,
                skipped_frames,
                skipped_samples,
            } = frame;
            // increase sequence number for frames skipped at source
            if let Some(skipped_frames) = skipped_frames {
                for _ in 0..skipped_frames {
                    sequencer.next_sequence_number();
                }
            }
            // increase timestamp for skipped samples
            // TODO: should also do that for skipped frames?
            if let Some(skipped_samples) = skipped_samples {
                packetizer.skip_samples(skipped_samples);
            }

            let sample_count = sample_count
                .ok_or_else(|| anyhow!("received media track frame without sample count"))?;
            let packets = packetizer.packetize(&payload, sample_count)?;
            for packet in packets {
                trace!(
                    "send packet len {} seq {} ts {}",
                    packet.payload.len(),
                    packet.header.sequence_number,
                    packet.header.timestamp,
                );
                self.send_flow.send_rtp(&packet)?;
            }
        }
        Ok(())
    }
}
