//! Real-time audio engine.
//!
//! ```text
//!  app threads ──EngineCommand──▶ engine thread ──MixerCommand (lock-free)──▶ audio callback
//!       ▲                         │ decodes files ahead ──Chunk queues (lock-free)──▶ voices
//!       └──────EngineEvent────────┴◀──────────────MixerEvent (lock-free)──────────┘
//! ```
//!
//! * The **audio callback** ([`mixer`]) only mixes pre-decoded chunks and applies gain ramps. It
//!   never blocks, allocates, frees memory or touches the filesystem: finished voices are handed
//!   back to the engine thread to be dropped there.
//! * The **engine thread** ([`engine`]) owns one [`stream`] per voice. It decodes and resamples
//!   each file ~0.5 s ahead of playback and bakes loop crossfades and end fades into the chunks.
//! * The **output** ([`output`]) is a cpal device stream, or a silent real-time clock when no
//!   device is available (headless machines, containers, CI) so the timeline keeps running.

mod api;
mod decoder;
mod engine;
mod mixer;
mod output;
mod ramp;
mod resample;
mod stream;
mod voice;

pub use api::{
    AudioBackend, AudioError, Bus, EngineEvent, PlayRequest, VoiceHandle, VoiceId, VoiceState,
};
pub use engine::{AudioEngine, EngineConfig, EngineHandle};
pub use output::{OutputInfo, OutputKind, OutputPreference, list_output_devices};

/// The engine mixes interleaved stereo internally.
pub(crate) const CHANNELS: usize = 2;

/// Converts a duration to a frame count at `sample_rate`.
pub(crate) fn duration_to_frames(duration: std::time::Duration, sample_rate: u32) -> u64 {
    let frames = duration.as_nanos() * u128::from(sample_rate) / 1_000_000_000;
    u64::try_from(frames).unwrap_or(u64::MAX)
}

/// Converts a frame count at `sample_rate` to a duration.
pub(crate) fn frames_to_duration(frames: u64, sample_rate: u32) -> std::time::Duration {
    let nanos = u128::from(frames) * 1_000_000_000 / u128::from(sample_rate.max(1));
    std::time::Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

/// Saturating conversion for ramp lengths.
pub(crate) fn frames_u32(frames: u64) -> u32 {
    u32::try_from(frames).unwrap_or(u32::MAX)
}
