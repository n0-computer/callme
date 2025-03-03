use anyhow::{Context, Result};
use cpal::{
    traits::{DeviceTrait, HostTrait},
    BufferSize, Device, SampleFormat, StreamConfig, SupportedStreamConfigRange,
};
use tracing::info;

use super::SAMPLE_RATE;

#[derive(Debug, Clone)]
pub struct AudioConfig {
    /// The input device to use.
    pub input_device: Option<String>,
    /// The output device to use.
    pub output_device: Option<String>,
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

pub fn find_input_device(host: &cpal::Host, input_device: Option<&str>) -> Result<Device> {
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

pub fn find_output_device(host: &cpal::Host, output_device: Option<&str>) -> Result<Device> {
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

pub fn input_stream_config(device: &Device, buffer_size: usize) -> Result<StreamInfo> {
    let mut config: Option<cpal::SupportedStreamConfig> = None;
    let mut supported_configs: Vec<_> = device
        .supported_input_configs()
        .expect("failed to list supported audio input configs")
        .collect();
    supported_configs.sort_by(SupportedStreamConfigRange::cmp_default_heuristics);
    for supported_config in supported_configs.iter().rev() {
        if let Some(c) = supported_config.try_with_sample_rate(SAMPLE_RATE) {
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
    // let channel_count = config.channels() as usize;

    let mut config: cpal::StreamConfig = config.into();
    config.buffer_size = BufferSize::Fixed(buffer_size as u32);
    Ok(StreamInfo {
        sample_format,
        config,
    })
}

pub fn output_stream_config(device: &Device, buffer_size: usize) -> Result<StreamInfo> {
    let mut config: Option<cpal::SupportedStreamConfig> = None;
    let mut supported_configs: Vec<_> = device
        .supported_output_configs()
        .expect("failed to list supported audio input configs")
        .collect();
    supported_configs.sort_by(SupportedStreamConfigRange::cmp_default_heuristics);
    for supported_config in supported_configs.iter().rev() {
        if let Some(c) = supported_config.try_with_sample_rate(SAMPLE_RATE) {
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
    // let channel_count = config.channels() as usize;

    let mut config: cpal::StreamConfig = config.into();
    config.buffer_size = BufferSize::Fixed(buffer_size as u32);
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
