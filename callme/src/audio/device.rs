use anyhow::{Context, Result};
use cpal::{
    traits::{DeviceTrait, HostTrait},
    BufferSize, Device, Host, SampleFormat, StreamConfig, SupportedStreamConfigRange,
};
use tracing::info;

use super::{AudioFormat, SAMPLE_RATE};
use crate::audio::{DURATION_10MS, DURATION_20MS};

#[derive(Debug, Clone)]
pub struct AudioConfig {
    /// The input device to use.
    pub input_device: Option<String>,
    /// The output device to use.
    pub output_device: Option<String>,
    pub processing_enabled: bool,
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
            processing_enabled: true,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Direction {
    Input,
    Output,
}

pub fn list_devices() -> Result<Devices> {
    let host = cpal::default_host();
    let input = host
        .input_devices()?
        .filter_map(|x| x.name().ok())
        .collect();
    let output = host
        .output_devices()?
        .filter_map(|x| x.name().ok())
        .collect();
    Ok(Devices { input, output })
}

#[derive(Debug, Default)]
pub struct Devices {
    pub input: Vec<String>,
    pub output: Vec<String>,
}

pub fn find_device(host: &cpal::Host, direction: Direction, name: Option<&str>) -> Result<Device> {
    let iter = || match direction {
        Direction::Input => host.input_devices(),
        Direction::Output => host.output_devices(),
    };
    let default = || {
        #[cfg(target_os = "linux")]
        if let Some(device) = iter()?.find(|x| x.name().ok().as_deref() == Some("pipewire")) {
            return anyhow::Ok(Some(device));
        };

        let device = match direction {
            Direction::Input => host.default_input_device(),
            Direction::Output => host.default_output_device(),
        };
        let device = match device {
            Some(device) => Some(device),
            None => iter()?.next(),
        };
        anyhow::Ok(device)
    };

    let device = match &name {
        Some(device) => iter()?.find(|x| x.name().map(|y| &y == device).unwrap_or(false)),
        None => default()?,
    };
    device.with_context(|| {
        format!(
            "could not find input audio device `{}`",
            name.unwrap_or("default")
        )
    })
}

pub fn input_stream_config(device: &Device, format: &AudioFormat) -> Result<StreamInfo> {
    let mut config: Option<cpal::SupportedStreamConfig> = None;
    let mut supported_configs: Vec<_> = device
        .supported_input_configs()
        .expect("failed to list supported audio input configs")
        .collect();
    supported_configs.sort_by(SupportedStreamConfigRange::cmp_default_heuristics);
    for supported_config in supported_configs.iter().rev() {
        if supported_config.channels() != format.channel_count {
            continue;
        }
        if let Some(c) = supported_config.try_with_sample_rate(format.sample_rate) {
            config = Some(c);
            break;
        }
    }
    info!("selected input config: {config:?}");
    let config = config.unwrap_or_else(|| {
        device
            .default_input_config()
            .expect("failed to open defualt audio input config")
    });
    info!("final input config: {config:?}");
    let sample_format = config.sample_format();
    let mut config: cpal::StreamConfig = config.into();
    config.buffer_size = BufferSize::Fixed(format.sample_count(DURATION_20MS) as u32);
    Ok(StreamInfo {
        sample_format,
        config,
    })
}

pub fn output_stream_config(device: &Device, format: &AudioFormat) -> Result<StreamInfo> {
    let mut config: Option<cpal::SupportedStreamConfig> = None;
    let mut supported_configs: Vec<_> = device
        .supported_output_configs()
        .expect("failed to list supported audio input configs")
        .collect();
    supported_configs.sort_by(SupportedStreamConfigRange::cmp_default_heuristics);
    for supported_config in supported_configs.iter().rev() {
        if supported_config.channels() != format.channel_count {
            continue;
        }
        if let Some(c) = supported_config.try_with_sample_rate(format.sample_rate) {
            config = Some(c);
            break;
        }
    }
    info!("selected output config: {config:?}");
    let config = config.unwrap_or_else(|| {
        device
            .default_output_config()
            .expect("failed to open defualt audio input config")
    });
    info!("final output config: {config:?}");
    let sample_format = config.sample_format();
    let mut config: cpal::StreamConfig = config.into();
    config.buffer_size = BufferSize::Fixed(format.sample_count(DURATION_20MS) as u32);
    Ok(StreamInfo {
        sample_format,
        config,
    })
}

#[derive(Debug)]
pub struct StreamInfo {
    pub sample_format: SampleFormat,
    pub config: StreamConfig,
}
