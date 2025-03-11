use anyhow::Result;
use iroh_roq::{
    rtp::{self, codecs::opus::OpusPacket, packetizer::Depacketizer},
    ReceiveFlow,
};
use tokio::sync::{broadcast, oneshot};
use tracing::{trace, warn};
use webrtc_media::io::sample_builder::SampleBuilder;

use crate::{codec::Codec, rtc::MediaFrame};

pub(crate) struct RtpMediaTrackReceiver {
    pub(crate) recv_flow: ReceiveFlow,
    pub(crate) track_sender: broadcast::Sender<MediaFrame>,
    pub(crate) init_tx: Option<oneshot::Sender<Result<Codec>>>,
}

impl RtpMediaTrackReceiver {
    pub async fn run(mut self) {
        if let Err(err) = self.run_inner().await {
            let id: u64 = self.recv_flow.flow_id().into();
            warn!(%id, "rtp receive flow failed: {err}");
            if let Some(tx) = self.init_tx.take() {
                tx.send(Err(err)).ok();
            }
        }
    }

    async fn run_inner(&mut self) -> Result<()> {
        let first_packet = self.recv_flow.read_rtp().await?;
        let codec = Codec::try_from_rtp_payload_type(first_packet.header.payload_type)
            .ok_or_else(|| anyhow::anyhow!("unsupported codec type"))?;
        if let Some(tx) = self.init_tx.take() {
            tx.send(Ok(codec)).ok();
        }
        match codec {
            Codec::Opus { .. } => {
                self.run_loop(OpusPacket, codec.sample_rate(), first_packet)
                    .await
            }
        }
    }

    async fn run_loop<T: Depacketizer>(
        &mut self,
        depacketizer: T,
        sample_rate: u32,
        first_packet: rtp::packet::Packet,
    ) -> Result<()> {
        let mut sample_builder = SampleBuilder::new(16, depacketizer, sample_rate);
        let mut packet = first_packet;
        loop {
            trace!(
                "recv packet len {} seq {} ts {}",
                packet.payload.len(),
                packet.header.sequence_number,
                packet.header.timestamp,
            );
            sample_builder.push(packet);
            if let Some(frame) = sample_builder.pop() {
                let webrtc_media::Sample {
                    data,
                    duration,
                    prev_dropped_packets,
                    timestamp: _,
                    packet_timestamp: _,
                    prev_padding_packets: _,
                } = frame;
                let frame = MediaFrame {
                    payload: data,
                    sample_count: Some((sample_rate as f32 / duration.as_secs_f32()) as u32),
                    skipped_frames: Some(prev_dropped_packets as u32),
                    skipped_samples: None,
                };
                self.track_sender.send(frame)?;
            }

            packet = self.recv_flow.read_rtp().await?;
        }
    }
}
