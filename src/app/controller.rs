//! The controller: state, engine events and library editing.

use crate::audio::{AudioBackend, EngineEvent};
use crate::state::{Library, Removed, Settings, Sound, Track};

use super::playlist::PlaylistPlayer;
use super::soundboard::SoundboardPlayer;

/// A message for the user (Kenku FM shows these in a snackbar).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub message: String,
}

/// What changed since the last save.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dirty {
    pub library: bool,
    pub settings: bool,
}

/// Changes to a playlist or soundboard; `None` leaves a field as it is.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CollectionEdit {
    pub title: Option<String>,
    pub background: Option<String>,
}

/// Changes to a track; `None` leaves a field as it is.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TrackEdit {
    pub title: Option<String>,
    pub url: Option<String>,
}

/// Changes to a sound; `None` leaves a field as it is.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SoundEdit {
    pub title: Option<String>,
    pub url: Option<String>,
    pub looping: Option<bool>,
    pub volume: Option<f32>,
    pub fade_in: Option<u32>,
    pub fade_out: Option<u32>,
}

pub struct Controller<B: AudioBackend> {
    pub(super) backend: B,
    pub(super) library: Library,
    pub(super) settings: Settings,
    pub(super) playlist: PlaylistPlayer,
    pub(super) soundboard: SoundboardPlayer,
    pub(super) notices: Vec<Notice>,
    pub(super) dirty: Dirty,
}

impl<B: AudioBackend> Controller<B> {
    pub fn new(backend: B, library: Library, settings: Settings) -> Self {
        let controller = Self {
            backend,
            library,
            settings,
            playlist: PlaylistPlayer::default(),
            soundboard: SoundboardPlayer::default(),
            notices: Vec::new(),
            dirty: Dirty::default(),
        };
        controller.apply_playlist_gain();
        controller
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn library(&self) -> &Library {
        &self.library
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn playlist(&self) -> &PlaylistPlayer {
        &self.playlist
    }

    pub fn soundboard(&self) -> &SoundboardPlayer {
        &self.soundboard
    }

    /// Changes settings. Playback preferences (volume, mute, repeat) take effect immediately.
    pub fn update_settings(&mut self, update: impl FnOnce(&mut Settings)) {
        update(&mut self.settings);
        self.dirty.settings = true;
        self.apply_playlist_gain();
        self.sync_track_looping();
    }

    /// Messages for the user since the last call.
    pub fn take_notices(&mut self) -> Vec<Notice> {
        std::mem::take(&mut self.notices)
    }

    /// What needs saving since the last call.
    pub fn take_dirty(&mut self) -> Dirty {
        std::mem::take(&mut self.dirty)
    }

    /// Flags data for saving again (e.g. after a failed write).
    pub fn mark_dirty(&mut self, dirty: Dirty) {
        self.dirty.library |= dirty.library;
        self.dirty.settings |= dirty.settings;
    }

    /// Applies an engine event. Events about voices the controller no longer tracks (stopped,
    /// replaced) are ignored.
    pub fn handle_engine_event(&mut self, event: &EngineEvent) {
        match event {
            EngineEvent::Started { voice, .. } => {
                if !self.playlist_started(*voice) {
                    self.soundboard_started(*voice);
                }
            }
            EngineEvent::Failed { voice, error } => {
                if !self.playlist_failed(*voice, error) {
                    self.soundboard_failed(*voice, error);
                }
            }
            EngineEvent::Ended { voice } => {
                if !self.playlist_ended(*voice) {
                    self.soundboard_ended(*voice);
                }
            }
            EngineEvent::Stopped { .. } => {}
            EngineEvent::OutputError(message) => self.notify(format!("Audio output error: {message}")),
        }
    }

    pub(super) fn notify(&mut self, message: String) {
        log::warn!("{message}");
        self.notices.push(Notice { message });
    }

    // ----- playlists ---------------------------------------------------------------------------

    /// Creates an empty playlist at the end of the list and returns its ID.
    pub fn add_playlist(&mut self, title: impl Into<String>) -> String {
        self.dirty.library = true;
        self.library.add_playlist(title, "").id.clone()
    }

    pub fn edit_playlist(&mut self, id: &str, edit: CollectionEdit) -> bool {
        let Some(playlist) = self.library.playlist_mut(id) else {
            return false;
        };
        if let Some(title) = edit.title {
            playlist.title = title;
        }
        if let Some(background) = edit.background {
            playlist.background = background;
        }
        self.dirty.library = true;
        true
    }

    /// Deletes a playlist and its tracks, stopping playback if it is the one playing.
    pub fn remove_playlist(&mut self, id: &str) -> bool {
        if self.playlist.queue.as_ref().is_some_and(|queue| queue.playlist_id() == id) {
            self.clear_playlist_playback();
        }
        let removed = self.library.remove_playlist(id).is_some();
        self.dirty.library |= removed;
        removed
    }

    pub fn move_playlist(&mut self, active: &str, over: &str) -> bool {
        let moved = self.library.move_playlist(active, over);
        self.dirty.library |= moved;
        moved
    }

    /// Inserts tracks at the top of a playlist (and of its play queue if it is playing).
    pub fn add_tracks(&mut self, playlist_id: &str, tracks: Vec<Track>) -> bool {
        let ids: Vec<String> = tracks.iter().map(|track| track.id.clone()).collect();
        if !self.library.add_tracks(playlist_id, tracks) {
            return false;
        }
        if let Some(queue) = self.playlist.queue.as_mut().filter(|queue| queue.playlist_id() == playlist_id) {
            queue.insert_front(&ids);
        }
        self.dirty.library = true;
        true
    }

    pub fn edit_track(&mut self, track_id: &str, edit: TrackEdit) -> bool {
        let Some(track) = self.library.track_mut(track_id) else {
            return false;
        };
        if let Some(title) = edit.title {
            track.title = title;
        }
        if let Some(url) = edit.url {
            track.url = url;
        }
        let updated = track.clone();
        // Keep the "now playing" copy in sync.
        if let Some(current) = self.playlist.track.as_mut().filter(|current| current.id == track_id) {
            *current = updated;
        }
        self.dirty.library = true;
        true
    }

    /// Deletes a track. Removing the track that is playing stops playback (as in Kenku FM).
    pub fn remove_track(&mut self, playlist_id: &str, track_id: &str) -> bool {
        let in_use = self.playlist.track.as_ref().is_some_and(|track| track.id == track_id)
            || self.playlist.loading.as_ref().is_some_and(|(_, track)| track.id == track_id);
        if in_use {
            self.clear_playlist_playback();
        } else if let Some(queue) = self.playlist.queue.as_mut().filter(|queue| queue.playlist_id() == playlist_id) {
            if queue.remove(track_id) == Removed::Current {
                self.playlist.queue = None;
            }
        }
        let removed = self.library.remove_track(playlist_id, track_id).is_some();
        self.dirty.library |= removed;
        removed
    }

    pub fn move_track(&mut self, playlist_id: &str, active: &str, over: &str) -> bool {
        if !self.library.move_track(playlist_id, active, over) {
            return false;
        }
        if let Some(queue) = self.playlist.queue.as_mut().filter(|queue| queue.playlist_id() == playlist_id) {
            queue.move_track(active, over);
        }
        self.dirty.library = true;
        true
    }

    // ----- soundboards -------------------------------------------------------------------------

    /// Creates an empty soundboard at the end of the list and returns its ID.
    pub fn add_soundboard(&mut self, title: impl Into<String>) -> String {
        self.dirty.library = true;
        self.library.add_soundboard(title, "").id.clone()
    }

    pub fn edit_soundboard(&mut self, id: &str, edit: CollectionEdit) -> bool {
        let Some(soundboard) = self.library.soundboard_mut(id) else {
            return false;
        };
        if let Some(title) = edit.title {
            soundboard.title = title;
        }
        if let Some(background) = edit.background {
            soundboard.background = background;
        }
        self.dirty.library = true;
        true
    }

    /// Deletes a soundboard and its sounds, fading out any of them that are playing.
    pub fn remove_soundboard(&mut self, id: &str) -> bool {
        let sound_ids: Vec<String> = match self.library.soundboard(id) {
            Some(soundboard) => soundboard.sounds.iter().map(|sound| sound.id.clone()).collect(),
            None => return false,
        };
        for sound_id in &sound_ids {
            self.stop_sound(sound_id);
        }
        self.library.remove_soundboard(id);
        self.dirty.library = true;
        true
    }

    pub fn move_soundboard(&mut self, active: &str, over: &str) -> bool {
        let moved = self.library.move_soundboard(active, over);
        self.dirty.library |= moved;
        moved
    }

    /// Inserts sounds at the top of a soundboard.
    pub fn add_sounds(&mut self, soundboard_id: &str, sounds: Vec<Sound>) -> bool {
        let added = self.library.add_sounds(soundboard_id, sounds);
        self.dirty.library |= added;
        added
    }

    /// Edits a sound. Volume and loop changes apply live to a playing instance, like Kenku FM;
    /// other changes take effect the next time the sound is played.
    pub fn edit_sound(&mut self, sound_id: &str, edit: SoundEdit) -> bool {
        let Some(sound) = self.library.sound_mut(sound_id) else {
            return false;
        };
        if let Some(title) = edit.title {
            sound.title = title;
        }
        if let Some(url) = edit.url {
            sound.url = url;
        }
        if let Some(looping) = edit.looping {
            sound.looping = looping;
        }
        if let Some(volume) = edit.volume.filter(|volume| volume.is_finite()) {
            sound.volume = volume.clamp(0.0, 1.0);
        }
        if let Some(fade_in) = edit.fade_in {
            sound.fade_in = fade_in;
        }
        if let Some(fade_out) = edit.fade_out {
            sound.fade_out = fade_out;
        }
        let updated = sound.clone();
        self.sync_active_sound(&updated);
        self.dirty.library = true;
        true
    }

    /// Deletes a sound, fading it out if it is playing.
    pub fn remove_sound(&mut self, soundboard_id: &str, sound_id: &str) -> bool {
        self.stop_sound(sound_id);
        let removed = self.library.remove_sound(soundboard_id, sound_id).is_some();
        self.dirty.library |= removed;
        removed
    }

    pub fn move_sound(&mut self, soundboard_id: &str, active: &str, over: &str) -> bool {
        let moved = self.library.move_sound(soundboard_id, active, over);
        self.dirty.library |= moved;
        moved
    }
}
