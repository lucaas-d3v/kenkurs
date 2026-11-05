//! User settings (Kenku FM's `settingsSlice` minus the web-browser options) and the persisted
//! playlist playback preferences.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::queue::Repeat;

/// Buffering used when streaming to Discord (Kenku FM: `lowLatency` = 20 ms, `performance` = 1 s).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamingMode {
    LowLatency,
    #[default]
    Performance,
}

/// Playlist playback preferences that survive restarts (Kenku persists the same four fields).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaybackPrefs {
    pub volume: f32,
    pub muted: bool,
    pub shuffle: bool,
    pub repeat: Repeat,
}

impl Default for PlaybackPrefs {
    fn default() -> Self {
        Self { volume: 1.0, muted: false, shuffle: false, repeat: Repeat::Playlist }
    }
}

impl PlaybackPrefs {
    /// Gain applied to the playlist bus.
    pub fn effective_volume(&self) -> f32 {
        if self.muted { 0.0 } else { self.volume }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// Discord bot token. Secret: never logged (see the `Debug` impl).
    pub discord_token: String,
    pub remote_enabled: bool,
    pub remote_address: String,
    pub remote_port: u16,
    pub external_inputs_enabled: bool,
    pub multiple_inputs_enabled: bool,
    pub multiple_outputs_enabled: bool,
    pub streaming_mode: StreamingMode,
    /// Local output device name; `None` selects the system default.
    pub output_device: Option<String>,
    pub playback: PlaybackPrefs,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            discord_token: String::new(),
            remote_enabled: false,
            remote_address: "127.0.0.1".into(),
            remote_port: 3333,
            external_inputs_enabled: false,
            multiple_inputs_enabled: false,
            multiple_outputs_enabled: false,
            streaming_mode: StreamingMode::default(),
            output_device: None,
            playback: PlaybackPrefs::default(),
        }
    }
}

impl fmt::Debug for Settings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let token = if self.discord_token.is_empty() { "<unset>" } else { "<redacted>" };
        f.debug_struct("Settings")
            .field("discord_token", &token)
            .field("remote_enabled", &self.remote_enabled)
            .field("remote_address", &self.remote_address)
            .field("remote_port", &self.remote_port)
            .field("external_inputs_enabled", &self.external_inputs_enabled)
            .field("multiple_inputs_enabled", &self.multiple_inputs_enabled)
            .field("multiple_outputs_enabled", &self.multiple_outputs_enabled)
            .field("streaming_mode", &self.streaming_mode)
            .field("output_device", &self.output_device)
            .field("playback", &self.playback)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_kenku() {
        let settings = Settings::default();
        assert_eq!((settings.remote_address.as_str(), settings.remote_port), ("127.0.0.1", 3333));
        assert_eq!(settings.streaming_mode, StreamingMode::Performance);
        assert_eq!(settings.playback.repeat, Repeat::Playlist);
        assert_eq!(settings.playback.volume, 1.0);
    }

    #[test]
    fn debug_never_prints_the_token() {
        let settings = Settings { discord_token: "super-secret".into(), ..Settings::default() };
        let debug = format!("{settings:?}");
        assert!(!debug.contains("super-secret"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn partial_json_fills_defaults() {
        let settings: Settings = serde_json::from_str(r#"{"remotePort": 4444, "streamingMode": "lowLatency"}"#).unwrap();
        assert_eq!(settings.remote_port, 4444);
        assert_eq!(settings.streaming_mode, StreamingMode::LowLatency);
        assert_eq!(settings.remote_address, "127.0.0.1");
    }

    #[test]
    fn muting_zeroes_the_bus_gain() {
        let prefs = PlaybackPrefs { volume: 0.7, muted: true, ..PlaybackPrefs::default() };
        assert_eq!(prefs.effective_volume(), 0.0);
        assert_eq!(PlaybackPrefs { muted: false, ..prefs }.effective_volume(), 0.7);
    }
}
