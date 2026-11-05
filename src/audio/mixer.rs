//! The audio-thread mixer: sums voices into per-bus buffers, applies bus gains and hands finished
//! voices back to the engine thread.
//!
//! Real-time rules: no locks, no allocation (voice storage is reserved up front), no frees
//! (finished voices travel back through the event queue) and no system calls.

use rtrb::{Consumer, Producer, RingBuffer};

use super::api::{Bus, VoiceId};
use super::ramp::Ramp;
use super::voice::{FinishReason, Voice};
use super::CHANNELS;

/// Simultaneous voices; further `Add` commands are rejected.
pub(crate) const MAX_VOICES: usize = 128;
/// Internal processing block; larger device buffers are rendered in several blocks.
pub(crate) const MAX_BLOCK_FRAMES: usize = 1024;
const COMMAND_CAPACITY: usize = 1024;
const EVENT_CAPACITY: usize = MAX_VOICES * 2;
/// Smoothing applied to bus volume changes.
const BUS_SMOOTHING_MS: u64 = 15;

pub(crate) enum MixerCommand {
    Add(Box<Voice>),
    Stop { id: VoiceId, fade_frames: u32 },
    Pause(VoiceId),
    Resume(VoiceId),
    SetVolume { id: VoiceId, volume: f32 },
    SetBusGain { bus: Bus, gain: f32 },
}

pub(crate) enum MixerEvent {
    /// The voice ended or was stopped; it is returned to be dropped off the audio thread.
    Finished { voice: Box<Voice>, reason: FinishReason },
    /// The voice could not be added (too many voices).
    Rejected(Box<Voice>),
}

/// The engine thread's ends of the mixer queues.
pub(crate) struct MixerLink {
    pub(crate) commands: Producer<MixerCommand>,
    pub(crate) events: Consumer<MixerEvent>,
}

pub(crate) struct Mixer {
    voices: Vec<Box<Voice>>,
    bus_gain: [Ramp; Bus::COUNT],
    bus_buffer: Box<[f32]>,
    commands: Consumer<MixerCommand>,
    events: Producer<MixerEvent>,
    sample_rate: u32,
    smoothing: u32,
}

impl Mixer {
    pub(crate) fn new(sample_rate: u32) -> (Self, MixerLink) {
        let (command_tx, command_rx) = RingBuffer::new(COMMAND_CAPACITY);
        let (event_tx, event_rx) = RingBuffer::new(EVENT_CAPACITY);
        let mixer = Self {
            voices: Vec::with_capacity(MAX_VOICES),
            bus_gain: [Ramp::new(1.0); Bus::COUNT],
            bus_buffer: vec![0.0; MAX_BLOCK_FRAMES * CHANNELS].into_boxed_slice(),
            commands: command_rx,
            events: event_tx,
            sample_rate,
            smoothing: u32::try_from(BUS_SMOOTHING_MS * u64::from(sample_rate) / 1000).unwrap_or(u32::MAX),
        };
        (mixer, MixerLink { commands: command_tx, events: event_rx })
    }

    pub(crate) fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Renders interleaved stereo into `out`, overwriting it.
    pub(crate) fn render(&mut self, out: &mut [f32]) {
        for block in out.chunks_mut(MAX_BLOCK_FRAMES * CHANNELS) {
            self.render_block(block);
        }
    }

    fn render_block(&mut self, out: &mut [f32]) {
        self.apply_commands();
        out.fill(0.0);
        let frames = out.len() / CHANNELS;
        for (bus, gain) in self.bus_gain.iter_mut().enumerate() {
            let buffer = &mut self.bus_buffer[..out.len()];
            let mut active = false;
            for voice in self.voices.iter_mut().filter(|voice| voice.bus.index() == bus && voice.finish().is_none()) {
                if !active {
                    buffer.fill(0.0);
                    active = true;
                }
                voice.render_add(buffer);
            }
            if !active {
                gain.skip(frames as u32);
                continue;
            }
            for (dst, src) in out.chunks_exact_mut(CHANNELS).zip(buffer.chunks_exact(CHANNELS)) {
                let gain = gain.tick();
                dst[0] += src[0] * gain;
                dst[1] += src[1] * gain;
            }
        }
        for sample in out.iter_mut() {
            *sample = sample.clamp(-1.0, 1.0);
        }
        self.retire_finished();
    }

    fn apply_commands(&mut self) {
        while let Ok(command) = self.commands.pop() {
            match command {
                MixerCommand::Add(voice) => {
                    if self.voices.len() < self.voices.capacity() {
                        self.voices.push(voice);
                    } else {
                        // Dropping here would free memory on the audio thread; only happens if
                        // the event queue is also full, which the capacities rule out.
                        let _ = self.events.push(MixerEvent::Rejected(voice));
                    }
                }
                MixerCommand::Stop { id, fade_frames } => {
                    if let Some(voice) = self.voice_mut(id) {
                        voice.stop(fade_frames);
                    }
                }
                MixerCommand::Pause(id) => {
                    if let Some(voice) = self.voice_mut(id) {
                        voice.pause();
                    }
                }
                MixerCommand::Resume(id) => {
                    if let Some(voice) = self.voice_mut(id) {
                        voice.resume();
                    }
                }
                MixerCommand::SetVolume { id, volume } => {
                    if let Some(voice) = self.voice_mut(id) {
                        voice.set_volume(volume);
                    }
                }
                MixerCommand::SetBusGain { bus, gain } => {
                    let gain = if gain.is_finite() { gain.clamp(0.0, 1.0) } else { 0.0 };
                    self.bus_gain[bus.index()].set_target(gain, self.smoothing);
                }
            }
        }
    }

    fn voice_mut(&mut self, id: VoiceId) -> Option<&mut Voice> {
        self.voices.iter_mut().find(|voice| voice.id == id).map(|voice| &mut **voice)
    }

    fn retire_finished(&mut self) {
        let mut index = 0;
        while index < self.voices.len() {
            let Some(reason) = self.voices[index].finish() else {
                index += 1;
                continue;
            };
            if self.events.is_full() {
                break; // retried next block
            }
            let voice = self.voices.swap_remove(index);
            let _ = self.events.push(MixerEvent::Finished { voice, reason });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::api::VoiceHandle;
    use super::super::decoder::test_wav::{quantized, temp_path, write};
    use super::super::stream::{CHUNK_COUNT, Stream, StreamParams};
    use super::*;

    const RATE: u32 = 48_000;

    struct Rig {
        mixer: Mixer,
        link: MixerLink,
        stream: Stream,
        handle: VoiceHandle,
        path: std::path::PathBuf,
    }

    /// A mixer playing one voice of a constant-level mono file.
    fn rig(name: &str, frames: usize, level: f32, bus: Bus, volume: f32) -> Rig {
        let path = temp_path(name);
        write(&path, RATE, 1, &vec![level; frames]);
        let (mut stream, queues) = Stream::open(&path, RATE, StreamParams { looping: false, fade_in: 0, fade_out: 0 }).unwrap();
        stream.fill(CHUNK_COUNT);
        let handle = VoiceHandle::new(VoiceId(1), RATE);
        let voice = Voice::new(VoiceId(1), bus, Arc::clone(handle.shared()), queues, volume, RATE);
        let (mixer, mut link) = Mixer::new(RATE);
        assert!(link.commands.push(MixerCommand::Add(Box::new(voice))).is_ok());
        Rig { mixer, link, stream, handle, path }
    }

    impl Rig {
        /// Renders `frames` frames in small blocks, refilling the stream in between like the
        /// engine thread does. Returns the left channel.
        fn render(&mut self, frames: usize) -> Vec<f32> {
            let mut out = vec![0.0; frames * CHANNELS];
            for block in out.chunks_mut(256 * CHANNELS) {
                self.stream.fill(CHUNK_COUNT);
                self.mixer.render(block);
            }
            out.iter().step_by(2).copied().collect()
        }

        fn command(&mut self, command: MixerCommand) {
            assert!(self.link.commands.push(command).is_ok());
        }

        fn finished(&mut self) -> Option<FinishReason> {
            match self.link.events.pop() {
                Ok(MixerEvent::Finished { reason, .. }) => Some(reason),
                Ok(MixerEvent::Rejected(_)) => panic!("rejected"),
                Err(_) => None,
            }
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[test]
    fn plays_to_the_end_and_reports_it() {
        let mut rig = rig("end", 3000, 0.5, Bus::Soundboard, 1.0);
        let level = quantized(0.5);
        let out = rig.render(2000);
        assert!(out.iter().all(|&s| (s - level).abs() < 1e-6));
        assert_eq!(rig.handle.position(), std::time::Duration::from_nanos(2000 * 1_000_000_000 / 48_000));
        let out = rig.render(2000);
        assert!(out[..1000].iter().all(|&s| (s - level).abs() < 1e-6));
        assert!(out[1000..].iter().all(|&s| s == 0.0));
        assert_eq!(rig.finished(), Some(FinishReason::Ended));
    }

    #[test]
    fn volume_and_bus_gain_multiply() {
        let mut rig = rig("gain", 48_000, 0.5, Bus::Playlist, 0.5);
        rig.command(MixerCommand::SetBusGain { bus: Bus::Playlist, gain: 0.5 });
        rig.render(2000); // let the bus smoothing settle
        let out = rig.render(100);
        let expected = quantized(0.5) * 0.25;
        assert!(out.iter().all(|&s| (s - expected).abs() < 1e-6), "{:?}", &out[..4]);
        // Other buses are unaffected by this bus gain.
        rig.command(MixerCommand::SetBusGain { bus: Bus::Soundboard, gain: 0.0 });
        let out = rig.render(2000);
        assert!((out[1999] - expected).abs() < 1e-6);
    }

    #[test]
    fn pause_holds_the_position_without_clicks() {
        let mut rig = rig("pause", 48_000, 0.5, Bus::Soundboard, 1.0);
        rig.render(1000);
        rig.command(MixerCommand::Pause(VoiceId(1)));
        let out = rig.render(1000);
        // De-click ramp, then silence.
        assert!(out[0] > 0.0 && out[0] < quantized(0.5));
        assert!(out[500..].iter().all(|&s| s == 0.0));
        let held = rig.handle.position();
        rig.render(5000);
        assert_eq!(rig.handle.position(), held);
        rig.command(MixerCommand::Resume(VoiceId(1)));
        let out = rig.render(1000);
        assert!(out[0] < out[200]);
        assert!(rig.handle.position() > held);
    }

    #[test]
    fn stop_fades_out_then_reports_stopped() {
        let mut rig = rig("stop", 48_000, 0.5, Bus::Soundboard, 1.0);
        rig.render(500);
        rig.command(MixerCommand::Stop { id: VoiceId(1), fade_frames: 480 });
        let out = rig.render(1000);
        assert!(out[0] > out[240] && out[240] > out[470]);
        assert!(out[480..].iter().all(|&s| s == 0.0));
        assert_eq!(rig.finished(), Some(FinishReason::Stopped));
    }

    #[test]
    fn seek_switches_to_the_new_generation() {
        let mut rig = rig("seek", 48_000, 0.5, Bus::Soundboard, 1.0);
        rig.render(1000);
        let generation = rig.handle.shared().next_generation();
        rig.stream.seek(30_000, generation);
        let out = rig.render(2000);
        // Stale audio is dropped and the new position fades in.
        assert!(out[0] < quantized(0.5));
        assert!((out[1999] - quantized(0.5)).abs() < 1e-6);
        let frames = rig.handle.position().as_secs_f64() * 48_000.0;
        assert!((30_000.0..=32_000.0).contains(&frames.round()), "{frames}");
    }

    #[test]
    fn output_is_clamped() {
        let mut rig = rig("clamp", 48_000, 0.9, Bus::Soundboard, 1.0);
        let path = temp_path("clamp2");
        write(&path, RATE, 1, &vec![0.9; 48_000]);
        let (mut second, queues) = Stream::open(&path, RATE, StreamParams { looping: false, fade_in: 0, fade_out: 0 }).unwrap();
        second.fill(CHUNK_COUNT);
        let handle = VoiceHandle::new(VoiceId(2), RATE);
        rig.command(MixerCommand::Add(Box::new(Voice::new(VoiceId(2), Bus::Soundboard, Arc::clone(handle.shared()), queues, 1.0, RATE))));
        let out = rig.render(100);
        assert!(out.iter().all(|&s| s == 1.0));
        std::fs::remove_file(path).unwrap();
    }
}
