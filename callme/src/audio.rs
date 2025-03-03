use anyhow::{Context, Result};
use async_channel::{Receiver, Sender};
use bytes::{Bytes, BytesMut};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, Stream};
use n0_future::time::Instant;
use rtrb::Producer;
use tracing::{debug, error, info, warn};

const RECORD_BUF_SIZE: usize = 960 * 8; // 20ms at 48kHz
pub const SAMPLE_RATE: u32 = 48_000;

pub type OutboundAudioReceiver = Receiver<OutboundAudio>;
pub type InboundAudioSender = async_channel::Sender<InboundAudio>;

#[derive(derive_more::Debug)]
pub struct MediaStreams {
    pub outbound_audio_receiver: OutboundAudioReceiver,
    pub inbound_audio_sender: InboundAudioSender,
}

#[derive(Debug, Clone)]
pub struct AudioConfig {
    /// The input device to use.
    pub input_device: Option<String>,
    /// The output device to use.
    pub output_device: Option<String>,
}

/// Stores the CPAL devices and streams.
/// Audio stops once this is dropped!
#[allow(dead_code)]
pub struct AudioState {
    input_device: Device,
    output_device: Device,
    input_stream: cpal::Stream,
    output_stream: cpal::Stream,
}

// TODO: Make sure this is sound.
// We are not ever accessing the fields of AudioState,
// just keeping them around until drop.
unsafe impl Send for AudioState {}

pub fn start_audio(opts: AudioConfig) -> Result<(MediaStreams, AudioState)> {
    let (outbound_audio_sender, outbound_audio_receiver) = async_channel::bounded(128);
    let (inbound_audio_sender, inbound_audio_receiver) = async_channel::bounded(128);

    let audio_state = setup_audio(opts, outbound_audio_sender, inbound_audio_receiver)
        .expect("failed to setup audio");
    Ok((
        MediaStreams {
            outbound_audio_receiver,
            inbound_audio_sender,
        },
        audio_state,
    ))
}

fn setup_audio(
    opts: AudioConfig,
    outbound_audio_sender: Sender<OutboundAudio>,
    inbound_audio_receiver: Receiver<InboundAudio>,
) -> Result<AudioState> {
    let host = cpal::default_host();

    for device in host.input_devices()? {
        info!("available input device: {}", device.name()?);
    }

    for device in host.output_devices()? {
        info!("available output device: {}", device.name()?);
    }

    // Find our input device. If set in opts, find that one.
    // Otherwise, use default or first in list.
    let input_device = {
        let device = match &opts.input_device {
            None => {
                if let Some(device) = host.default_input_device() {
                    Some(device)
                } else {
                    host.input_devices()?.next()
                }
            }
            Some(device) => host
                .input_devices()?
                .find(|x| x.name().map(|y| &y == device).unwrap_or(false)),
        };
        device.with_context(|| {
            format!(
                "could not find input audio device `{}`",
                opts.input_device.as_deref().unwrap_or("default")
            )
        })?
    };

    info!("using input device `{}`", input_device.name()?);

    // Find our output device. If the input device supports output too, use that.
    // Otherwise, use default or first in list.
    let output_device = {
        let device = match &opts.output_device {
            None => {
                if let Some(device) = host.default_output_device() {
                    Some(device)
                } else {
                    host.output_devices()?.next()
                }
            }
            Some(device) => host
                .output_devices()?
                .find(|x| x.name().map(|y| &y == device).unwrap_or(false)),
        };
        device.with_context(|| {
            format!(
                "could not find output audio device `{}`",
                opts.input_device.as_deref().unwrap_or("default")
            )
        })?
    };

    info!("using output device `{}`", input_device.name()?);

    let input_stream = record(&input_device, outbound_audio_sender).expect("record failed");
    let output_stream = play(&output_device, inbound_audio_receiver).expect("play failed");
    let state = AudioState {
        input_device,
        output_device,
        input_stream,
        output_stream,
    };
    Ok(state)
}

impl Default for AudioConfig {
    fn default() -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        let input_device = std::env::var("CALLME_INPUT_DEVICE").ok();
        #[cfg(target_arch = "wasm32")]
        let input_device = None;

        #[cfg(not(target_arch = "wasm32"))]
        let output_device = std::env::var("CALLME_OUTPUT_DEVICE").ok();
        #[cfg(target_arch = "wasm32")]
        let output_device = None;

        Self {
            input_device,
            output_device,
        }
    }
}

pub enum OutboundAudio {
    Opus { payload: Bytes, sample_count: u32 },
}

pub enum InboundAudio {
    Opus {
        payload: Bytes,
        skipped_samples: Option<u32>,
    },
}

fn record(
    device: &Device,
    outbound_audio_sender: Sender<OutboundAudio>,
) -> Result<Stream, anyhow::Error> {
    info!("Input device: {}", device.name()?);
    info!("Begin recording...");

    let err_fn = move |err| {
        eprintln!("an error occurred on stream: {}", err);
    };

    let range = cpal::SupportedBufferSize::Range { min: 960, max: 960 };
    let config = cpal::SupportedStreamConfig::new(
        1,
        cpal::SampleRate(SAMPLE_RATE),
        range,
        cpal::SampleFormat::I16,
    );
    let mut encoder = OpusFramer::new();

    let callback = move |data: &[i16], _context: &_| {
        // println!("rec cb: send audio {}", data.len());
        for sample in data {
            if let Some((payload, sample_count)) = encoder.push_sample(*sample) {
                if let Err(err) = outbound_audio_sender.try_send(OutboundAudio::Opus {
                    payload,
                    sample_count,
                }) {
                    warn!("failed to forward encoded audio: {err}");
                }
            }
        }
    };
    let config = cpal::StreamConfig::from(config);
    info!("record stream config {config:?}");
    let stream = device.build_input_stream(&config, callback, err_fn, None)?;
    stream.play()?;
    info!("record stream started");

    Ok(stream)
}

pub struct OpusFramer {
    encoder: opus::Encoder,
    samples: Vec<i16>,
    out_buf: BytesMut,
    samples_per_frame: usize,
}

impl OpusFramer {
    pub fn new() -> Self {
        let samples_per_frame = 960; // 20ms at 48000Hz sample rate
        let encoder =
            opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip).unwrap();
        let mut out_buf = BytesMut::new();
        out_buf.resize(samples_per_frame, 0);
        let samples = Vec::new();
        Self {
            encoder,
            out_buf,
            samples,
            samples_per_frame,
        }
    }
    pub fn push_sample(&mut self, sample: i16) -> Option<(Bytes, u32)> {
        self.samples.push(sample);
        if self.samples.len() >= self.samples_per_frame {
            let sample_count = self.samples.len() as u32;
            let size = self
                .encoder
                .encode(&self.samples, &mut self.out_buf)
                .expect("failed to encode");
            self.samples.clear();
            let encoded = self.out_buf.split_to(size).freeze();
            self.out_buf.resize(self.samples_per_frame, 0);
            Some((encoded, sample_count))
        } else {
            None
        }
    }
}

fn play(device: &Device, mut receiver: Receiver<InboundAudio>) -> Result<Stream, anyhow::Error> {
    info!("Output device: {}", device.name()?);

    let range = cpal::SupportedBufferSize::Range { min: 960, max: 960 };
    let config = cpal::SupportedStreamConfig::new(
        1,
        cpal::SampleRate(SAMPLE_RATE),
        range,
        cpal::SampleFormat::I16,
    );

    info!("Begin playing...");

    let err_fn = move |err| {
        error!("an error occurred on playback stream: {}", err);
    };

    let mut last_underflow = Instant::now();
    let mut underflow_count = 0;

    let mut audio_buf = vec![0i16; 960 * 16];
    let mut opus_decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono).unwrap();

    #[cfg(not(target_family = "wasm"))]
    let (mut audio_producer, mut audio_consumer) = rtrb::RingBuffer::new(RECORD_BUF_SIZE);
    #[cfg(target_family = "wasm")]
    let mut decode_pos = 0;

    let callback = move |audio_out: &mut [i16], _: &cpal::OutputCallbackInfo| {
        // on non-wasm targets, we keep the decoding to a separate thread with a ringbuffer
        // to communicate into the audio callback.
        #[cfg(not(target_family = "wasm"))]
        for i in 0..audio_out.len() {
            let Ok(val) = audio_consumer.pop() else {
                let now = Instant::now();
                underflow_count += 1;
                if last_underflow.duration_since(now) > n0_future::time::Duration::from_secs(2) {
                    warn!("play buffer underflow {underflow_count}",);
                }
                last_underflow = now;
                return;
            };
            audio_out[i] = val;
        }

        // on wasm we cannot spawn threads, therefore we decode right within the callback.
        #[cfg(target_family = "wasm")]
        {
            while decode_pos > 0 {
                let end = decode_pos.min(audio_out.len());
                audio_out.copy_from_slice(&audio_buf[..end]);
                decode_pos -= end;
                if let Ok(OutboundAudio::Opus {
                    field1: encoded_buf,
                }) = receiver.try_recv()
                {
                    let len = opus_decoder
                        .decode(&encoded_buf, &mut audio_buf, false)
                        .unwrap();
                    decode_pos += len;
                } else {
                    return;
                }
            }
        }
    };
    let config: cpal::StreamConfig = config.into();
    info!("play stream config {config:?}");
    let stream = device.build_output_stream(&config.into(), callback, err_fn, None)?;
    stream.play()?;
    info!("play stream started");

    #[cfg(not(target_family = "wasm"))]
    std::thread::spawn(move || {
        decode_opus_loop(
            &mut opus_decoder,
            &mut audio_buf,
            &mut receiver,
            &mut audio_producer,
        );
    });
    Ok(stream)
}

#[cfg(not(target_family = "wasm"))]
fn decode_opus_loop(
    opus_decoder: &mut opus::Decoder,
    audio_buf: &mut Vec<i16>,
    opus_receiver: &mut Receiver<InboundAudio>,
    audio_producer: &mut Producer<i16>,
) {
    loop {
        let Ok(InboundAudio::Opus {
            payload: encoded_buf,
            skipped_samples: _,
        }) = opus_receiver.recv_blocking()
        else {
            debug!("stopping encoder thread: channel closed");
            break;
        };
        // println!("deco: recv paket {}", encoded_buf.len());
        let len = opus_decoder.decode(&encoded_buf, audio_buf, false).unwrap();
        let mut fail = 0;
        for i in 0..len {
            if let Err(_err) = audio_producer.push(audio_buf[i]) {
                fail = i;
                break;
            }
        }
        if fail > 0 {
            warn!("failed to push sample {fail} of {len}");
        }
    }
}
