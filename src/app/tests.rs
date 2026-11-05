//! Controller tests against a recording fake engine.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::time::Duration;

use super::*;
use crate::audio::{AudioBackend, Bus, EngineEvent, PlayRequest, VoiceHandle, VoiceId};
use crate::state::{Library, Playlist, Repeat, Settings, Sound, Soundboard, Track};

#[derive(Clone, Debug, PartialEq)]
enum Call {
    Play { id: u64, path: PathBuf, bus: Bus, volume: f32, looping: bool, fade_in: Duration, fade_out: Duration },
    Stop(u64, Duration),
    Pause(u64),
    Resume(u64),
    Seek(u64, Duration),
    Volume(u64, f32),
    Looping(u64, bool),
    BusVolume(Bus, f32),
}

#[derive(Default)]
struct Fake {
    calls: RefCell<Vec<Call>>,
    voices: RefCell<Vec<VoiceHandle>>,
    next_id: Cell<u64>,
}

impl Fake {
    fn record(&self, call: Call) {
        self.calls.borrow_mut().push(call);
    }
}

impl AudioBackend for Fake {
    fn play(&self, request: PlayRequest) -> VoiceHandle {
        let id = self.next_id.get() + 1;
        self.next_id.set(id);
        let voice = VoiceHandle::new(VoiceId(id), 48_000);
        self.voices.borrow_mut().push(voice.clone());
        self.record(Call::Play {
            id,
            path: request.path,
            bus: request.bus,
            volume: request.volume,
            looping: request.looping,
            fade_in: request.fade_in,
            fade_out: request.fade_out,
        });
        voice
    }
    fn stop(&self, voice: VoiceId, fade: Duration) {
        self.record(Call::Stop(voice.0, fade));
    }
    fn pause(&self, voice: VoiceId) {
        self.record(Call::Pause(voice.0));
    }
    fn resume(&self, voice: VoiceId) {
        self.record(Call::Resume(voice.0));
    }
    fn seek(&self, voice: VoiceId, position: Duration) {
        self.record(Call::Seek(voice.0, position));
    }
    fn set_volume(&self, voice: VoiceId, volume: f32) {
        self.record(Call::Volume(voice.0, volume));
    }
    fn set_looping(&self, voice: VoiceId, looping: bool) {
        self.record(Call::Looping(voice.0, looping));
    }
    fn set_bus_volume(&self, bus: Bus, volume: f32) {
        self.record(Call::BusVolume(bus, volume));
    }
}

fn track(id: &str) -> Track {
    Track { id: id.into(), url: format!("/music/{id}.mp3"), title: id.to_uppercase() }
}

fn sound(id: &str, looping: bool) -> Sound {
    Sound { id: id.into(), url: format!("file:///sfx/{id}.ogg"), title: id.to_uppercase(), looping, volume: 0.8, fade_in: 100, fade_out: 250 }
}

/// Playlist `p` = [t1, t2, t3], playlist `q` = [u1]; soundboard `b` = [s1 (loop), s2].
fn controller() -> Controller<Fake> {
    let library = Library {
        playlists: vec![
            Playlist { id: "p".into(), title: "P".into(), background: String::new(), tracks: vec![track("t1"), track("t2"), track("t3")] },
            Playlist { id: "q".into(), title: "Q".into(), background: String::new(), tracks: vec![track("u1")] },
        ],
        soundboards: vec![Soundboard {
            id: "b".into(),
            title: "B".into(),
            background: String::new(),
            sounds: vec![sound("s1", true), sound("s2", false)],
        }],
    };
    let controller = Controller::new(Fake::default(), library, Settings::default());
    assert_eq!(calls(&controller), [Call::BusVolume(Bus::Playlist, 1.0)]);
    controller
}

fn calls(controller: &Controller<Fake>) -> Vec<Call> {
    std::mem::take(&mut *controller.backend().calls.borrow_mut())
}

fn voice(controller: &Controller<Fake>, id: u64) -> VoiceHandle {
    controller.backend().voices.borrow().iter().find(|voice| voice.id() == VoiceId(id)).unwrap().clone()
}

/// Simulates the engine starting a voice of a 3-minute file.
fn started(controller: &mut Controller<Fake>, id: u64) {
    let duration = Duration::from_secs(180);
    let known = controller.backend().voices.borrow().iter().find(|voice| voice.id() == VoiceId(id)).cloned();
    if let Some(voice) = known {
        voice.set_progress_for_test(Duration::ZERO, Some(duration));
    }
    controller.handle_engine_event(&EngineEvent::Started { voice: VoiceId(id), duration: Some(duration) });
}

fn ended(controller: &mut Controller<Fake>, id: u64) {
    controller.handle_engine_event(&EngineEvent::Ended { voice: VoiceId(id) });
}

fn played_path(call: &Call) -> &str {
    match call {
        Call::Play { path, .. } => path.to_str().unwrap(),
        other => panic!("expected a Play call, got {other:?}"),
    }
}

fn playing_title(controller: &Controller<Fake>) -> Option<&str> {
    controller.playlist().track().map(|track| track.title.as_str())
}

#[test]
fn playing_a_track_crossfades_from_the_previous_one() {
    let mut c = controller();
    assert!(c.play_track("t1"));
    assert_eq!(
        calls(&c),
        [Call::Play {
            id: 1,
            path: "/music/t1.mp3".into(),
            bus: Bus::Playlist,
            volume: 1.0,
            looping: false,
            fade_in: TRACK_CROSSFADE,
            fade_out: Duration::ZERO
        }]
    );
    assert!(c.playlist().is_loading());
    assert_eq!(playing_title(&c), None);

    started(&mut c, 1);
    assert_eq!((playing_title(&c), c.playlist().is_playing()), (Some("T1"), true));
    assert_eq!(c.playlist().queue().unwrap().current_track(), "t1");

    assert!(c.play_track("t2"));
    started(&mut c, 2);
    let log = calls(&c);
    assert_eq!(played_path(&log[0]), "/music/t2.mp3");
    assert_eq!(log[1], Call::Stop(1, TRACK_CROSSFADE));
    assert_eq!(playing_title(&c), Some("T2"));
}

#[test]
fn a_newer_request_cancels_a_track_still_loading() {
    let mut c = controller();
    c.play_track("t1");
    c.play_track("t3");
    let log = calls(&c);
    assert_eq!(log[1], Call::Stop(1, Duration::ZERO));
    assert_eq!(played_path(&log[2]), "/music/t3.mp3");
    started(&mut c, 1); // stale: ignored
    assert_eq!(playing_title(&c), None);
    started(&mut c, 2);
    assert_eq!(playing_title(&c), Some("T3"));
}

#[test]
fn track_ends_advance_and_stop_at_the_end_with_repeat_off() {
    let mut c = controller();
    c.set_repeat(Repeat::Off);
    c.play_track("t1");
    started(&mut c, 1);
    calls(&c);
    for (id, next) in [(1, "/music/t2.mp3"), (2, "/music/t3.mp3")] {
        ended(&mut c, id);
        assert_eq!(played_path(&calls(&c)[0]), next);
        started(&mut c, id + 1);
    }
    ended(&mut c, 3);
    assert!(calls(&c).is_empty());
    assert_eq!((playing_title(&c), c.playlist().is_playing()), (Some("T3"), false));
    // The finished track still reports its length, at position zero.
    assert_eq!((c.playlist().progress(), c.playlist().duration()), (Duration::ZERO, Some(Duration::from_secs(180))));

    // Pressing play again restarts the finished track.
    c.set_playlist_playing(true);
    let log = calls(&c);
    assert!(matches!(&log[0], Call::Play { fade_in, .. } if *fade_in == Duration::ZERO));
    assert_eq!(played_path(&log[0]), "/music/t3.mp3");
}

#[test]
fn repeat_playlist_wraps_around() {
    let mut c = controller();
    c.play_track("t3");
    started(&mut c, 1);
    ended(&mut c, 1);
    assert_eq!(played_path(&calls(&c)[1]), "/music/t1.mp3");
}

#[test]
fn repeat_track_loops_inside_the_engine() {
    let mut c = controller();
    c.set_repeat(Repeat::Track);
    c.play_track("t1");
    assert!(matches!(calls(&c)[0], Call::Play { looping: true, .. }));
    started(&mut c, 1);
    c.playlist_next();
    assert_eq!(calls(&c), [Call::Seek(1, Duration::ZERO)]);
    c.set_repeat(Repeat::Playlist);
    assert_eq!(calls(&c), [Call::Looping(1, false)]);
}

#[test]
fn previous_restarts_unless_near_the_start() {
    let mut c = controller();
    c.play_track("t2");
    started(&mut c, 1);
    calls(&c);
    voice(&c, 1).set_progress_for_test(Duration::from_secs(30), Some(Duration::from_secs(180)));
    c.playlist_previous();
    assert_eq!(calls(&c), [Call::Seek(1, Duration::ZERO)]);
    voice(&c, 1).set_progress_for_test(Duration::from_secs(2), Some(Duration::from_secs(180)));
    c.playlist_previous();
    assert_eq!(played_path(&calls(&c)[0]), "/music/t1.mp3");
    assert_eq!(c.playlist().queue().unwrap().current_track(), "t1");
}

#[test]
fn load_failures_stop_playback_and_notify() {
    let mut c = controller();
    c.play_track("t1");
    started(&mut c, 1);
    c.play_track("t2");
    calls(&c);
    c.handle_engine_event(&EngineEvent::Failed { voice: VoiceId(2), error: "corrupt".into() });
    assert_eq!(calls(&c), [Call::Stop(1, QUICK_FADE)]);
    assert_eq!((playing_title(&c), c.playlist().is_playing()), (None, false));
    let notices = c.take_notices();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].message.contains("T2") && notices[0].message.contains("corrupt"));
    assert!(c.take_notices().is_empty());
}

#[test]
fn web_urls_fail_without_reaching_the_engine() {
    let mut c = controller();
    c.edit_track("t1", TrackEdit { url: Some("https://example.com/a.mp3".into()), ..TrackEdit::default() });
    c.play_track("t1");
    assert!(calls(&c).is_empty());
    assert!(c.take_notices()[0].message.contains("web URLs"));
}

#[test]
fn transport_and_volume_controls() {
    let mut c = controller();
    c.set_playlist_playing(true); // nothing loaded: ignored
    assert!(calls(&c).is_empty());
    c.play_track("t1");
    started(&mut c, 1);
    calls(&c);
    c.toggle_playlist_playing();
    c.toggle_playlist_playing();
    c.seek_playlist(Duration::from_secs(500)); // clamped to the duration
    c.set_playlist_volume(0.5);
    c.set_playlist_muted(true);
    c.set_playlist_volume(f32::NAN); // ignored
    c.stop_playlist();
    assert_eq!(
        calls(&c),
        [
            Call::Pause(1),
            Call::Resume(1),
            Call::Seek(1, Duration::from_secs(180)),
            Call::BusVolume(Bus::Playlist, 0.5),
            Call::BusVolume(Bus::Playlist, 0.0),
            Call::Pause(1),
            Call::Seek(1, Duration::ZERO),
        ]
    );
    assert!(c.take_dirty().settings);
    assert_eq!(c.settings().playback.volume, 0.5);
}

#[test]
fn shuffle_continues_from_the_current_track() {
    let mut c = controller();
    c.play_track("t2");
    started(&mut c, 1);
    c.set_shuffle(true);
    let queue = c.playlist().queue().unwrap();
    assert_eq!(queue.order()[0], queue.current_index());
    assert_eq!(queue.current_track(), "t2");
}

#[test]
fn library_edits_keep_the_queue_in_sync() {
    let mut c = controller();
    c.play_track("t2");
    started(&mut c, 1);
    assert!(c.add_tracks("p", vec![track("n1")]));
    assert_eq!(c.playlist().queue().unwrap().tracks(), ["n1", "t1", "t2", "t3"]);
    assert!(c.move_track("p", "t3", "n1"));
    assert_eq!(c.playlist().queue().unwrap().tracks(), ["t3", "n1", "t1", "t2"]);
    assert!(c.remove_track("p", "t1"));
    assert_eq!(c.playlist().queue().unwrap().tracks(), ["t3", "n1", "t2"]);
    assert_eq!(c.playlist().queue().unwrap().current_track(), "t2");
    calls(&c);

    // Removing the playing track stops playback.
    assert!(c.remove_track("p", "t2"));
    assert_eq!(calls(&c), [Call::Stop(1, QUICK_FADE)]);
    assert!(c.playlist().queue().is_none() && c.playlist().track().is_none());
    assert_eq!(c.library().playlist("p").unwrap().tracks.len(), 2);
    assert!(c.take_dirty().library);
}

#[test]
fn renaming_the_playing_track_updates_now_playing() {
    let mut c = controller();
    c.play_track("t1");
    started(&mut c, 1);
    c.edit_track("t1", TrackEdit { title: Some("Renamed".into()), ..TrackEdit::default() });
    assert_eq!(playing_title(&c), Some("Renamed"));
}

#[test]
fn removing_the_playing_playlist_stops_it() {
    let mut c = controller();
    c.play_track("u1");
    started(&mut c, 1);
    calls(&c);
    assert!(c.remove_playlist("q"));
    assert_eq!(calls(&c), [Call::Stop(1, QUICK_FADE)]);
    assert!(c.library().find_track("u1").is_none());
    assert!(!c.remove_playlist("q"));
}

#[test]
fn remote_play_accepts_track_or_playlist_ids() {
    let mut c = controller();
    assert!(c.remote_play_playlist("p"));
    assert_eq!(played_path(&calls(&c)[0]), "/music/t1.mp3");
    assert!(c.remote_play_playlist("t3"));
    assert_eq!(played_path(&calls(&c)[1]), "/music/t3.mp3");
    assert!(!c.remote_play_playlist("nope"));
}

#[test]
fn sounds_play_restart_and_stop_with_their_fades() {
    let mut c = controller();
    assert!(c.play_sound("s1"));
    assert_eq!(
        calls(&c),
        [Call::Play {
            id: 1,
            path: "/sfx/s1.ogg".into(),
            bus: Bus::Soundboard,
            volume: 0.8,
            looping: true,
            fade_in: Duration::from_millis(100),
            fade_out: Duration::from_millis(250)
        }]
    );
    assert!(!c.soundboard().get("s1").unwrap().is_started());
    started(&mut c, 1);
    assert!(c.soundboard().get("s1").unwrap().is_started());

    // Retriggering restarts it.
    c.play_sound("s1");
    let log = calls(&c);
    assert_eq!(log[0], Call::Stop(1, QUICK_FADE));
    assert!(matches!(log[1], Call::Play { id: 2, .. }));
    assert_eq!(c.soundboard().sounds().len(), 1);

    assert!(c.toggle_sound("s1"));
    assert_eq!(calls(&c), [Call::Stop(2, Duration::from_millis(250))]);
    assert!(!c.soundboard().is_playing("s1"));
    assert!(!c.stop_sound("s1"));
}

#[test]
fn several_sounds_play_together_and_end_independently() {
    let mut c = controller();
    c.play_sound("s1");
    c.play_sound("s2");
    started(&mut c, 1);
    started(&mut c, 2);
    assert_eq!(c.soundboard().sounds().len(), 2);
    ended(&mut c, 2);
    assert_eq!(c.soundboard().sounds().len(), 1);
    assert!(c.soundboard().is_playing("s1"));
}

#[test]
fn editing_a_playing_sound_applies_live() {
    let mut c = controller();
    c.play_sound("s1");
    calls(&c);
    c.edit_sound("s1", SoundEdit { volume: Some(0.3), looping: Some(false), fade_out: Some(900), ..SoundEdit::default() });
    assert_eq!(calls(&c), [Call::Volume(1, 0.3), Call::Looping(1, false)]);
    assert_eq!(c.library().find_sound("s1").unwrap().1.fade_out, 900);
    // The running instance keeps the fade-out it started with.
    c.stop_sound("s1");
    assert_eq!(calls(&c), [Call::Stop(1, Duration::from_millis(250))]);
}

#[test]
fn sound_failures_are_reported() {
    let mut c = controller();
    c.play_sound("s2");
    c.handle_engine_event(&EngineEvent::Failed { voice: VoiceId(1), error: "unsupported".into() });
    assert!(!c.soundboard().is_playing("s2"));
    assert!(c.take_notices()[0].message.contains("S2"));
}

#[test]
fn remote_soundboard_play_picks_a_sound_of_the_board() {
    let mut c = controller();
    assert!(c.remote_play_soundboard("b"));
    let path = played_path(&calls(&c)[0]).to_owned();
    assert!(path == "/sfx/s1.ogg" || path == "/sfx/s2.ogg");
    assert!(c.remote_play_soundboard("s2"));
    assert!(!c.remote_play_soundboard("nope"));
}

#[test]
fn removing_a_soundboard_fades_out_its_sounds() {
    let mut c = controller();
    c.play_sound("s1");
    calls(&c);
    assert!(c.remove_soundboard("b"));
    assert_eq!(calls(&c), [Call::Stop(1, Duration::from_millis(250))]);
    assert!(c.library().soundboards.is_empty());
}

#[test]
fn stale_and_foreign_events_are_ignored() {
    let mut c = controller();
    started(&mut c, 42);
    ended(&mut c, 42);
    c.handle_engine_event(&EngineEvent::Stopped { voice: VoiceId(42) });
    c.handle_engine_event(&EngineEvent::Failed { voice: VoiceId(42), error: "x".into() });
    assert!(calls(&c).is_empty());
    assert!(c.take_notices().is_empty());
    c.handle_engine_event(&EngineEvent::OutputError("unplugged".into()));
    assert!(c.take_notices()[0].message.contains("unplugged"));
}

#[test]
fn settings_updates_apply_playback_preferences() {
    let mut c = controller();
    c.play_track("t1");
    started(&mut c, 1);
    calls(&c);
    c.update_settings(|settings| {
        settings.playback.volume = 0.25;
        settings.playback.repeat = Repeat::Track;
    });
    assert_eq!(calls(&c), [Call::BusVolume(Bus::Playlist, 0.25), Call::Looping(1, true)]);
    assert_eq!(c.take_dirty(), Dirty { library: false, settings: true });
}
