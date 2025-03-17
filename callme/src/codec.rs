use self::opus::OpusChannels;
use crate::rtc::TrackKind;

pub mod opus;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[non_exhaustive]
pub enum Codec {
    Opus { channels: OpusChannels },
}

impl Codec {
    /// We use the "dynamic" identifiers 96-127 in a "static" way here
    /// to skip SDP.
    ///
    /// See https://en.wikipedia.org/wiki/RTP_payload_formats
    pub fn rtp_payload_type(&self) -> u8 {
        match self {
            Codec::Opus {
                channels: OpusChannels::Mono,
            } => 96,
            Codec::Opus {
                channels: OpusChannels::Stereo,
            } => 97,
        }
    }

    pub fn try_from_rtp_payload_type(payload_type: u8) -> Option<Self> {
        match payload_type {
            96 => Some(Codec::Opus {
                channels: OpusChannels::Mono,
            }),
            97 => Some(Codec::Opus {
                channels: OpusChannels::Stereo,
            }),
            _ => None,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        match self {
            Codec::Opus { .. } => self::opus::OPUS_SAMPLE_RATE,
        }
    }

    pub fn kind(&self) -> TrackKind {
        match self {
            Codec::Opus { .. } => TrackKind::Audio,
        }
    }
}
