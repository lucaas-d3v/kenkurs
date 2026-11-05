//! Playlist playback: one track at a time, crossfaded, driven by the play queue.
//!
//! Port of Kenku FM's `usePlaylistPlayback` hook and `PlaylistRemote` handlers.

use std::time::Duration;

use crate::audio::{AudioBackend, Bus, PlayRequest, VoiceHandle, VoiceId};
use crate::state::{Queue, Repeat, Step, Track};
use crate::util::source::resolve_local_path;

use super::QUICK_FADE;
use super::controller::Controller;

/// Crossfade between consecutive tracks (Kenku FM: 1000 ms).
pub const TRACK_CROSSFADE: Duration = Duration::from_millis(1000);
/// "Previous" restarts the current track unless it is closer than this to its start.
pub const PREVIOUS_RESTART_THRESHOLD: Duration = Duration::from_secs(5);

/// Playlist playback state.
#[derive(Debug, Default)]
pub struct PlaylistPlayer {
    pub(super) queue: Option<Queue>,
    pub(super) playing: bool,
    /// The loaded track. Stays set when playback stops at the end of the queue.
    pub(super) track: Option<Track>,
    /// Voice of `track`; `None` once it has played to its end.
    pub(super) voice: Option<VoiceHandle>,
    /// Next track, still being opened by the engine.
    pub(super) loading: Option<(VoiceHandle, Track)>,
    /// Length of `track`, kept after its voice is gone.
    pub(super) track_duration: Option<Duration>,
}

impl PlaylistPlayer {
    pub fn queue(&self) -> Option<&Queue> {
        self.queue.as_ref()
    }

    /// The loaded track (playing, paused or stopped).
    pub fn track(&self) -> Option<&Track> {
        self.track.as_ref()
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    pub fn is_loading(&self) -> bool {
        self.loading.is_some()
    }

    pub fn progress(&self) -> Duration {
        self.voice.as_ref().map_or(Duration::ZERO, VoiceHandle::position)
    }

    pub fn duration(&self) -> Option<Duration> {
        self.voice.as_ref().and_then(VoiceHandle::duration).or(self.track_duration)
    }

    fn loading_id(&self) -> Option<VoiceId> {
        self.loading.as_ref().map(|(voice, _)| voice.id())
    }

    fn voice_id(&self) -> Option<VoiceId> {
        self.voice.as_ref().map(VoiceHandle::id)
    }
}

impl<B: AudioBackend> Controller<B> {
    /// Plays a track, starting a queue over its playlist (Kenku FM: clicking a track).
    pub fn play_track(&mut self, track_id: &str) -> bool {
        let Some((playlist, track)) = self.library.find_track(track_id) else {
            return false;
        };
        let tracks = playlist.tracks.iter().map(|track| track.id.clone()).collect();
        self.playlist.queue = Queue::new(playlist.id.clone(), tracks, track_id);
        let track = track.clone();
        self.start_track(track, TRACK_CROSSFADE);
        true
    }

    /// Plays a playlist from its first track, or a random one when shuffling.
    pub fn play_playlist(&mut self, playlist_id: &str) -> bool {
        let Some(playlist) = self.library.playlist(playlist_id).filter(|playlist| !playlist.tracks.is_empty()) else {
            return false;
        };
        let index = if self.settings.playback.shuffle { fastrand::usize(..playlist.tracks.len()) } else { 0 };
        let track = playlist.tracks[index].clone();
        let tracks = playlist.tracks.iter().map(|track| track.id.clone()).collect();
        self.playlist.queue = Queue::new(playlist.id.clone(), tracks, &track.id);
        self.start_track(track, TRACK_CROSSFADE);
        true
    }

    /// Remote-control "play": accepts a track ID or a playlist ID.
    pub fn remote_play_playlist(&mut self, id: &str) -> bool {
        self.play_track(id) || self.play_playlist(id)
    }

    /// Play/pause. Resuming a track that already played to its end starts it again.
    pub fn set_playlist_playing(&mut self, playing: bool) {
        let Some(track) = self.playlist.track.clone() else {
            return;
        };
        self.playlist.playing = playing;
        match (self.playlist.voice_id(), playing) {
            (Some(voice), true) => self.backend.resume(voice),
            (Some(voice), false) => self.backend.pause(voice),
            (None, true) => self.start_track(track, Duration::ZERO),
            (None, false) => {}
        }
    }

    pub fn toggle_playlist_playing(&mut self) {
        self.set_playlist_playing(!self.playlist.playing);
    }

    pub fn playlist_next(&mut self) {
        if self.playlist.track.is_none() {
            return;
        }
        let prefs = self.settings.playback;
        let step = self.playlist.queue.as_ref().map_or(Step::Stop, |queue| queue.next_step(prefs.shuffle, prefs.repeat));
        self.apply_step(step);
    }

    pub fn playlist_previous(&mut self) {
        if self.playlist.track.is_none() {
            return;
        }
        let prefs = self.settings.playback;
        let at_start = self.playlist.progress() < PREVIOUS_RESTART_THRESHOLD;
        let step = self
            .playlist
            .queue
            .as_ref()
            .map_or(Step::Stop, |queue| queue.previous_step(prefs.shuffle, prefs.repeat, at_start));
        self.apply_step(step);
    }

    /// Kenku FM `stop()`: pause and rewind, keeping the track loaded.
    pub fn stop_playlist(&mut self) {
        self.playlist.playing = false;
        if let Some(voice) = self.playlist.voice_id() {
            self.backend.pause(voice);
            self.backend.seek(voice, Duration::ZERO);
        }
    }

    pub fn seek_playlist(&mut self, position: Duration) {
        let Some(voice) = &self.playlist.voice else {
            return;
        };
        let position = voice.duration().map_or(position, |duration| position.min(duration));
        self.backend.seek(voice.id(), position);
    }

    pub fn set_playlist_volume(&mut self, volume: f32) {
        if volume.is_finite() {
            self.settings.playback.volume = volume.clamp(0.0, 1.0);
            self.dirty.settings = true;
            self.apply_playlist_gain();
        }
    }

    pub fn set_playlist_muted(&mut self, muted: bool) {
        self.settings.playback.muted = muted;
        self.dirty.settings = true;
        self.apply_playlist_gain();
    }

    /// Turning shuffle on reshuffles the queue, continuing from the current track.
    pub fn set_shuffle(&mut self, shuffle: bool) {
        if shuffle {
            if let Some(queue) = self.playlist.queue.as_mut() {
                queue.reshuffle();
            }
        }
        self.settings.playback.shuffle = shuffle;
        self.dirty.settings = true;
    }

    /// Repeat-track loops gaplessly inside the engine.
    pub fn set_repeat(&mut self, repeat: Repeat) {
        self.settings.playback.repeat = repeat;
        self.dirty.settings = true;
        self.sync_track_looping();
    }

    pub(super) fn apply_playlist_gain(&self) {
        self.backend.set_bus_volume(Bus::Playlist, self.settings.playback.effective_volume());
    }

    pub(super) fn sync_track_looping(&self) {
        let looping = self.settings.playback.repeat == Repeat::Track;
        let loading = self.playlist.loading.as_ref().map(|(voice, _)| voice);
        for voice in self.playlist.voice.iter().chain(loading) {
            self.backend.set_looping(voice.id(), looping);
        }
    }

    /// Stops everything and forgets the queue.
    pub(super) fn clear_playlist_playback(&mut self) {
        if let Some((voice, _)) = self.playlist.loading.take() {
            self.backend.stop(voice.id(), Duration::ZERO);
        }
        if let Some(voice) = self.playlist.voice.take() {
            self.backend.stop(voice.id(), QUICK_FADE);
        }
        self.playlist.track = None;
        self.playlist.track_duration = None;
        self.playlist.queue = None;
        self.playlist.playing = false;
    }

    /// The loading track became audible: crossfade from the previous one.
    pub(super) fn playlist_started(&mut self, id: VoiceId) -> bool {
        if self.playlist.loading_id() != Some(id) {
            return false;
        }
        let (voice, track) = self.playlist.loading.take().expect("checked above");
        self.playlist.track_duration = voice.duration();
        if let Some(previous) = self.playlist.voice.replace(voice) {
            self.backend.stop(previous.id(), TRACK_CROSSFADE);
        }
        self.playlist.track = Some(track);
        self.playlist.playing = true;
        true
    }

    pub(super) fn playlist_failed(&mut self, id: VoiceId, error: &str) -> bool {
        if self.playlist.loading_id() == Some(id) {
            let (_, track) = self.playlist.loading.take().expect("checked above");
            self.fail_track(&track, error);
            return true;
        }
        if self.playlist.voice_id() == Some(id) {
            // Rejected after starting (too many voices).
            self.playlist.voice = None;
            if let Some(track) = self.playlist.track.clone() {
                self.fail_track(&track, error);
            }
            return true;
        }
        false
    }

    /// The current track played to its end: move on like Kenku FM's `end` handler.
    pub(super) fn playlist_ended(&mut self, id: VoiceId) -> bool {
        if self.playlist.voice_id() != Some(id) {
            return false;
        }
        self.playlist.voice = None;
        let prefs = self.settings.playback;
        let step = self.playlist.queue.as_ref().map_or(Step::Stop, |queue| queue.next_step(prefs.shuffle, prefs.repeat));
        self.apply_step(step);
        true
    }

    fn apply_step(&mut self, step: Step) {
        match step {
            Step::Stop => self.stop_playlist(),
            Step::Restart => match self.playlist.voice_id() {
                Some(voice) => self.backend.seek(voice, Duration::ZERO),
                None => {
                    if let Some(track) = self.playlist.track.clone() {
                        self.start_track(track, Duration::ZERO);
                    }
                }
            },
            Step::Play(index) => self.play_queue_index(index),
        }
    }

    fn play_queue_index(&mut self, index: usize) {
        let Some(queue) = self.playlist.queue.as_mut() else {
            return;
        };
        let Some(track) = queue
            .tracks()
            .get(index)
            .and_then(|track_id| self.library.find_track(track_id))
            .map(|(_, track)| track.clone())
        else {
            return;
        };
        queue.set_current(index);
        self.start_track(track, TRACK_CROSSFADE);
    }

    /// Opens `track` on a new voice; it replaces the current one once the engine starts it.
    fn start_track(&mut self, track: Track, fade_in: Duration) {
        if let Some((superseded, _)) = self.playlist.loading.take() {
            self.backend.stop(superseded.id(), Duration::ZERO);
        }
        let path = match resolve_local_path(&track.url) {
            Ok(path) => path,
            Err(error) => {
                self.fail_track(&track, &error.to_string());
                return;
            }
        };
        let voice = self.backend.play(PlayRequest {
            path,
            bus: Bus::Playlist,
            volume: 1.0,
            looping: self.settings.playback.repeat == Repeat::Track,
            fade_in,
            fade_out: Duration::ZERO,
        });
        self.playlist.loading = Some((voice, track));
    }

    /// Kenku FM's load-error handling: drop the previous track as well and tell the user.
    fn fail_track(&mut self, track: &Track, reason: &str) {
        if let Some(voice) = self.playlist.voice.take() {
            self.backend.stop(voice.id(), QUICK_FADE);
        }
        self.playlist.track = None;
        self.playlist.track_duration = None;
        self.playlist.playing = false;
        self.notify(format!("Unable to play track: {} ({reason})", track.title));
    }
}
