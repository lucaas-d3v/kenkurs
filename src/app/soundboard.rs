//! Soundboard playback: any number of sounds at once, each with its own volume, loop and fades.
//!
//! Port of Kenku FM's `useSoundboardPlayback` hook, `Sound` class and `SoundboardRemote`.

use std::time::Duration;

use crate::audio::{AudioBackend, Bus, PlayRequest, VoiceHandle, VoiceId};
use crate::state::Sound;
use crate::util::source::resolve_local_path;

use super::QUICK_FADE;
use super::controller::Controller;

/// A playing sound.
#[derive(Debug)]
pub struct ActiveSound {
    /// The sound as it was started; title, volume and loop follow later edits.
    pub(super) sound: Sound,
    pub(super) voice: VoiceHandle,
    pub(super) started: bool,
}

impl ActiveSound {
    pub fn sound(&self) -> &Sound {
        &self.sound
    }

    /// False while the engine is still opening the file.
    pub fn is_started(&self) -> bool {
        self.started
    }

    pub fn progress(&self) -> Duration {
        self.voice.position()
    }

    pub fn duration(&self) -> Option<Duration> {
        self.voice.duration()
    }
}

#[derive(Debug, Default)]
pub struct SoundboardPlayer {
    pub(super) active: Vec<ActiveSound>,
}

impl SoundboardPlayer {
    /// Playing sounds, oldest first.
    pub fn sounds(&self) -> &[ActiveSound] {
        &self.active
    }

    pub fn get(&self, sound_id: &str) -> Option<&ActiveSound> {
        self.active.iter().find(|active| active.sound.id == sound_id)
    }

    pub fn is_playing(&self, sound_id: &str) -> bool {
        self.get(sound_id).is_some()
    }

    fn index_of_sound(&self, sound_id: &str) -> Option<usize> {
        self.active.iter().position(|active| active.sound.id == sound_id)
    }

    fn index_of_voice(&self, voice: VoiceId) -> Option<usize> {
        self.active.iter().position(|active| active.voice.id() == voice)
    }
}

fn millis(ms: u32) -> Duration {
    Duration::from_millis(u64::from(ms))
}

impl<B: AudioBackend> Controller<B> {
    /// Plays a sound; playing it again restarts it.
    pub fn play_sound(&mut self, sound_id: &str) -> bool {
        let Some(sound) = self.library.find_sound(sound_id).map(|(_, sound)| sound.clone()) else {
            return false;
        };
        if let Some(index) = self.soundboard.index_of_sound(sound_id) {
            let previous = self.soundboard.active.remove(index);
            self.backend.stop(previous.voice.id(), QUICK_FADE);
        }
        let path = match resolve_local_path(&sound.url) {
            Ok(path) => path,
            Err(error) => {
                self.notify(format!("Unable to play sound: {} ({error})", sound.title));
                return true;
            }
        };
        let voice = self.backend.play(PlayRequest {
            path,
            bus: Bus::Soundboard,
            volume: sound.volume,
            looping: sound.looping,
            fade_in: millis(sound.fade_in),
            fade_out: millis(sound.fade_out),
        });
        self.soundboard.active.push(ActiveSound { sound, voice, started: false });
        true
    }

    /// Plays a random sound of a soundboard (Kenku FM's remote "play" with a soundboard ID).
    pub fn play_soundboard(&mut self, soundboard_id: &str) -> bool {
        let Some(soundboard) = self.library.soundboard(soundboard_id).filter(|board| !board.sounds.is_empty()) else {
            return false;
        };
        let sound_id = soundboard.sounds[fastrand::usize(..soundboard.sounds.len())].id.clone();
        self.play_sound(&sound_id)
    }

    /// Remote-control "play": accepts a sound ID or a soundboard ID.
    pub fn remote_play_soundboard(&mut self, id: &str) -> bool {
        self.play_sound(id) || self.play_soundboard(id)
    }

    /// Stops a sound with its fade-out.
    pub fn stop_sound(&mut self, sound_id: &str) -> bool {
        let Some(index) = self.soundboard.index_of_sound(sound_id) else {
            return false;
        };
        let active = self.soundboard.active.remove(index);
        self.backend.stop(active.voice.id(), millis(active.sound.fade_out));
        true
    }

    /// The soundboard button: stops a playing sound, plays a stopped one.
    pub fn toggle_sound(&mut self, sound_id: &str) -> bool {
        if self.soundboard.is_playing(sound_id) { self.stop_sound(sound_id) } else { self.play_sound(sound_id) }
    }

    pub fn seek_sound(&mut self, sound_id: &str, position: Duration) -> bool {
        let Some(active) = self.soundboard.get(sound_id) else {
            return false;
        };
        let position = active.duration().map_or(position, |duration| position.min(duration));
        self.backend.seek(active.voice.id(), position);
        true
    }

    pub fn stop_all_sounds(&mut self) {
        for active in std::mem::take(&mut self.soundboard.active) {
            self.backend.stop(active.voice.id(), millis(active.sound.fade_out));
        }
    }

    /// Applies live edits (volume, loop, title) to a playing instance of `updated`.
    pub(super) fn sync_active_sound(&mut self, updated: &Sound) {
        let Some(index) = self.soundboard.index_of_sound(&updated.id) else {
            return;
        };
        let active = &mut self.soundboard.active[index];
        if active.sound.volume != updated.volume {
            self.backend.set_volume(active.voice.id(), updated.volume);
        }
        if active.sound.looping != updated.looping {
            self.backend.set_looping(active.voice.id(), updated.looping);
        }
        active.sound.volume = updated.volume;
        active.sound.looping = updated.looping;
        active.sound.title.clone_from(&updated.title);
    }

    pub(super) fn soundboard_started(&mut self, voice: VoiceId) -> bool {
        match self.soundboard.index_of_voice(voice) {
            Some(index) => {
                self.soundboard.active[index].started = true;
                true
            }
            None => false,
        }
    }

    pub(super) fn soundboard_failed(&mut self, voice: VoiceId, error: &str) -> bool {
        let Some(index) = self.soundboard.index_of_voice(voice) else {
            return false;
        };
        let active = self.soundboard.active.remove(index);
        self.notify(format!("Unable to play sound: {} ({error})", active.sound.title));
        true
    }

    pub(super) fn soundboard_ended(&mut self, voice: VoiceId) -> bool {
        match self.soundboard.index_of_voice(voice) {
            Some(index) => {
                self.soundboard.active.remove(index);
                true
            }
            None => false,
        }
    }
}
