//! `kenkurs` — headless runner for the native Kenku FM core.
//!
//! The egui frontend is the next milestone; until then this binary exercises the real engine and
//! controller from the command line, on a sound card or on the silent clock.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use kenkurs::app::{Controller, Core, CoreConfig, SoundEdit};
use kenkurs::audio::{EngineHandle, OutputKind, OutputPreference, list_output_devices};
use kenkurs::state::{Repeat, Sound, Store, Track};
use kenkurs::util::logger;

const USAGE: &str = "\
kenkurs — native Kenku FM core (headless runner)

USAGE:
    kenkurs [--null | --device NAME] <COMMAND>

COMMANDS:
    play [--shuffle] [--repeat off|track|playlist] FILE...
        Plays the files as a temporary playlist (repeat defaults to off).
    sound [--loop] [--volume 0..1] [--fade-in MS] [--fade-out MS] [--seconds N] FILE
        Plays one soundboard sound (Kenku FM defaults: 100 ms fades).
        With --seconds, stops it (fading out) after N seconds.
    devices
        Lists the audio output devices.
    info
        Shows where the library and settings are stored.

OPTIONS:
    --null          Silent real-time clock instead of an audio device.
    --device NAME   Output device to use (see `kenkurs devices`).

ENVIRONMENT:
    KENKURS_LOG          off|error|warn|info|debug|trace (default: info)
    KENKURS_CONFIG_DIR   Overrides the configuration directory.
";

const POLL: Duration = Duration::from_millis(100);

fn main() -> ExitCode {
    logger::init();
    match run(std::env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("kenkurs: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    let mut args = args.into_iter();
    let mut output = None;
    let command = loop {
        match args.next().as_deref() {
            Some("--null") => output = Some(OutputPreference::Null),
            Some("--device") => output = Some(OutputPreference::Device(args.next().ok_or("--device needs a name")?)),
            Some("-h" | "--help" | "help") | None => {
                print!("{USAGE}");
                return Ok(());
            }
            Some(command) => break command.to_owned(),
        }
    };
    let rest: Vec<String> = args.collect();
    match command.as_str() {
        "play" => play(output, rest),
        "sound" => sound(output, rest),
        "devices" => devices(),
        "info" => info(),
        other => Err(format!("unknown command `{other}` (see `kenkurs --help`)")),
    }
}

fn play(output: Option<OutputPreference>, args: Vec<String>) -> Result<(), String> {
    let mut shuffle = false;
    let mut repeat = Repeat::Off;
    let mut files = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--shuffle" => shuffle = true,
            "--repeat" => {
                repeat = match args.next().as_deref() {
                    Some("off") => Repeat::Off,
                    Some("track") => Repeat::Track,
                    Some("playlist") => Repeat::Playlist,
                    _ => return Err("--repeat takes off, track or playlist".into()),
                }
            }
            flag if flag.starts_with("--") => return Err(format!("unknown option `{flag}`")),
            _ => files.push(absolute(&arg)?),
        }
    }
    if files.is_empty() {
        return Err("play: no files given".into());
    }

    let core = start_core(output)?;
    {
        let mut controller = core.lock();
        let playlist = controller.add_playlist("Command line");
        let tracks: Vec<Track> = files.iter().map(|file| Track::from_path(file)).collect();
        let first = tracks[0].id.clone();
        controller.add_tracks(&playlist, tracks);
        controller.set_repeat(repeat);
        controller.set_shuffle(shuffle);
        if shuffle {
            controller.play_playlist(&playlist);
        } else {
            controller.play_track(&first);
        }
    }

    let mut last_line = String::new();
    let mut failures = 0;
    loop {
        thread::sleep(POLL);
        let mut controller = core.lock();
        failures += print_notices(&mut controller);
        let player = controller.playlist();
        if let Some(track) = player.track() {
            let state = if player.is_playing() { "▶" } else { "■" };
            let line = format!("{state} {}  {} / {}", track.title, clock(player.progress()), player.duration().map_or("--:--".into(), clock));
            if line != last_line {
                println!("{line}");
                last_line = line;
            }
        }
        if !player.is_playing() && !player.is_loading() {
            return if failures == 0 { Ok(()) } else { Err("playback failed".into()) };
        }
    }
}

fn sound(output: Option<OutputPreference>, args: Vec<String>) -> Result<(), String> {
    let mut edit = SoundEdit::default();
    let mut seconds = None;
    let mut file = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--loop" => edit.looping = Some(true),
            "--volume" => edit.volume = Some(parse(&value("--volume")?)?),
            "--fade-in" => edit.fade_in = Some(parse(&value("--fade-in")?)?),
            "--fade-out" => edit.fade_out = Some(parse(&value("--fade-out")?)?),
            "--seconds" => seconds = Some(Duration::from_secs_f64(parse(&value("--seconds")?)?)),
            flag if flag.starts_with("--") => return Err(format!("unknown option `{flag}`")),
            _ if file.is_none() => file = Some(absolute(&arg)?),
            _ => return Err("sound: only one file can be given".into()),
        }
    }
    let file = file.ok_or("sound: no file given")?;

    let core = start_core(output)?;
    let sound_id = {
        let mut controller = core.lock();
        let board = controller.add_soundboard("Command line");
        let sound = Sound::from_path(&file);
        let sound_id = sound.id.clone();
        controller.add_sounds(&board, vec![sound]);
        controller.edit_sound(&sound_id, edit);
        controller.play_sound(&sound_id);
        sound_id
    };

    let deadline = seconds.map(|seconds| Instant::now() + seconds);
    let mut last_line = String::new();
    let mut failures = 0;
    loop {
        thread::sleep(POLL);
        let mut controller = core.lock();
        failures += print_notices(&mut controller);
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) && controller.soundboard().is_playing(&sound_id) {
            let fade = Duration::from_millis(u64::from(controller.soundboard().get(&sound_id).map_or(0, |s| s.sound().fade_out)));
            controller.stop_sound(&sound_id);
            drop(controller);
            println!("■ stopping (fade-out {} ms)", fade.as_millis());
            thread::sleep(fade + POLL); // let the fade finish before tearing down the engine
            return Ok(());
        }
        let Some(active) = controller.soundboard().get(&sound_id) else {
            return if failures == 0 { Ok(()) } else { Err("playback failed".into()) };
        };
        if active.is_started() {
            let looping = if active.sound().looping { " (loop)" } else { "" };
            let line = format!(
                "♪ {}{looping}  {} / {}",
                active.sound().title,
                clock(active.progress()),
                active.duration().map_or("--:--".into(), clock)
            );
            if line != last_line {
                println!("{line}");
                last_line = line;
            }
        }
    }
}

fn devices() -> Result<(), String> {
    let names = list_output_devices().map_err(|error| error.to_string())?;
    if names.is_empty() {
        let hint = if cfg!(feature = "cpal") { "" } else { " (built without the `cpal` feature)" };
        println!("no output devices found{hint}");
    }
    for name in names {
        println!("{name}");
    }
    Ok(())
}

fn info() -> Result<(), String> {
    let Some(store) = Store::open_default() else {
        println!("no configuration directory: neither KENKURS_CONFIG_DIR, XDG_CONFIG_HOME nor HOME is set");
        return Ok(());
    };
    println!("configuration directory: {}", store.dir().display());
    match store.load_library() {
        Ok(library) => {
            let tracks: usize = library.playlists.iter().map(|playlist| playlist.tracks.len()).sum();
            let sounds: usize = library.soundboards.iter().map(|soundboard| soundboard.sounds.len()).sum();
            println!(
                "library: {} playlists ({tracks} tracks), {} soundboards ({sounds} sounds)",
                library.playlists.len(),
                library.soundboards.len()
            );
        }
        Err(error) => println!("library: {error}"),
    }
    Ok(())
}

fn start_core(output: Option<OutputPreference>) -> Result<Core, String> {
    // The CLI works on a throwaway in-memory library: nothing is persisted.
    let core = Core::start(CoreConfig { store: None, output }).map_err(|error| error.to_string())?;
    if core.output_info().kind == OutputKind::Null {
        eprintln!("note: no audio device in use — playing silently on a real-time clock");
    }
    Ok(core)
}

/// Prints the controller's user-facing messages; returns how many there were.
fn print_notices(controller: &mut Controller<EngineHandle>) -> usize {
    let notices = controller.take_notices();
    for notice in &notices {
        eprintln!("! {}", notice.message);
    }
    notices.len()
}

fn absolute(path: &str) -> Result<PathBuf, String> {
    std::path::absolute(Path::new(path)).map_err(|error| format!("{path}: {error}"))
}

fn parse<T: std::str::FromStr>(text: &str) -> Result<T, String> {
    text.parse().map_err(|_| format!("invalid number `{text}`"))
}

fn clock(duration: Duration) -> String {
    let seconds = duration.as_secs();
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}
