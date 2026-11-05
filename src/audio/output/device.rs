//! Device output through cpal (ALSA on Linux, which also reaches PipeWire and PulseAudio).

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, FromSample, Sample, SampleFormat, SizedSample, StreamConfig, SupportedBufferSize};
use crossbeam_channel::Sender;

use super::super::CHANNELS;
use super::super::api::{AudioError, EngineEvent};
use super::super::mixer::{MAX_BLOCK_FRAMES, Mixer, MixerLink};
use super::{Output, OutputInfo, OutputKind};

/// Discord/Opus native rate: avoids resampling twice once Discord output exists.
const PREFERRED_SAMPLE_RATE: u32 = 48_000;
/// ~21 ms at 48 kHz: responsive soundboard triggers without stressing the scheduler.
const PREFERRED_BUFFER_FRAMES: u32 = 1024;

fn device_error(context: &str, error: impl std::fmt::Display) -> AudioError {
    AudioError::Device(format!("{context}: {error}"))
}

pub(super) fn open(name: Option<&str>, events: Sender<EngineEvent>) -> Result<(Output, MixerLink, OutputInfo), AudioError> {
    let host = cpal::default_host();
    let device = match name {
        Some(name) => find_device(&host, name)?,
        None => host.default_output_device().ok_or_else(|| AudioError::Device("no default output device".into()))?,
    };
    let device_name = device_name(&device);
    let supported = choose_config(&device)?;
    let sample_format = supported.sample_format();
    let mut config = supported.config();

    let mut buffer_sizes = Vec::with_capacity(2);
    if let SupportedBufferSize::Range { min, max } = *supported.buffer_size() {
        if min <= PREFERRED_BUFFER_FRAMES && PREFERRED_BUFFER_FRAMES <= max {
            buffer_sizes.push(BufferSize::Fixed(PREFERRED_BUFFER_FRAMES));
        }
    }
    buffer_sizes.push(BufferSize::Default);

    let mut last_error = None;
    for buffer_size in buffer_sizes {
        config.buffer_size = buffer_size;
        let (mixer, link) = Mixer::new(config.sample_rate);
        match build_stream(&device, &config, sample_format, mixer, events.clone()) {
            Ok(stream) => {
                stream.play().map_err(|error| device_error("cannot start the output stream", error))?;
                let info = OutputInfo {
                    kind: OutputKind::Device,
                    device: device_name,
                    sample_rate: config.sample_rate,
                    channels: config.channels,
                };
                return Ok((Output::Device { _stream: stream }, link, info));
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| AudioError::Device("no usable stream configuration".into())))
}

pub(super) fn list() -> Result<Vec<String>, AudioError> {
    let host = cpal::default_host();
    let devices = host.output_devices().map_err(|error| device_error("cannot list devices", error))?;
    Ok(devices.map(|device| device_name(&device)).collect())
}

fn device_name(device: &cpal::Device) -> String {
    device.description().map(|description| description.name().to_owned()).unwrap_or_else(|_| device.to_string())
}

fn find_device(host: &cpal::Host, name: &str) -> Result<cpal::Device, AudioError> {
    let devices: Vec<cpal::Device> =
        host.output_devices().map_err(|error| device_error("cannot list devices", error))?.collect();
    let wanted = name.to_lowercase();
    let exact = devices.iter().position(|device| device_name(device) == name);
    let partial = || devices.iter().position(|device| device_name(device).to_lowercase().contains(&wanted));
    match exact.or_else(partial) {
        Some(index) => Ok(devices.into_iter().nth(index).expect("index from the same list")),
        None => Err(AudioError::Device(format!("no output device named `{name}`"))),
    }
}

/// The device's default configuration, at 48 kHz when the device supports it.
fn choose_config(device: &cpal::Device) -> Result<cpal::SupportedStreamConfig, AudioError> {
    let default = device.default_output_config().map_err(|error| device_error("no output configuration", error))?;
    if default.sample_rate() == PREFERRED_SAMPLE_RATE {
        return Ok(default);
    }
    if let Ok(ranges) = device.supported_output_configs() {
        for range in ranges {
            if range.channels() == default.channels() && range.sample_format() == default.sample_format() {
                if let Some(config) = range.try_with_sample_rate(PREFERRED_SAMPLE_RATE) {
                    return Ok(config);
                }
            }
        }
    }
    Ok(default)
}

fn build_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    format: SampleFormat,
    mixer: Mixer,
    events: Sender<EngineEvent>,
) -> Result<cpal::Stream, AudioError> {
    match format {
        SampleFormat::F32 => build::<f32>(device, config, mixer, events),
        SampleFormat::F64 => build::<f64>(device, config, mixer, events),
        SampleFormat::I16 => build::<i16>(device, config, mixer, events),
        SampleFormat::I32 => build::<i32>(device, config, mixer, events),
        SampleFormat::U16 => build::<u16>(device, config, mixer, events),
        SampleFormat::I8 => build::<i8>(device, config, mixer, events),
        SampleFormat::U8 => build::<u8>(device, config, mixer, events),
        other => Err(AudioError::Device(format!("unsupported sample format {other:?}"))),
    }
}

fn build<T>(device: &cpal::Device, config: &StreamConfig, mut mixer: Mixer, events: Sender<EngineEvent>) -> Result<cpal::Stream, AudioError>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = usize::from(config.channels.max(1));
    // Allocated here and moved into the callback: the callback itself never allocates.
    let mut stereo = vec![0.0f32; MAX_BLOCK_FRAMES * CHANNELS];
    let data_callback = move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
        for out in data.chunks_mut(MAX_BLOCK_FRAMES * channels) {
            let frames = out.len() / channels;
            let mix = &mut stereo[..frames * CHANNELS];
            mixer.render(mix);
            write_frames(out, mix, channels);
        }
    };
    let error_callback = move |error: cpal::Error| {
        log::error!("audio output error: {error}");
        let _ = events.send(EngineEvent::OutputError(error.to_string()));
    };
    device
        .build_output_stream(*config, data_callback, error_callback, None)
        .map_err(|error| device_error("cannot open the output stream", error))
}

/// Writes stereo frames to a device buffer with `channels` channels.
fn write_frames<T: Sample + FromSample<f32>>(out: &mut [T], stereo: &[f32], channels: usize) {
    if channels == 1 {
        for (sample, frame) in out.iter_mut().zip(stereo.chunks_exact(CHANNELS)) {
            *sample = T::from_sample((frame[0] + frame[1]) * 0.5);
        }
        return;
    }
    for (device_frame, frame) in out.chunks_exact_mut(channels).zip(stereo.chunks_exact(CHANNELS)) {
        device_frame[0] = T::from_sample(frame[0]);
        device_frame[1] = T::from_sample(frame[1]);
        for extra in &mut device_frame[2..] {
            *extra = T::EQUILIBRIUM;
        }
    }
}
