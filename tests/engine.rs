//! End-to-end tests of the real engine and runtime on the silent clock output (no audio device
//! needed, so they also run in containers and CI).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use kenkurs::app::{Core, CoreConfig};
use kenkurs::audio::{AudioBackend, AudioEngine, Bus, EngineConfig, EngineEvent, OutputKind, OutputPreference, PlayRequest, VoiceState};
use kenkurs::state::{Repeat, Sound, Store, Track};

const RATE: u32 = 48_000;
const TIMEOUT: Duration = Duration::from_secs(10);

/// Writes a 16-bit stereo WAV of a quiet sine.
fn wav(name: &str, seconds: f32) -> PathBuf {
    let path = std::env::temp_dir().join(format!("kenkurs-it-{name}-{}.wav", std::process::id()));
    let frames = (RATE as f32 * seconds) as usize;
    let mut data = Vec::with_capacity(frames * 4);
    for i in 0..frames {
        let sample = ((i as f32 * 440.0 * std::f32::consts::TAU / RATE as f32).sin() * 8000.0) as i16;
        data.extend_from_slice(&sample.to_le_bytes());
        data.extend_from_slice(&sample.to_le_bytes());
    }
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    for value in [16u32.to_le_bytes(), [1, 0, 2, 0], RATE.to_le_bytes(), (RATE * 4).to_le_bytes(), [4, 0, 16, 0]] {
        bytes.extend_from_slice(&value);
    }
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&data);
    std::fs::write(&path, bytes).unwrap();
    path
}

fn request(path: &Path, looping: bool) -> PlayRequest {
    PlayRequest { path: path.to_owned(), bus: Bus::Soundboard, volume: 1.0, looping, fade_in: Duration::ZERO, fade_out: Duration::ZERO }
}

fn next_event(events: &Receiver<EngineEvent>) -> EngineEvent {
    events.recv_timeout(TIMEOUT).expect("engine event")
}

fn null_engine() -> (AudioEngine, Receiver<EngineEvent>) {
    let (engine, events) = AudioEngine::start(EngineConfig { output: OutputPreference::Null }).unwrap();
    assert_eq!(engine.output_info().kind, OutputKind::Null);
    (engine, events)
}

#[test]
fn plays_a_file_to_the_end_in_real_time() {
    let path = wav("end", 0.3);
    let (engine, events) = null_engine();
    let handle = engine.handle();
    let start = Instant::now();
    let voice = handle.play(request(&path, false));
    assert_eq!(next_event(&events), EngineEvent::Started { voice: voice.id(), duration: Some(Duration::from_millis(300)) });
    assert_eq!(voice.duration(), Some(Duration::from_millis(300)));
    assert_eq!(next_event(&events), EngineEvent::Ended { voice: voice.id() });
    let elapsed = start.elapsed();
    assert!(elapsed >= Duration::from_millis(250), "finished too early: {elapsed:?}");
    assert_eq!(voice.state(), VoiceState::Finished);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn stop_fades_out_and_reports_stopped() {
    let path = wav("stop", 5.0);
    let (engine, events) = null_engine();
    let handle = engine.handle();
    let voice = handle.play(request(&path, true));
    assert!(matches!(next_event(&events), EngineEvent::Started { .. }));
    std::thread::sleep(Duration::from_millis(100));
    assert!(voice.position() > Duration::ZERO);
    handle.stop(voice.id(), Duration::from_millis(50));
    assert_eq!(next_event(&events), EngineEvent::Stopped { voice: voice.id() });
    std::fs::remove_file(path).unwrap();
}

#[test]
fn pause_seek_and_resume() {
    let path = wav("seek", 5.0);
    let (engine, events) = null_engine();
    let handle = engine.handle();
    let voice = handle.play(request(&path, false));
    assert!(matches!(next_event(&events), EngineEvent::Started { .. }));
    handle.pause(voice.id());
    handle.seek(voice.id(), Duration::from_secs(4));
    std::thread::sleep(Duration::from_millis(150));
    let held = voice.position();
    assert!((held.as_secs_f64() - 4.0).abs() < 0.05, "{held:?}");
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(voice.position(), held, "paused voices must not advance");
    handle.resume(voice.id());
    assert_eq!(next_event(&events), EngineEvent::Ended { voice: voice.id() });
    std::fs::remove_file(path).unwrap();
}

#[test]
fn missing_files_fail_cleanly() {
    let (engine, events) = null_engine();
    let voice = engine.handle().play(request(Path::new("/no/such/file.flac"), false));
    assert!(matches!(next_event(&events), EngineEvent::Failed { voice: id, .. } if id == voice.id()));
    assert_eq!(voice.state(), VoiceState::Failed);
}

#[test]
fn core_runs_a_playlist_and_persists_the_library() {
    let first = wav("core1", 0.2);
    let second = wav("core2", 0.2);
    let dir = std::env::temp_dir().join(format!("kenkurs-it-store-{}", std::process::id()));
    let store = Store::new(&dir);
    {
        let core = Core::start(CoreConfig { store: Some(store.clone()), output: Some(OutputPreference::Null) }).unwrap();
        let mut controller = core.lock();
        let playlist = controller.add_playlist("Session");
        let tracks = vec![Track::from_path(&first), Track::from_path(&second)];
        let first_id = tracks[0].id.clone();
        controller.add_tracks(&playlist, tracks);
        controller.set_repeat(Repeat::Off);
        let board = controller.add_soundboard("SFX");
        controller.add_sounds(&board, vec![Sound::from_path(&first)]);
        assert!(controller.play_track(&first_id));
        drop(controller);

        // The core thread advances through the queue on its own and stops at the end.
        let deadline = Instant::now() + TIMEOUT;
        let mut titles = Vec::new();
        loop {
            {
                let controller = core.lock();
                if let Some(track) = controller.playlist().track() {
                    if titles.last() != Some(&track.title) {
                        titles.push(track.title.clone());
                    }
                }
                if !titles.is_empty() && !controller.playlist().is_playing() && !controller.playlist().is_loading() {
                    break;
                }
            }
            assert!(Instant::now() < deadline, "playlist did not finish");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(titles.len(), 2, "{titles:?}");
        assert!(core.lock().take_notices().is_empty());
    } // dropping the core saves

    let library = store.load_library().unwrap();
    assert_eq!(library.playlists[0].tracks.len(), 2);
    assert_eq!(library.soundboards[0].sounds.len(), 1);
    assert_eq!(store.load_settings().unwrap().playback.repeat, Repeat::Off);
    std::fs::remove_dir_all(dir).unwrap();
    std::fs::remove_file(first).unwrap();
    std::fs::remove_file(second).unwrap();
}
