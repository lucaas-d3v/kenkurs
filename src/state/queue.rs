//! Playlist play queue with shuffle and repeat.
//!
//! Port of Kenku FM's `playlistPlaybackSlice` queue reducers and of the next/previous/track-end
//! navigation in `usePlaylistPlayback`. Two Kenku bugs are fixed along the way: the current track
//! is stored as an index into `tracks` in both shuffle modes (Kenku mixed both meanings, which
//! broke track removal with shuffle off), and reordering a playlist keeps the queue in sync in
//! shuffle mode too.

use serde::{Deserialize, Serialize};

use super::library::move_item;

/// Repeat mode, serialized like Kenku FM (`"off"`, `"track"`, `"playlist"`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Repeat {
    Off,
    Track,
    #[default]
    Playlist,
}

impl Repeat {
    /// Next mode of the player's repeat button: playlist → track → off → playlist.
    pub fn cycle(self) -> Self {
        match self {
            Self::Playlist => Self::Track,
            Self::Track => Self::Off,
            Self::Off => Self::Playlist,
        }
    }
}

/// What the player should do after a navigation request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Play the track at this index of [`Queue::tracks`].
    Play(usize),
    /// Restart the current track.
    Restart,
    /// Stop: the queue ran out and repeat is off.
    Stop,
}

/// Outcome of [`Queue::remove`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Removed {
    NotFound,
    /// Another track was removed; the queue stays valid.
    Other,
    /// The current track was removed. The queue is left untouched: the player stops and drops it.
    Current,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Queue {
    playlist_id: String,
    /// Track IDs in playlist order, kept in sync with playlist edits.
    tracks: Vec<String>,
    /// Shuffled play order: a permutation of indices into `tracks`.
    order: Vec<usize>,
    /// Index into `tracks` of the current track.
    current: usize,
}

impl Queue {
    /// Starts a queue on `start` (Kenku `startQueue`). The shuffled order always begins with the
    /// starting track, so turning shuffle on later continues naturally from it.
    /// Returns `None` if `start` is not one of `tracks`.
    pub fn new(playlist_id: impl Into<String>, tracks: Vec<String>, start: &str) -> Option<Self> {
        let current = tracks.iter().position(|id| id == start)?;
        let mut queue = Self { playlist_id: playlist_id.into(), tracks, order: Vec::new(), current };
        queue.reshuffle();
        Some(queue)
    }

    pub fn playlist_id(&self) -> &str {
        &self.playlist_id
    }

    pub fn tracks(&self) -> &[String] {
        &self.tracks
    }

    /// Shuffled play order (indices into [`tracks`](Self::tracks)).
    pub fn order(&self) -> &[usize] {
        &self.order
    }

    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    pub fn current_index(&self) -> usize {
        self.current
    }

    pub fn current_track(&self) -> &str {
        &self.tracks[self.current]
    }

    /// Makes `index` the current track once the player has started it.
    pub fn set_current(&mut self, index: usize) {
        if index < self.tracks.len() {
            self.current = index;
        }
    }

    /// Where playback goes after the current track: the "next" button and the end of a track.
    pub fn next_step(&self, shuffle: bool, repeat: Repeat) -> Step {
        if repeat == Repeat::Track {
            return Step::Restart;
        }
        let mut position = self.position(shuffle) + 1;
        if position >= self.tracks.len() {
            if repeat == Repeat::Off {
                return Step::Stop;
            }
            position = 0;
        }
        self.step_to(position, shuffle)
    }

    /// The "previous" button. Away from the start of a track it restarts the track; near the
    /// start (`at_track_start`, Kenku: under 5 s) it goes to the previous one.
    pub fn previous_step(&self, shuffle: bool, repeat: Repeat, at_track_start: bool) -> Step {
        if repeat == Repeat::Track || !at_track_start {
            return Step::Restart;
        }
        let position = match self.position(shuffle).checked_sub(1) {
            Some(position) => position,
            None if repeat == Repeat::Off => return Step::Stop,
            None => self.tracks.len() - 1,
        };
        self.step_to(position, shuffle)
    }

    /// Restarts the shuffled order from the current track (Kenku `shuffle(true)`).
    pub fn reshuffle(&mut self) {
        let current = self.current;
        let mut rest: Vec<usize> = (0..self.tracks.len()).filter(|&index| index != current).collect();
        fastrand::shuffle(&mut rest);
        self.order.clear();
        self.order.push(current);
        self.order.extend(rest);
    }

    /// Mirrors tracks inserted at the top of the playlist (Kenku `addTracksToQueueIfNeeded`).
    /// They are placed, shuffled among themselves, at the front of the play order.
    pub fn insert_front(&mut self, ids: &[String]) {
        let added = ids.len();
        if added == 0 {
            return;
        }
        let mut order: Vec<usize> = (0..added).collect();
        fastrand::shuffle(&mut order);
        order.extend(self.order.iter().map(|&index| index + added));
        self.order = order;
        self.tracks.splice(0..0, ids.iter().cloned());
        self.current += added;
    }

    /// Mirrors a track removed from the playlist (Kenku `removeTrackFromQueue`).
    pub fn remove(&mut self, track_id: &str) -> Removed {
        let Some(index) = self.tracks.iter().position(|id| id == track_id) else {
            return Removed::NotFound;
        };
        if index == self.current {
            return Removed::Current;
        }
        self.tracks.remove(index);
        self.order.retain(|&entry| entry != index);
        for entry in &mut self.order {
            if *entry > index {
                *entry -= 1;
            }
        }
        if self.current > index {
            self.current -= 1;
        }
        Removed::Other
    }

    /// Mirrors a drag-and-drop reorder of the playlist (Kenku `moveQueueIfNeeded`). The shuffled
    /// order is preserved by identity.
    pub fn move_track(&mut self, active: &str, over: &str) {
        let find = |id: &str| self.tracks.iter().position(|track| track == id);
        let (Some(from), Some(to)) = (find(active), find(over)) else {
            return;
        };
        if from == to {
            return;
        }
        move_item(&mut self.tracks, from, to);
        let remap = |index: usize| {
            if index == from {
                to
            } else if from < to && (from + 1..=to).contains(&index) {
                index - 1
            } else if to < from && (to..from).contains(&index) {
                index + 1
            } else {
                index
            }
        };
        for entry in &mut self.order {
            *entry = remap(*entry);
        }
        self.current = remap(self.current);
    }

    /// Position of the current track in play order.
    fn position(&self, shuffle: bool) -> usize {
        if shuffle {
            self.order.iter().position(|&index| index == self.current).unwrap_or(0)
        } else {
            self.current
        }
    }

    fn step_to(&self, position: usize, shuffle: bool) -> Step {
        let index = if shuffle { self.order[position] } else { position };
        if index == self.current { Step::Restart } else { Step::Play(index) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue(len: usize, start: usize) -> Queue {
        let tracks = (0..len).map(|i| format!("t{i}")).collect();
        Queue::new("p", tracks, &format!("t{start}")).unwrap()
    }

    fn assert_permutation(queue: &Queue) {
        let mut sorted = queue.order().to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..queue.len()).collect::<Vec<_>>());
    }

    #[test]
    fn new_queue_starts_shuffled_order_with_the_chosen_track() {
        let q = queue(10, 4);
        assert_eq!(q.current_index(), 4);
        assert_eq!(q.current_track(), "t4");
        assert_eq!(q.order()[0], 4);
        assert_permutation(&q);
        assert!(Queue::new("p", vec!["a".into()], "zzz").is_none());
    }

    #[test]
    fn next_in_order_with_repeat_modes() {
        let mut q = queue(3, 0);
        assert_eq!(q.next_step(false, Repeat::Playlist), Step::Play(1));
        q.set_current(2);
        assert_eq!(q.next_step(false, Repeat::Playlist), Step::Play(0)); // wraps
        assert_eq!(q.next_step(false, Repeat::Off), Step::Stop);
        assert_eq!(q.next_step(false, Repeat::Track), Step::Restart);
        // A one-track playlist restarts instead of "playing" itself.
        assert_eq!(queue(1, 0).next_step(false, Repeat::Playlist), Step::Restart);
    }

    #[test]
    fn next_follows_shuffled_order() {
        let mut q = queue(6, 2);
        let order = q.order().to_vec();
        for window in order.windows(2) {
            assert_eq!(q.next_step(true, Repeat::Off), Step::Play(window[1]));
            q.set_current(window[1]);
        }
        assert_eq!(q.next_step(true, Repeat::Off), Step::Stop);
        assert_eq!(q.next_step(true, Repeat::Playlist), Step::Play(order[0]));
    }

    #[test]
    fn previous_restarts_unless_near_the_start() {
        let mut q = queue(3, 1);
        assert_eq!(q.previous_step(false, Repeat::Playlist, false), Step::Restart);
        assert_eq!(q.previous_step(false, Repeat::Playlist, true), Step::Play(0));
        q.set_current(0);
        assert_eq!(q.previous_step(false, Repeat::Off, true), Step::Stop);
        assert_eq!(q.previous_step(false, Repeat::Playlist, true), Step::Play(2)); // wraps
        assert_eq!(q.previous_step(false, Repeat::Track, true), Step::Restart);
    }

    #[test]
    fn reshuffle_keeps_current_first() {
        let mut q = queue(8, 0);
        q.set_current(5);
        q.reshuffle();
        assert_eq!(q.order()[0], 5);
        assert_eq!(q.current_index(), 5);
        assert_permutation(&q);
    }

    #[test]
    fn insert_front_shifts_indices_and_keeps_the_current_track() {
        let mut q = queue(3, 1);
        let before: Vec<String> = q.order().iter().map(|&i| q.tracks()[i].clone()).collect();
        q.insert_front(&["n0".into(), "n1".into()]);
        assert_eq!(q.tracks(), ["n0", "n1", "t0", "t1", "t2"]);
        assert_eq!(q.current_track(), "t1");
        assert_permutation(&q);
        // New tracks come first in play order; the old order is preserved after them.
        let after: Vec<String> = q.order().iter().map(|&i| q.tracks()[i].clone()).collect();
        assert!(after[..2].iter().all(|id| id.starts_with('n')));
        assert_eq!(after[2..], before[..]);
    }

    #[test]
    fn remove_other_track_remaps_indices() {
        let mut q = queue(4, 2);
        let current = q.current_track().to_owned();
        let order_ids: Vec<String> = q.order().iter().map(|&i| q.tracks()[i].clone()).filter(|id| id != "t0").collect();
        assert_eq!(q.remove("t0"), Removed::Other);
        assert_eq!(q.tracks(), ["t1", "t2", "t3"]);
        assert_eq!(q.current_track(), current);
        assert_permutation(&q);
        let remaining: Vec<String> = q.order().iter().map(|&i| q.tracks()[i].clone()).collect();
        assert_eq!(remaining, order_ids);
        assert_eq!(q.remove("t2"), Removed::Current);
        assert_eq!(q.remove("nope"), Removed::NotFound);
    }

    #[test]
    fn move_track_keeps_identity_in_both_modes() {
        let mut q = queue(4, 0);
        let order_ids: Vec<String> = q.order().iter().map(|&i| q.tracks()[i].clone()).collect();
        q.move_track("t0", "t2");
        assert_eq!(q.tracks(), ["t1", "t2", "t0", "t3"]);
        assert_eq!(q.current_track(), "t0");
        let after: Vec<String> = q.order().iter().map(|&i| q.tracks()[i].clone()).collect();
        assert_eq!(after, order_ids);
        q.move_track("t3", "t1");
        assert_eq!(q.tracks(), ["t3", "t1", "t2", "t0"]);
        assert_eq!(q.current_track(), "t0");
        assert_permutation(&q);
        // Playlist order drives navigation with shuffle off.
        assert_eq!(q.next_step(false, Repeat::Playlist), Step::Play(0));
    }

    #[test]
    fn repeat_serializes_like_kenku() {
        assert_eq!(serde_json::to_string(&Repeat::Playlist).unwrap(), "\"playlist\"");
        assert_eq!(serde_json::from_str::<Repeat>("\"off\"").unwrap(), Repeat::Off);
        assert_eq!(Repeat::Playlist.cycle().cycle().cycle(), Repeat::Playlist);
    }
}
