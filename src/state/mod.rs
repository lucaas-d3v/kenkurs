//! Application state: plain data plus pure logic, independent of audio and UI.
//!
//! These types mirror Kenku FM's Redux slices (`playlistsSlice`, `soundboardsSlice`,
//! `playlistPlaybackSlice`, `settingsSlice`), with their behaviour ported faithfully and a few
//! edge-case bugs fixed.

pub mod library;
pub mod persist;
pub mod queue;
pub mod settings;

pub use library::{Library, Playlist, Sound, Soundboard, Track};
pub use persist::{PersistError, Store};
pub use queue::{Queue, Removed, Repeat, Step};
pub use settings::{PlaybackPrefs, Settings, StreamingMode};
