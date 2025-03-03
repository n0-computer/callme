use std::num::NonZeroUsize;
use std::time::Duration;

use anyhow::{Context, Result};
use async_channel::{Receiver, Sender};
use bytes::{Bytes, BytesMut};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, Sample, SampleFormat, Stream, SupportedStreamConfigRange};
use fixed_resample::{ResamplingCons, ResamplingProd};
use tracing::{debug, error, info, warn};

pub const SAMPLE_RATE: u32 = 48_000;
const SAMPLES_PER_FRAME: usize = 960; // 20ms at 48kHz

const MAX_CHANNELS: usize = 2;

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
    config: AudioConfig,
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
    let input_device = find_input_device(&host, config.input_device.as_deref())?;
    info!("using input device `{}`", input_device.name()?);

    // Find our output device. If the input device supports output too, use that.
    // Otherwise, use default or first in list.
    let output_device = find_output_device(&host, config.output_device.as_deref())?;
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

fn find_input_device(host: &cpal::Host, input_device: Option<&str>) -> Result<Device> {
    let device = match &input_device {
        None => {
            // On linux, prefer "pipewire" device if it exists.
            // On some machines, the "default" ALSA device cannot be opened multiple times,
            // while the "pipewire" device can.
            #[cfg(target_os = "linux")]
            if let Some(device) = host
                .input_devices()?
                .find(|x| x.name().map(|y| &y == "pipewire").unwrap_or(false))
                .or_else(|| host.default_input_device())
            {
                Some(device)
            } else {
                host.input_devices()?.next()
            }

            #[cfg(not(target_os = "linux"))]
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
            input_device.unwrap_or("default")
        )
    })
}

fn find_output_device(host: &cpal::Host, output_device: Option<&str>) -> Result<Device> {
    let device = match &output_device {
        None => {
            // On linux, prefer "pipewire" device if it exists.
            // On some machines, the "default" ALSA device cannot be opened multiple times,
            // while the "pipewire" device can.
            #[cfg(target_os = "linux")]
            if let Some(device) = host
                .output_devices()?
                .find(|x| x.name().map(|y| &y == "pipewire").unwrap_or(false))
                .or_else(|| host.default_output_device())
            {
                Some(device)
            } else {
                host.output_devices()?.next()
            }

            #[cfg(not(target_os = "linux"))]
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
            output_device.unwrap_or("default")
        )
    })
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

    const CHANNEL_COUNT: usize = 1;

    let mut stream_config: Option<cpal::SupportedStreamConfig> = None;
    let mut supported_configs: Vec<_> = device
        .supported_input_configs()
        .expect("failed to list supported audio input configs")
        .collect();
    supported_configs.sort_by(SupportedStreamConfigRange::cmp_default_heuristics);
    for supported_config in supported_configs.iter().rev() {
        if supported_config.channels() != CHANNEL_COUNT as u16 {
            continue;
        }
        if let Some(config) = supported_config.try_with_sample_rate(cpal::SampleRate(SAMPLE_RATE)) {
            stream_config = Some(config);
            break;
        }
    }
    info!("selected input config: {stream_config:?}");
    let stream_config = stream_config.unwrap_or_else(|| {
        device
            .default_input_config()
            .expect("failed to open defualt audio input config")
    });
    info!("final input config: {stream_config:?}");
    let sample_format = stream_config.sample_format();
    let stream_config: cpal::StreamConfig = stream_config.into();

    let channel_count = stream_config.channels as usize;
    assert_eq!(channel_count, CHANNEL_COUNT);
    let (prod, mut cons) = fixed_resample::resampling_channel::<f32, MAX_CHANNELS>(
        NonZeroUsize::new(channel_count).unwrap(),
        stream_config.sample_rate.0,
        SAMPLE_RATE,
        Default::default(),
    );

    std::thread::spawn(move || {
        let mut encoder = OpusFramer::new();
        let mut buf = vec![0f32; SAMPLES_PER_FRAME];
        loop {
            std::thread::sleep(Duration::from_millis(5));
            let available = cons.available_frames();
            let len = available.min(SAMPLES_PER_FRAME);
            if len == 0 {
                continue;
            };
            let _ = cons.read_interleaved(&mut buf[..len]);
            for sample in &buf[..len] {
                if let Some((payload, sample_count)) = encoder.push_sample(*sample) {
                    if let Err(err) = outbound_audio_sender.try_send(OutboundAudio::Opus {
                        payload,
                        sample_count,
                    }) {
                        match err {
                            async_channel::TrySendError::Full(_) => {
                                warn!("failed to forward encoded audio: {err}");
                            }
                            async_channel::TrySendError::Closed(_) => {
                                info!("closing audio input thread: input receiver closed.");
                                break;
                            }
                        }
                    }
                }
            }
        }
    });

    info!("record stream config {stream_config:?}");
    let stream = match sample_format {
        SampleFormat::I8 => build_input_stream::<i8>(&device, &stream_config, prod),
        SampleFormat::I16 => build_input_stream::<i16>(&device, &stream_config, prod),
        SampleFormat::I32 => build_input_stream::<i32>(&device, &stream_config, prod),
        SampleFormat::F32 => build_input_stream::<f32>(&device, &stream_config, prod),
        sample_format => {
            tracing::error!("Unsupported sample format '{sample_format}'");
            Err(cpal::BuildStreamError::StreamConfigNotSupported)
        }
    }?;
    // let stream = build_resampled_input_stream(device, config, prod)
    stream.play()?;
    info!("record stream started");

    Ok(stream)
}

fn build_input_stream<S: dasp_sample::ToSample<f32> + cpal::SizedSample>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut prod: ResamplingProd<f32, MAX_CHANNELS>,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let channel_count = config.channels as usize;
    let mut buf = vec![0f32; SAMPLES_PER_FRAME];
    device.build_input_stream::<S, _, _>(
        config,
        move |data: &[S], _: &_| {
            let len = data.len();
            if len > buf.len() {
                buf.resize(len, 0f32);
            }
            for (i, sample) in data.iter().enumerate() {
                buf[i] = sample.to_sample();
            }
            let pushed_frames = prod.push_interleaved(&buf[..len]);

            if pushed_frames * channel_count < len {
                warn!("audio input fell behind");
            }
        },
        |err| {
            error!("an error occurred on stream: {}", err);
        },
        None,
    )
}

pub struct OpusFramer {
    encoder: opus::Encoder,
    samples: Vec<f32>,
    out_buf: BytesMut,
    samples_per_frame: usize,
}

impl OpusFramer {
    pub fn new() -> Self {
        let samples_per_frame = SAMPLES_PER_FRAME as usize;
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

    pub fn push_sample(&mut self, sample: f32) -> Option<(Bytes, u32)> {
        self.samples.push(sample);
        if self.samples.len() >= self.samples_per_frame {
            let sample_count = self.samples.len() as u32;
            let size = self
                .encoder
                .encode_float(&self.samples, &mut self.out_buf)
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

fn play(device: &Device, receiver: Receiver<InboundAudio>) -> Result<Stream, anyhow::Error> {
    info!("Output device: {}", device.name()?);

    info!("Begin playing...");
    const CHANNEL_COUNT: usize = 1;

    let mut stream_config = None;
    let mut supported_configs: Vec<_> = device
        .supported_input_configs()
        .expect("failed to list supported audio input configs")
        .collect();
    supported_configs.sort_by(SupportedStreamConfigRange::cmp_default_heuristics);
    for supported_config in supported_configs.iter().rev() {
        if supported_config.channels() != CHANNEL_COUNT as u16 {
            continue;
        }
        if let Some(config) = supported_config.try_with_sample_rate(cpal::SampleRate(SAMPLE_RATE)) {
            stream_config = Some(config);
            break;
        }
    }
    info!("selected output config: {stream_config:?}");
    let stream_config = stream_config.unwrap_or_else(|| {
        device
            .default_input_config()
            .expect("failed to open defualt audio input config")
    });
    info!("final output config: {stream_config:?}");
    let sample_format = stream_config.sample_format();
    let channel_count = stream_config.channels() as usize;
    let config: cpal::StreamConfig = stream_config.into();

    let (prod, cons) = fixed_resample::resampling_channel::<f32, MAX_CHANNELS>(
        NonZeroUsize::new(channel_count).unwrap(),
        SAMPLE_RATE,
        config.sample_rate.0,
        Default::default(),
    );
    info!("play stream config {config:?}");
    let stream = match sample_format {
        SampleFormat::I8 => build_output_stream::<i8>(&device, &config, cons),
        SampleFormat::I16 => build_output_stream::<i16>(&device, &config, cons),
        SampleFormat::I32 => build_output_stream::<i32>(&device, &config, cons),
        SampleFormat::F32 => build_output_stream::<f32>(&device, &config, cons),
        sample_format => {
            tracing::error!("Unsupported sample format '{sample_format}'");
            Err(cpal::BuildStreamError::StreamConfigNotSupported)
        }
    }?;
    // let stream = device.build_output_stream(&config.into(), data_fn, err_fn, None)?;
    stream.play()?;
    info!("play stream started");

    std::thread::spawn(move || {
        decode_opus_loop(receiver, prod);
    });
    Ok(stream)
}

fn build_output_stream<S: dasp_sample::FromSample<f32> + cpal::SizedSample>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut cons: ResamplingCons<f32>,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let mut tmp_buffer = vec![0f32; SAMPLES_PER_FRAME * 16];
    device.build_output_stream::<S, _, _>(
        config,
        move |data: &mut [S], _: &_| {
            let len = data.len();
            tmp_buffer.resize(len, 0f32);
            let status = cons.read_interleaved(&mut tmp_buffer[..len]);

            if let fixed_resample::ReadStatus::Underflow { .. } = status {
                tracing::warn!("output callback fell behind");
            }

            for i in 0..len {
                data[i] = tmp_buffer[i].to_sample();
            }
        },
        |err| {
            error!("an error occurred on output stream: {}", err);
        },
        None,
    )
}

fn decode_opus_loop(
    opus_receiver: Receiver<InboundAudio>,
    mut prod: fixed_resample::ResamplingProd<f32, MAX_CHANNELS>,
) {
    let mut audio_buf = vec![0f32; 960 * 16];
    let mut opus_decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono).unwrap();
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
        let len = opus_decoder
            .decode_float(&encoded_buf, &mut audio_buf, false)
            .unwrap();
        let pushed = prod.push_interleaved(&audio_buf[..len]);
        if pushed < len {
            tracing::warn!("output decoder fell behind by {} samples", len - pushed);
        }
    }
}
