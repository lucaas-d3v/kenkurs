//! The user's library: playlists of tracks and soundboards of sounds.
//!
//! Kenku FM stores these normalized (`byId` / `allIds` maps), but every track belongs to exactly
//! one playlist and every sound to one soundboard, so a nested representation is equivalent and
//! simpler. Ordering rules follow Kenku: new playlists/soundboards are appended, new tracks/sounds
//! are inserted at the top, and drag-and-drop moves use `arrayMove` semantics.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::util::id::new_id;
use crate::util::source::{path_to_url, title_from_path};

/// Fade duration Kenku FM pre-fills when a sound is created, in milliseconds.
pub const DEFAULT_SOUND_FADE_MS: u32 = 100;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub id: String,
    pub url: String,
    pub title: String,
}

impl Track {
    pub fn new(title: impl Into<String>, url: impl Into<String>) -> Self {
        Self { id: new_id(), url: url.into(), title: title.into() }
    }

    /// A track for a local file, titled after the file name.
    pub fn from_path(path: &Path) -> Self {
        Self::new(title_from_path(path), path_to_url(path))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sound {
    pub id: String,
    pub url: String,
    pub title: String,
    #[serde(rename = "loop")]
    pub looping: bool,
    /// Linear gain, `0.0..=1.0`.
    pub volume: f32,
    /// Fade-in duration in milliseconds.
    pub fade_in: u32,
    /// Fade-out duration in milliseconds.
    pub fade_out: u32,
}

impl Sound {
    /// A sound with Kenku FM's defaults: one-shot, full volume, 100 ms fades.
    pub fn new(title: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            id: new_id(),
            url: url.into(),
            title: title.into(),
            looping: false,
            volume: 1.0,
            fade_in: DEFAULT_SOUND_FADE_MS,
            fade_out: DEFAULT_SOUND_FADE_MS,
        }
    }

    /// A sound for a local file, titled after the file name.
    pub fn from_path(path: &Path) -> Self {
        Self::new(title_from_path(path), path_to_url(path))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Playlist {
    pub id: String,
    pub title: String,
    /// Artwork key kept for compatibility with Kenku data; may be empty.
    #[serde(default)]
    pub background: String,
    #[serde(default)]
    pub tracks: Vec<Track>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Soundboard {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub background: String,
    #[serde(default)]
    pub sounds: Vec<Sound>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Library {
    pub playlists: Vec<Playlist>,
    pub soundboards: Vec<Soundboard>,
}

/// Anything with a stable string ID.
pub trait Identified {
    fn id(&self) -> &str;
}

macro_rules! identified {
    ($($ty:ty),*) => {$(
        impl Identified for $ty {
            fn id(&self) -> &str {
                &self.id
            }
        }
    )*};
}
identified!(Track, Sound, Playlist, Soundboard);

fn index_of<T: Identified>(items: &[T], id: &str) -> Option<usize> {
    items.iter().position(|item| item.id() == id)
}

/// Moves the element at `from` to index `to` (dnd-kit `arrayMove` semantics).
pub(crate) fn move_item<T>(items: &mut Vec<T>, from: usize, to: usize) {
    if from == to || from >= items.len() || to >= items.len() {
        return;
    }
    let item = items.remove(from);
    items.insert(to, item);
}

/// Moves item `active` to the position currently held by item `over`.
fn move_by_id<T: Identified>(items: &mut Vec<T>, active: &str, over: &str) -> bool {
    match (index_of(items, active), index_of(items, over)) {
        (Some(from), Some(to)) => {
            move_item(items, from, to);
            true
        }
        _ => false,
    }
}

/// Inserts `new` before the existing items, keeping its order (Kenku `unshift(...items)`).
fn insert_front<T>(items: &mut Vec<T>, new: Vec<T>) {
    items.splice(0..0, new);
}

impl Library {
    // ----- playlists ---------------------------------------------------------------------------

    pub fn playlist(&self, id: &str) -> Option<&Playlist> {
        self.playlists.iter().find(|playlist| playlist.id == id)
    }

    pub fn playlist_mut(&mut self, id: &str) -> Option<&mut Playlist> {
        self.playlists.iter_mut().find(|playlist| playlist.id == id)
    }

    /// Appends a new, empty playlist and returns it.
    pub fn add_playlist(&mut self, title: impl Into<String>, background: impl Into<String>) -> &mut Playlist {
        self.playlists.push(Playlist {
            id: new_id(),
            title: title.into(),
            background: background.into(),
            tracks: Vec::new(),
        });
        self.playlists.last_mut().expect("a playlist was just pushed")
    }

    /// Removes a playlist together with its tracks.
    pub fn remove_playlist(&mut self, id: &str) -> Option<Playlist> {
        index_of(&self.playlists, id).map(|index| self.playlists.remove(index))
    }

    pub fn move_playlist(&mut self, active: &str, over: &str) -> bool {
        move_by_id(&mut self.playlists, active, over)
    }

    /// Inserts tracks at the top of a playlist.
    pub fn add_tracks(&mut self, playlist_id: &str, tracks: Vec<Track>) -> bool {
        match self.playlist_mut(playlist_id) {
            Some(playlist) => {
                insert_front(&mut playlist.tracks, tracks);
                true
            }
            None => false,
        }
    }

    pub fn remove_track(&mut self, playlist_id: &str, track_id: &str) -> Option<Track> {
        let playlist = self.playlist_mut(playlist_id)?;
        let index = index_of(&playlist.tracks, track_id)?;
        Some(playlist.tracks.remove(index))
    }

    pub fn move_track(&mut self, playlist_id: &str, active: &str, over: &str) -> bool {
        self.playlist_mut(playlist_id)
            .is_some_and(|playlist| move_by_id(&mut playlist.tracks, active, over))
    }

    /// Finds a track anywhere in the library, with the playlist that owns it.
    pub fn find_track(&self, track_id: &str) -> Option<(&Playlist, &Track)> {
        self.playlists.iter().find_map(|playlist| {
            playlist.tracks.iter().find(|track| track.id == track_id).map(|track| (playlist, track))
        })
    }

    pub fn track_mut(&mut self, track_id: &str) -> Option<&mut Track> {
        self.playlists.iter_mut().flat_map(|playlist| playlist.tracks.iter_mut()).find(|track| track.id == track_id)
    }

    // ----- soundboards -------------------------------------------------------------------------

    pub fn soundboard(&self, id: &str) -> Option<&Soundboard> {
        self.soundboards.iter().find(|soundboard| soundboard.id == id)
    }

    pub fn soundboard_mut(&mut self, id: &str) -> Option<&mut Soundboard> {
        self.soundboards.iter_mut().find(|soundboard| soundboard.id == id)
    }

    /// Appends a new, empty soundboard and returns it.
    pub fn add_soundboard(&mut self, title: impl Into<String>, background: impl Into<String>) -> &mut Soundboard {
        self.soundboards.push(Soundboard {
            id: new_id(),
            title: title.into(),
            background: background.into(),
            sounds: Vec::new(),
        });
        self.soundboards.last_mut().expect("a soundboard was just pushed")
    }

    /// Removes a soundboard together with its sounds.
    pub fn remove_soundboard(&mut self, id: &str) -> Option<Soundboard> {
        index_of(&self.soundboards, id).map(|index| self.soundboards.remove(index))
    }

    pub fn move_soundboard(&mut self, active: &str, over: &str) -> bool {
        move_by_id(&mut self.soundboards, active, over)
    }

    /// Inserts sounds at the top of a soundboard.
    pub fn add_sounds(&mut self, soundboard_id: &str, sounds: Vec<Sound>) -> bool {
        match self.soundboard_mut(soundboard_id) {
            Some(soundboard) => {
                insert_front(&mut soundboard.sounds, sounds);
                true
            }
            None => false,
        }
    }

    pub fn remove_sound(&mut self, soundboard_id: &str, sound_id: &str) -> Option<Sound> {
        let soundboard = self.soundboard_mut(soundboard_id)?;
        let index = index_of(&soundboard.sounds, sound_id)?;
        Some(soundboard.sounds.remove(index))
    }

    pub fn move_sound(&mut self, soundboard_id: &str, active: &str, over: &str) -> bool {
        self.soundboard_mut(soundboard_id)
            .is_some_and(|soundboard| move_by_id(&mut soundboard.sounds, active, over))
    }

    /// Finds a sound anywhere in the library, with the soundboard that owns it.
    pub fn find_sound(&self, sound_id: &str) -> Option<(&Soundboard, &Sound)> {
        self.soundboards.iter().find_map(|soundboard| {
            soundboard.sounds.iter().find(|sound| sound.id == sound_id).map(|sound| (soundboard, sound))
        })
    }

    pub fn sound_mut(&mut self, sound_id: &str) -> Option<&mut Sound> {
        self.soundboards.iter_mut().flat_map(|soundboard| soundboard.sounds.iter_mut()).find(|sound| sound.id == sound_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids<T: Identified>(items: &[T]) -> Vec<&str> {
        items.iter().map(Identified::id).collect()
    }

    fn track(id: &str) -> Track {
        Track { id: id.into(), url: format!("/{id}.mp3"), title: id.to_uppercase() }
    }

    #[test]
    fn move_item_matches_array_move() {
        let mut items = vec!['a', 'b', 'c', 'd'];
        move_item(&mut items, 0, 2);
        assert_eq!(items, ['b', 'c', 'a', 'd']);
        move_item(&mut items, 3, 0);
        assert_eq!(items, ['d', 'b', 'c', 'a']);
        move_item(&mut items, 1, 9); // out of range: ignored
        assert_eq!(items, ['d', 'b', 'c', 'a']);
    }

    #[test]
    fn playlist_crud() {
        let mut library = Library::default();
        let first = library.add_playlist("Combat", "bg").id.clone();
        let second = library.add_playlist("Tavern", "").id.clone();
        assert_eq!(ids(&library.playlists), [first.as_str(), second.as_str()]);

        assert!(library.add_tracks(&first, vec![track("a"), track("b")]));
        assert!(library.add_tracks(&first, vec![track("c")]));
        // New tracks go to the top, keeping their relative order.
        assert_eq!(ids(&library.playlist(&first).unwrap().tracks), ["c", "a", "b"]);

        assert!(library.move_track(&first, "c", "b"));
        assert_eq!(ids(&library.playlist(&first).unwrap().tracks), ["a", "b", "c"]);

        let (owner, found) = library.find_track("b").unwrap();
        assert_eq!((owner.id.as_str(), found.title.as_str()), (first.as_str(), "B"));
        library.track_mut("b").unwrap().title = "Renamed".into();
        assert_eq!(library.find_track("b").unwrap().1.title, "Renamed");

        assert_eq!(library.remove_track(&first, "a").map(|t| t.id), Some("a".into()));
        assert!(library.remove_track(&first, "a").is_none());

        assert!(library.move_playlist(&second, &first));
        assert_eq!(ids(&library.playlists), [second.as_str(), first.as_str()]);

        let removed = library.remove_playlist(&first).unwrap();
        assert_eq!(removed.tracks.len(), 2);
        assert!(library.find_track("b").is_none());
        assert!(!library.add_tracks("missing", vec![track("x")]));
    }

    #[test]
    fn soundboard_crud() {
        let mut library = Library::default();
        let board = library.add_soundboard("SFX", "").id.clone();
        let sound = Sound::new("Door", "/door.wav");
        let sound_id = sound.id.clone();
        assert!(library.add_sounds(&board, vec![sound]));
        assert_eq!(library.find_sound(&sound_id).unwrap().0.id, board);
        library.sound_mut(&sound_id).unwrap().looping = true;
        assert!(library.find_sound(&sound_id).unwrap().1.looping);
        assert!(library.remove_sound(&board, &sound_id).is_some());
        assert!(library.remove_soundboard(&board).is_some());
        assert!(library.soundboards.is_empty());
    }

    #[test]
    fn json_uses_kenku_field_names() {
        let sound = Sound { id: "s".into(), url: "u".into(), title: "t".into(), looping: true, volume: 0.5, fade_in: 10, fade_out: 20 };
        let json = serde_json::to_value(&sound).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"id": "s", "url": "u", "title": "t", "loop": true, "volume": 0.5, "fadeIn": 10, "fadeOut": 20})
        );

        let mut library = Library::default();
        library.add_playlist("P", "").tracks.push(track("a"));
        library.add_soundboard("S", "").sounds.push(sound);
        let text = serde_json::to_string(&library).unwrap();
        assert_eq!(serde_json::from_str::<Library>(&text).unwrap(), library);
        // Missing collections default to empty.
        assert_eq!(serde_json::from_str::<Library>("{}").unwrap(), Library::default());
    }

    #[test]
    fn from_path_builds_url_and_title() {
        let track = Track::from_path(Path::new("/music/epic_battle.mp3"));
        assert_eq!(track.title, "Epic Battle");
        assert_eq!(track.url, "file:///music/epic_battle.mp3");
        let sound = Sound::from_path(Path::new("/sfx/door creak.wav"));
        assert_eq!((sound.fade_in, sound.fade_out, sound.looping), (100, 100, false));
    }
}
