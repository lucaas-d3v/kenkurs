//! Audio outputs: a cpal device stream, or a silent real-time clock.

#[cfg(feature = "cpal")]
mod device;
mod null;

use crossbeam_channel::Sender;

use super::api::{AudioError, EngineEvent};
use super::mixer::{Mixer, MixerLink};

/// Sample rate of the silent clock (Discord/Opus native rate).
const NULL_SAMPLE_RATE: u32 = 48_000;

/// Which output the engine should use.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum OutputPreference {
    /// The system default device; falls back to the silent clock if it cannot be opened.
    #[default]
    Auto,
    /// A device by name (exact, else case-insensitive substring); falls back like `Auto`.
    Device(String),
    /// No device: a silent real-time clock drives the mixer (headless, containers, CI).
    Null,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputKind {
    Device,
    Null,
}

#[derive(Clone, Debug)]
pub struct OutputInfo {
    pub kind: OutputKind,
    pub device: String,
    pub sample_rate: u32,
    pub channels: u16,
}

/// Keeps the output running; dropping it stops the audio callback.
pub(crate) enum Output {
    #[cfg(feature = "cpal")]
    Device { _stream: cpal::Stream },
    Null { _clock: null::NullOutput },
}

/// Opens the preferred output, falling back to the silent clock.
pub(crate) fn open(preference: &OutputPreference, events: Sender<EngineEvent>) -> Result<(Output, MixerLink, OutputInfo), AudioError> {
    if *preference != OutputPreference::Null {
        #[cfg(feature = "cpal")]
        {
            let name = match preference {
                OutputPreference::Device(name) => Some(name.as_str()),
                _ => None,
            };
            match device::open(name, events) {
                Ok(opened) => return Ok(opened),
                Err(error) => log::warn!("{error}; falling back to the silent clock output"),
            }
        }
        #[cfg(not(feature = "cpal"))]
        {
            drop(events);
            log::warn!("built without the `cpal` feature: no audio device support, using the silent clock output");
        }
    }
    let (mixer, link) = Mixer::new(NULL_SAMPLE_RATE);
    let output = null::NullOutput::start(mixer)?;
    let info = OutputInfo { kind: OutputKind::Null, device: "silent clock".into(), sample_rate: NULL_SAMPLE_RATE, channels: 2 };
    Ok((Output::Null { _clock: output }, link, info))
}

/// Names of the available output devices (empty without the `cpal` feature).
pub fn list_output_devices() -> Result<Vec<String>, AudioError> {
    #[cfg(feature = "cpal")]
    {
        device::list()
    }
    #[cfg(not(feature = "cpal"))]
    {
        Ok(Vec::new())
    }
}
