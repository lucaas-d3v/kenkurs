//! Application layer: Kenku FM's playback behaviour on top of the audio engine.
//!
//! [`Controller`] is a plain struct — no threads, generic over [`AudioBackend`] — that owns the
//! library, the settings and the playback state. It ports Kenku FM's `usePlaylistPlayback` and
//! `useSoundboardPlayback` hooks, the playback Redux slices and the remote-control handlers; the
//! egui frontend and the HTTP remote API will both drive it. [`Core`] hosts it on threads next
//! to the real engine, delivers engine events to it and autosaves.
//!
//! [`AudioBackend`]: crate::audio::AudioBackend

mod controller;
mod playlist;
pub mod runtime;
mod soundboard;
#[cfg(test)]
mod tests;

pub use controller::{CollectionEdit, Controller, Dirty, Notice, SoundEdit, TrackEdit};
pub use playlist::{PREVIOUS_RESTART_THRESHOLD, PlaylistPlayer, TRACK_CROSSFADE};
pub use runtime::{Core, CoreConfig, CoreError};
pub use soundboard::{ActiveSound, SoundboardPlayer};

use std::time::Duration;

/// Fade used for "immediate" stops of audible voices, just long enough to avoid a click.
pub const QUICK_FADE: Duration = Duration::from_millis(30);
