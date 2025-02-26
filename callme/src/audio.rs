use anyhow::{Context, Result};
use bytes::{Bytes, BytesMut};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, Stream};
use n0_future::time::Instant;
use rtrb::Producer;
use tracing::{debug, error, info, warn};

const RECORD_BUF_SIZE: usize = 960 * 8; // 20ms at 48kHz
const SAMPLE_RATE: u32 = 48_000;

pub type OpusProducer = async_channel::Sender<OpusPacket>;
pub type OpusConsumer = async_channel::Receiver<OpusPacket>;

#[derive(derive_more::Debug)]
pub struct AudioStreams {
    pub output_producer: OpusProducer,
    pub input_consumer: OpusConsumer,
}

#[derive(Debug, Clone)]
pub struct Opts {
    /// The audio device to use
    pub device: Option<String>,
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

pub fn start_audio(opts: Opts) -> Result<(AudioStreams, AudioState)> {
    let (output_producer, output_consumer) = async_channel::bounded(128);
    let (input_producer, input_consumer) = async_channel::bounded(128);

    let audio_state =
        setup_audio(opts, input_producer, output_consumer).expect("failed to setup audio");
    Ok((
        AudioStreams {
            output_producer,
            input_consumer,
        },
        audio_state,
    ))
}

fn setup_audio(
    opts: Opts,
    input_producer: OpusProducer,
    output_consumer: OpusConsumer,
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
        let device = match &opts.device {
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
                opts.device.as_deref().unwrap_or("default")
            )
        })?
    };

    info!("using input device `{}`", input_device.name()?);

    // Find our output device. If the input device supports output too, use that.
    // Otherwise, use default or first in list.
    let output_device = if let Some(_) = input_device.supported_output_configs()?.next() {
        input_device.clone()
    } else {
        let device = match &opts.device {
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
                opts.device.as_deref().unwrap_or("default")
            )
        })?
    };

    info!("using output device `{}`", input_device.name()?);

    let input_stream = record(&input_device, input_producer).expect("record failed");
    let output_stream = play(&output_device, output_consumer).expect("play failed");
    let state = AudioState {
        input_device,
        output_device,
        input_stream,
        output_stream,
    };
    Ok(state)
}

impl Default for Opts {
    fn default() -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        let device = std::env::var("CALLME_DEVICE").ok();

        #[cfg(target_arch = "wasm32")]
        let device = None;

        Self { device }
    }
}

pub enum OpusPacket {
    Encoded(Bytes),
}

fn record(device: &Device, opus_producer: OpusProducer) -> Result<Stream, anyhow::Error> {
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
            if let Some(encoded) = encoder.push_sample(*sample) {
                if let Err(err) = opus_producer.try_send(OpusPacket::Encoded(encoded)) {
                    warn!("failed to forward encoded audio: {err}");
                }
            }
        }
    };
    let stream = device.build_input_stream(&config.into(), callback, err_fn, None)?;
    stream.play()?;

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
        let samples_per_frame = 960;
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
    pub fn push_sample(&mut self, sample: i16) -> Option<Bytes> {
        self.samples.push(sample);
        if self.samples.len() >= self.samples_per_frame {
            let size = self
                .encoder
                .encode(&self.samples, &mut self.out_buf)
                .expect("failed to encode");
            self.samples.clear();
            let encoded = self.out_buf.split_to(size).freeze();
            self.out_buf.resize(self.samples_per_frame, 0);
            Some(encoded)
        } else {
            None
        }
    }
}

fn play(device: &Device, mut opus_consumer: OpusConsumer) -> Result<Stream, anyhow::Error> {
    info!("Output device: {}", device.name()?);

    let range = cpal::SupportedBufferSize::Range { min: 960, max: 960 };
    let config = cpal::SupportedStreamConfig::new(
        1,
        cpal::SampleRate(SAMPLE_RATE),
        range,
        cpal::SampleFormat::I16,
    );

    info!("Begin playing...");

    #[cfg(not(target_family = "wasm"))]
    let (mut audio_producer, mut audio_consumer) = rtrb::RingBuffer::new(RECORD_BUF_SIZE);

    let err_fn = move |err| {
        error!("an error occurred on playback stream: {}", err);
    };

    let mut last_underflow = Instant::now();
    let mut underflow_count = 0;

    let mut audio_buf = vec![0i16; 960 * 16];
    let mut opus_decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono).unwrap();

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
                if let Ok(OpusPacket::Encoded(encoded_buf)) = opus_consumer.try_recv() {
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
    let stream = device.build_output_stream(&config.into(), callback, err_fn, None)?;
    stream.play()?;

    #[cfg(not(target_family = "wasm"))]
    std::thread::spawn(move || {
        decode_opus_loop(
            &mut opus_decoder,
            &mut audio_buf,
            &mut opus_consumer,
            &mut audio_producer,
        );
    });
    Ok(stream)
}

#[cfg(not(target_family = "wasm"))]
fn decode_opus_loop(
    opus_decoder: &mut opus::Decoder,
    audio_buf: &mut Vec<i16>,
    opus_consumer: &mut OpusConsumer,
    audio_producer: &mut Producer<i16>,
) {
    loop {
        let Ok(OpusPacket::Encoded(encoded_buf)) = opus_consumer.recv_blocking() else {
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
