//! Public types of the audio engine.

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use super::frames_to_duration;

/// Identifies one playback of a file. Every `play` call creates a new voice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VoiceId(pub(crate) u64);

impl fmt::Display for VoiceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "voice#{}", self.0)
    }
}

/// Mixer group a voice is routed through. Each bus has its own volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Bus {
    /// Playlist tracks (one at a time, crossfaded).
    Playlist,
    /// Soundboard sounds (any number at once).
    Soundboard,
}

impl Bus {
    pub(crate) const COUNT: usize = 2;

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Playlist => 0,
            Self::Soundboard => 1,
        }
    }
}

/// Parameters of a new voice.
#[derive(Clone, Debug, PartialEq)]
pub struct PlayRequest {
    pub path: PathBuf,
    pub bus: Bus,
    /// Linear gain, `0.0..=1.0`; adjustable while playing.
    pub volume: f32,
    /// Restart at the end of the file; adjustable while playing.
    pub looping: bool,
    /// Fade-in at the start of playback (and of each loop iteration when crossfading).
    pub fade_in: Duration,
    /// Without looping: fade-out ending exactly at the end of the file. With looping and both
    /// fades non-zero: consecutive iterations overlap by this much (crossfade loop, Kenku FM's
    /// behaviour); otherwise the loop is gapless.
    pub fade_out: Duration,
}

/// Notifications from the engine to the application.
#[derive(Clone, Debug, PartialEq)]
pub enum EngineEvent {
    /// The file was opened and the voice is audible (Howler's `load` event in Kenku FM).
    Started { voice: VoiceId, duration: Option<Duration> },
    /// The file could not be opened or decoded.
    Failed { voice: VoiceId, error: String },
    /// Playback reached the end of the file. Never sent while a voice loops.
    Ended { voice: VoiceId },
    /// The voice finished its fade-out after [`AudioBackend::stop`].
    Stopped { voice: VoiceId },
    /// The output device reported an error (for example, it was unplugged).
    OutputError(String),
}

/// Coarse lifecycle of a voice, readable at any time through [`VoiceHandle::state`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceState {
    Loading,
    Playing,
    Paused,
    Finished,
    Failed,
}

impl VoiceState {
    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Loading,
            1 => Self::Playing,
            2 => Self::Paused,
            3 => Self::Finished,
            _ => Self::Failed,
        }
    }

    fn as_u8(self) -> u8 {
        match self {
            Self::Loading => 0,
            Self::Playing => 1,
            Self::Paused => 2,
            Self::Finished => 3,
            Self::Failed => 4,
        }
    }
}

const UNKNOWN_DURATION: u64 = u64::MAX;

/// Lock-free state shared between the application, the engine thread and the audio callback.
///
/// Writers: the audio callback stores the position; the engine thread stores everything else.
#[derive(Debug)]
pub(crate) struct VoiceShared {
    position: AtomicU64,
    duration: AtomicU64,
    generation: AtomicU32,
    state: AtomicU8,
}

impl VoiceShared {
    fn new() -> Self {
        Self {
            position: AtomicU64::new(0),
            duration: AtomicU64::new(UNKNOWN_DURATION),
            generation: AtomicU32::new(0),
            state: AtomicU8::new(VoiceState::Loading.as_u8()),
        }
    }

    pub(crate) fn position(&self) -> u64 {
        self.position.load(Ordering::Relaxed)
    }

    pub(crate) fn store_position(&self, frames: u64) {
        self.position.store(frames, Ordering::Relaxed);
    }

    pub(crate) fn duration(&self) -> Option<u64> {
        let frames = self.duration.load(Ordering::Relaxed);
        (frames != UNKNOWN_DURATION).then_some(frames)
    }

    pub(crate) fn set_duration(&self, frames: Option<u64>) {
        self.duration.store(frames.unwrap_or(UNKNOWN_DURATION), Ordering::Relaxed);
    }

    /// Seek epoch: chunks decoded before the latest seek carry an older generation.
    pub(crate) fn generation(&self) -> u32 {
        self.generation.load(Ordering::Acquire)
    }

    /// Starts a new seek epoch and returns it (engine thread only).
    pub(crate) fn next_generation(&self) -> u32 {
        self.generation.fetch_add(1, Ordering::AcqRel).wrapping_add(1)
    }

    pub(crate) fn state(&self) -> VoiceState {
        VoiceState::from_u8(self.state.load(Ordering::Relaxed))
    }

    pub(crate) fn set_state(&self, state: VoiceState) {
        self.state.store(state.as_u8(), Ordering::Relaxed);
    }
}

/// The application's view of a voice: its ID plus live, lock-free progress information.
#[derive(Clone)]
pub struct VoiceHandle {
    id: VoiceId,
    shared: Arc<VoiceShared>,
    sample_rate: u32,
}

impl VoiceHandle {
    pub(crate) fn new(id: VoiceId, sample_rate: u32) -> Self {
        Self { id, shared: Arc::new(VoiceShared::new()), sample_rate }
    }

    pub fn id(&self) -> VoiceId {
        self.id
    }

    /// Playback position within the current loop iteration.
    pub fn position(&self) -> Duration {
        frames_to_duration(self.shared.position(), self.sample_rate)
    }

    /// Length of the file, once known.
    pub fn duration(&self) -> Option<Duration> {
        self.shared.duration().map(|frames| frames_to_duration(frames, self.sample_rate))
    }

    pub fn state(&self) -> VoiceState {
        self.shared.state()
    }

    pub(crate) fn shared(&self) -> &Arc<VoiceShared> {
        &self.shared
    }

    /// Lets tests simulate playback progress without an engine.
    #[cfg(test)]
    pub(crate) fn set_progress_for_test(&self, position: Duration, duration: Option<Duration>) {
        self.shared.store_position(super::duration_to_frames(position, self.sample_rate));
        self.shared.set_duration(duration.map(|d| super::duration_to_frames(d, self.sample_rate)));
    }
}

impl fmt::Debug for VoiceHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VoiceHandle")
            .field("id", &self.id)
            .field("state", &self.state())
            .field("position", &self.position())
            .finish()
    }
}

/// Everything the application needs from an audio engine. Implemented by
/// [`EngineHandle`](super::EngineHandle); tests substitute a recording fake.
///
/// All methods are fire-and-forget: they enqueue a command and return immediately. Outcomes are
/// reported through [`EngineEvent`]s and [`VoiceHandle`]s.
pub trait AudioBackend {
    /// Starts playing a file. Loading is asynchronous: expect `Started` or `Failed`.
    fn play(&self, request: PlayRequest) -> VoiceHandle;
    /// Fades the voice out over `fade` (immediately if zero), then sends `Stopped`.
    fn stop(&self, voice: VoiceId, fade: Duration);
    fn pause(&self, voice: VoiceId);
    fn resume(&self, voice: VoiceId);
    fn seek(&self, voice: VoiceId, position: Duration);
    fn set_volume(&self, voice: VoiceId, volume: f32);
    fn set_looping(&self, voice: VoiceId, looping: bool);
    fn set_bus_volume(&self, bus: Bus, volume: f32);
}

#[derive(Debug)]
pub enum AudioError {
    Open { path: PathBuf, source: io::Error },
    Unsupported(String),
    NoAudioTrack,
    Decode(String),
    Seek(String),
    Device(String),
    Io(io::Error),
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open { path, source } => write!(f, "cannot open {}: {source}", path.display()),
            Self::Unsupported(what) => write!(f, "unsupported audio: {what}"),
            Self::NoAudioTrack => f.write_str("the file has no playable audio track"),
            Self::Decode(what) => write!(f, "decoding failed: {what}"),
            Self::Seek(what) => write!(f, "seeking failed: {what}"),
            Self::Device(what) => write!(f, "audio device error: {what}"),
            Self::Io(source) => write!(f, "{source}"),
        }
    }
}

impl std::error::Error for AudioError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Open { source, .. } | Self::Io(source) => Some(source),
            _ => None,
        }
    }
}

impl From<io::Error> for AudioError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
