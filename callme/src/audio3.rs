use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{anyhow, Result};
use n0_future::task::AbortOnDropHandle;
use processor::{Direction, WebrtcProcessor};
use ringbuf::traits::Observer;
use tokio::sync::broadcast;
use tracing::{error, info};
use web_audio_api::{
    context::AudioContext as WebAudioContext,
    context::{
        AudioContextLatencyCategory, AudioContextOptions, AudioContextRegistration,
        BaseAudioContext,
    },
    media_devices::{
        self, enumerate_devices_sync, MediaDeviceInfo, MediaDeviceInfoKind, MediaStreamConstraints,
        MediaTrackConstraints,
    },
    media_streams::MediaStreamTrack,
    node::{AudioNode, ChannelConfig, MediaStreamAudioDestinationNode},
    worklet::{AudioWorkletNode, AudioWorkletNodeOptions},
    AudioBuffer, AudioBufferOptions,
};

use crate::{
    audio::{
        record::OpusFramer, AudioConfig, AudioPlayer, AudioRecorder, OutboundAudio,
        WebrtcAudioProcessor,
    },
    rtc::{Codec, MediaFrame, MediaTrack, OpusChannels, TrackKind},
};

mod processor;

const SAMPLE_RATE: u32 = 48_000;
const SAMPLE_RATE_F32: f32 = SAMPLE_RATE as f32;

const RENDER_SAMPLE_COUNT: usize = 128;

type DynAudioNode = Arc<dyn web_audio_api::node::AudioNode + Send + Sync + 'static>;

#[derive(derive_more::Debug, Clone)]
#[allow(unused)]
pub struct AudioContext {
    config: Arc<AudioConfig>,
    ctx: Arc<WebAudioContext>,
    processor: WebrtcAudioProcessor,
    #[debug("dyn AudioNode")]
    output: DynAudioNode,
    #[debug("dyn AudioNode")]
    input: DynAudioNode,
    #[debug("dyn AudioNode")]
    mic_source: DynAudioNode,
}

impl AudioContext {
    pub fn log_devices() {
        let devices = enumerate_devices_sync();
        log_devices(&devices);
    }

    pub async fn new(config: AudioConfig) -> Result<Self> {
        Self::new_sync(config)
        // tokio::task::spawn_blocking(|| Self::new_sync(config)).await?
    }

    pub fn new_sync(config: AudioConfig) -> Result<Self> {
        let devices = enumerate_devices_sync();
        log_devices(&devices);

        let sink_id = find_device(
            &devices,
            config.output_device.as_deref(),
            MediaDeviceInfoKind::AudioOutput,
        )
        .unwrap_or_default();
        println!("default output: {sink_id}");
        // let latency_hint = match std::env::var("WEB_AUDIO_LATENCY").as_deref() {
        //     Ok("playback") => AudioContextLatencyCategory::Playback,
        //     _ => AudioContextLatencyCategory::default(),
        // };
        let latency_hint = AudioContextLatencyCategory::Balanced;

        let options = AudioContextOptions {
            sink_id,
            sample_rate: Some(SAMPLE_RATE_F32),
            latency_hint,
            ..AudioContextOptions::default()
        };
        println!("ctx opts {options:?}");

        // Setup context.
        let ctx = WebAudioContext::new(options);

        ctx.set_onstatechange(|event| info!("STATE CHANGE: {event:?}"));

        // Setup webrtc audio processor.
        let processor = WebrtcAudioProcessor::new(2, 2, None)?;

        // Setup output.
        let output_worklet =
            create_webrtc_processor_node(&ctx, processor.clone(), Direction::Render);
        output_worklet.connect(&ctx.destination());

        // setup input
        let stream_constraints = {
            let mut constraints = MediaTrackConstraints::default();
            constraints.device_id = find_device(
                &devices,
                config.input_device.as_deref(),
                MediaDeviceInfoKind::AudioInput,
            );
            constraints.sample_rate = Some(SAMPLE_RATE_F32);
            constraints.latency = Some(1. / 1000. * 10.);
            MediaStreamConstraints::AudioWithConstraints(constraints)
        };
        let mic = media_devices::get_user_media_sync(stream_constraints);
        let mic_source = ctx.create_media_stream_source(&mic);
        let input_worklet =
            create_webrtc_processor_node(&ctx, processor.clone(), Direction::Capture);
        mic_source.connect(input_worklet.as_ref());
        // mic_source.connect(&ctx.destination());
        // input_worklet.connect(output_worklet.as_ref());
        //
        mic_source.set_channel_count(2);
        ctx.destination().set_channel_count(2);
        output_worklet.set_channel_count(2);
        input_worklet.set_channel_count(2);

        let output = output_worklet;
        let input = input_worklet;

        Ok(Self {
            config: Arc::new(config),
            ctx: Arc::new(ctx),
            output,
            input,
            mic_source: Arc::new(mic_source),
            processor,
        })
    }

    pub fn capture(&self) -> Result<MediaTrack> {
        let dest = self.ctx.create_media_stream_destination();
        dest.set_channel_count(1);
        self.input.connect(&dest);
        let track = audio_to_opus_track(dest);
        Ok(track)
    }

    pub fn playback(&self, track: MediaTrack) -> Result<()> {
        let audio = opus_track_to_audio(&track)?;
        let source = self.ctx.create_media_stream_track_source(&audio);
        source.connect(self.output.as_ref());
        Ok(())
    }

    pub fn feedback_raw(&self) {
        self.mic_source.connect(&self.ctx.destination());
    }

    pub fn feedback_processed(&self) {
        self.input.connect(self.output.as_ref());
    }

    pub fn feedback_encoded(&self) -> Result<()> {
        let opus_stream = self.capture()?;
        self.playback(opus_stream)?;
        Ok(())
    }

    pub fn ctx(&self) -> &WebAudioContext {
        &self.ctx
    }
}

type MediaTrackSender = broadcast::Sender<MediaFrame>;

pub fn audio_to_opus_track(node: MediaStreamAudioDestinationNode) -> MediaTrack {
    let (sender, receiver) = tokio::sync::broadcast::channel(8);
    let channels = match node.channel_count() {
        1 => OpusChannels::Mono,
        _ => OpusChannels::Stereo,
    };
    std::thread::spawn(move || {
        if let Err(err) = audio_to_opus_loop(node, sender) {
            error!("media track thread failed: {err:?}");
        }
    });
    let codec = Codec::Opus { channels };
    MediaTrack::new(receiver, codec, TrackKind::Audio)
}

pub fn opus_track_to_audio(track: &MediaTrack) -> Result<MediaStreamTrack> {
    let iter = OpusIterator::try_from_track(&track)?;
    Ok(MediaStreamTrack::from_iter(iter))
}

fn find_device(
    devices: &[MediaDeviceInfo],
    name: Option<&str>,
    kind: MediaDeviceInfoKind,
) -> Option<String> {
    // on linux, default to "pipewire" device, if available.
    #[cfg(target_os = "linux")]
    {
        if name.is_none() {
            if let Some(device) = devices
                .iter()
                .filter(|d| d.kind() == kind)
                .filter(|d| d.label() == "pipewire")
                .next()
            {
                return Some(device.device_id().to_string());
            }
        }
    }

    let name = name?;
    let device = devices
        .iter()
        .filter(|d| d.kind() == kind)
        .filter(|d| d.label().starts_with(name))
        .next();
    device.map(|d| d.device_id().to_string())
}

fn log_devices(devices: &[MediaDeviceInfo]) {
    let input_devices = devices
        .iter()
        .filter(|d| d.kind() == MediaDeviceInfoKind::AudioInput);
    println!("input devices");
    println!("");
    for device in input_devices {
        println!("{device:?}")
    }
    println!("");
    println!("output devices");
    println!("");
    let output_devices = devices
        .iter()
        .filter(|d| d.kind() == MediaDeviceInfoKind::AudioOutput);
    for device in output_devices {
        println!("{device:?}")
    }
}

fn create_webrtc_processor_node(
    ctx: &WebAudioContext,
    processor: WebrtcAudioProcessor,
    direction: Direction,
) -> Arc<dyn AudioNode + Send + Sync + 'static> {
    let opts = AudioWorkletNodeOptions {
        number_of_inputs: 2,
        number_of_outputs: 2,
        output_channel_count: Default::default(),
        parameter_data: Default::default(),
        processor_options: (processor, direction),
        audio_node_options: Default::default(),
    };
    let worklet = AudioWorkletNode::new::<WebrtcProcessor>(ctx, opts);
    Arc::new(worklet)
}

type FallibleBuffer = Result<AudioBuffer, Box<dyn std::error::Error + Send + Sync>>;
struct OpusIterator {
    track: MediaTrack,
    channel_count: usize,
    decoder: Mutex<opus::Decoder>,
    buf: Vec<f32>,
    decode_buf: Vec<f32>,
}

impl OpusIterator {
    pub fn try_from_track(track: &MediaTrack) -> Result<OpusIterator> {
        let channels = match track.codec() {
            Codec::Opus { channels } => channels,
        };
        let channel_count = channels as usize;
        let decode_buf = vec![0f32; 960 * channel_count];
        let decoder = opus::Decoder::new(SAMPLE_RATE, channels.into()).unwrap();
        tracing::trace!("setup opus iterator decoder, channels {channel_count}");
        Ok(Self {
            track: track.clone(),
            channel_count,
            decoder: Mutex::new(decoder),
            decode_buf,
            buf: Vec::new(),
        })
    }
}

impl Iterator for OpusIterator {
    type Item = FallibleBuffer;

    fn next(&mut self) -> Option<Self::Item> {
        tracing::trace!("opus decoder iterator next");
        let channel_count = self.channel_count;
        loop {
            let frame = match self.track.try_recv() {
                Ok(frame) => frame,
                Err(broadcast::error::TryRecvError::Empty) => {
                    tracing::trace!("opus decoder: mediatrack recv empty");
                    break;
                }
                Err(broadcast::error::TryRecvError::Lagged(count)) => {
                    tracing::trace!("opus decoder: mediatrack recv lagged {count}");
                    // // TODO: Deal with lagging.
                    continue;
                }
                Err(broadcast::error::TryRecvError::Closed) => {
                    info!("stop opus to audio loop: media track sender dropped");
                    return None;
                }
            };
            let count = match self.decoder.lock().unwrap().decode_float(
                &frame.payload,
                &mut self.decode_buf,
                false,
            ) {
                Err(err) => return Some(Err(Box::new(err))),
                Ok(count) => count,
            };
            debug_assert!(count % channel_count == 0);
            tracing::trace!("decoded {count} samples");
            self.buf.extend(&self.decode_buf);
        }

        let len = self.buf.len().min(RENDER_SAMPLE_COUNT * channel_count);
        let slices = &self.buf[..len];
        let mut channels = Vec::with_capacity(channel_count);
        // copy rendered audio into output slice
        for i in 0..channel_count {
            let mut channel_buf: Vec<f32> = slices
                .iter()
                .skip(i)
                .step_by(channel_count)
                .cloned()
                .collect();
            channel_buf.resize(RENDER_SAMPLE_COUNT, 0.);
            channels.push(channel_buf);
        }
        self.buf.copy_within(len.., 0);
        self.buf.truncate(self.buf.len() - len);
        let buffer = AudioBuffer::from(channels, SAMPLE_RATE_F32);
        Some(Ok(buffer))
    }
}

fn audio_to_opus_loop(
    node: MediaStreamAudioDestinationNode,
    sender: MediaTrackSender,
) -> Result<()> {
    let track = &node.stream().get_tracks()[0];
    let channel_count = match node.channel_count() {
        1 => 1u16,
        _ => 2,
    };
    let params = crate::audio::StreamParams {
        sample_rate: cpal::SampleRate(SAMPLE_RATE),
        channel_count,
    };
    let mut encoder = OpusFramer::new(params);
    let iter = track.iter();
    for buffer in iter {
        let buffer = buffer.map_err(|e| anyhow!("source iterator failed: {e:?}"))?;
        let samples = Interleave::new_from_buffer(&buffer);
        for sample in samples {
            if let Some((payload, sample_count)) = encoder.push_sample(sample) {
                let frame = MediaFrame {
                    payload,
                    sample_count: Some(sample_count),
                    skipped_frames: None,
                    skipped_samples: None,
                };
                if let Err(_frame) = sender.send(frame) {
                    info!("audio to opus loop underflow: failed to send to track");
                }
            }
        }
    }
    Ok(())
}

pub(crate) struct Interleave<'a> {
    left: &'a [f32],
    right: Option<&'a [f32]>,
    pos: usize,
    x: bool,
}

impl<'a> Interleave<'a> {
    pub fn new_from_buffer(buffer: &'a AudioBuffer) -> Self {
        let right = if buffer.number_of_channels() > 1 {
            Some(buffer.get_channel_data(1))
        } else {
            None
        };
        Self {
            left: buffer.get_channel_data(0),
            right,
            pos: 0,
            x: false,
        }
    }
}

impl<'a> Iterator for Interleave<'a> {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        let out = match (&self.right, self.x) {
            (Some(right), true) => right.get(self.pos),
            _ => self.left.get(self.pos),
        };
        self.pos += 1;
        self.x = !self.x;
        out.map(|s| *s)
    }
}
