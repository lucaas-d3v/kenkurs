//! The engine thread and the public engine handles.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use rtrb::PushError;

use super::api::{AudioBackend, AudioError, Bus, EngineEvent, PlayRequest, VoiceHandle, VoiceId, VoiceShared, VoiceState};
use super::mixer::{MixerCommand, MixerEvent, MixerLink};
use super::output::{self, Output, OutputInfo, OutputPreference};
use super::stream::{CHUNK_COUNT, PREFILL_CHUNKS, Stream, StreamParams};
use super::voice::{FinishReason, Voice};
use super::{duration_to_frames, frames_to_duration, frames_u32};

/// How often the engine thread tops up the decode queues while something is playing. Queues
/// hold ~0.5 s, so this leaves a wide safety margin.
const ACTIVE_POLL: Duration = Duration::from_millis(10);
/// Poll interval when every voice is paused (only finished-voice events can arrive).
const PAUSED_POLL: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, Default)]
pub struct EngineConfig {
    pub output: OutputPreference,
}

enum EngineCommand {
    Play { voice: VoiceHandle, request: PlayRequest },
    Stop { id: VoiceId, fade: Duration },
    Pause(VoiceId),
    Resume(VoiceId),
    Seek { id: VoiceId, position: Duration },
    SetVolume { id: VoiceId, volume: f32 },
    SetLooping { id: VoiceId, looping: bool },
    SetBusVolume { bus: Bus, volume: f32 },
    Shutdown,
}

/// Owns the output stream and the engine thread. Dropping it stops all audio.
pub struct AudioEngine {
    handle: EngineHandle,
    info: OutputInfo,
    output: Option<Output>,
    thread: Option<JoinHandle<()>>,
}

impl AudioEngine {
    /// Opens the output and starts the engine thread. Engine events arrive on the returned
    /// receiver.
    pub fn start(config: EngineConfig) -> Result<(Self, Receiver<EngineEvent>), AudioError> {
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let (output, link, info) = output::open(&config.output, event_tx.clone())?;
        log::info!("audio output: {} ({:?}, {} Hz, {} ch)", info.device, info.kind, info.sample_rate, info.channels);

        let (command_tx, command_rx) = crossbeam_channel::unbounded();
        let worker = Worker {
            commands: command_rx,
            events: event_tx,
            mixer: link,
            backlog: VecDeque::new(),
            voices: HashMap::new(),
            sample_rate: info.sample_rate,
        };
        let thread = thread::Builder::new().name("kenkurs-engine".into()).spawn(move || worker.run())?;
        let handle = EngineHandle { commands: command_tx, next_id: Arc::new(AtomicU64::new(1)), sample_rate: info.sample_rate };
        Ok((Self { handle, info, output: Some(output), thread: Some(thread) }, event_rx))
    }

    /// A cloneable, thread-safe handle for controlling playback.
    pub fn handle(&self) -> EngineHandle {
        self.handle.clone()
    }

    pub fn output_info(&self) -> &OutputInfo {
        &self.info
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        // Silence the device first, then let the engine thread release the streams.
        drop(self.output.take());
        let _ = self.handle.commands.send(EngineCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Cheap, cloneable command sender. Implements [`AudioBackend`].
#[derive(Clone)]
pub struct EngineHandle {
    commands: Sender<EngineCommand>,
    next_id: Arc<AtomicU64>,
    sample_rate: u32,
}

impl EngineHandle {
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn send(&self, command: EngineCommand) {
        if self.commands.send(command).is_err() {
            log::error!("audio engine is not running; command dropped");
        }
    }
}

impl AudioBackend for EngineHandle {
    fn play(&self, request: PlayRequest) -> VoiceHandle {
        let voice = VoiceHandle::new(VoiceId(self.next_id.fetch_add(1, Ordering::Relaxed)), self.sample_rate);
        if self.commands.send(EngineCommand::Play { voice: voice.clone(), request }).is_err() {
            log::error!("audio engine is not running; cannot play");
            voice.shared().set_state(VoiceState::Failed);
        }
        voice
    }

    fn stop(&self, id: VoiceId, fade: Duration) {
        self.send(EngineCommand::Stop { id, fade });
    }

    fn pause(&self, id: VoiceId) {
        self.send(EngineCommand::Pause(id));
    }

    fn resume(&self, id: VoiceId) {
        self.send(EngineCommand::Resume(id));
    }

    fn seek(&self, id: VoiceId, position: Duration) {
        self.send(EngineCommand::Seek { id, position });
    }

    fn set_volume(&self, id: VoiceId, volume: f32) {
        self.send(EngineCommand::SetVolume { id, volume });
    }

    fn set_looping(&self, id: VoiceId, looping: bool) {
        self.send(EngineCommand::SetLooping { id, looping });
    }

    fn set_bus_volume(&self, bus: Bus, volume: f32) {
        self.send(EngineCommand::SetBusVolume { bus, volume });
    }
}

/// Engine-thread bookkeeping for a voice living in the mixer.
struct LiveVoice {
    stream: Stream,
    shared: Arc<VoiceShared>,
    paused: bool,
}

struct Worker {
    commands: Receiver<EngineCommand>,
    events: Sender<EngineEvent>,
    mixer: MixerLink,
    /// Mixer commands waiting for room in the real-time queue.
    backlog: VecDeque<MixerCommand>,
    voices: HashMap<VoiceId, LiveVoice>,
    sample_rate: u32,
}

impl Worker {
    fn run(mut self) {
        loop {
            let first = if self.voices.is_empty() && self.backlog.is_empty() {
                // Idle: sleep until there is something to do.
                self.commands.recv().ok()
            } else {
                match self.commands.recv_timeout(self.poll_interval()) {
                    Ok(command) => Some(command),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            };
            match first {
                Some(command) => {
                    if !self.handle(command) {
                        return;
                    }
                }
                // Idle `recv` failed: every handle is gone.
                None if self.voices.is_empty() && self.backlog.is_empty() => return,
                None => {}
            }
            while let Ok(command) = self.commands.try_recv() {
                if !self.handle(command) {
                    return;
                }
            }
            self.flush_backlog();
            for voice in self.voices.values_mut() {
                voice.stream.fill(CHUNK_COUNT);
            }
            self.collect_mixer_events();
        }
    }

    fn poll_interval(&self) -> Duration {
        if self.voices.values().any(|voice| !voice.paused) { ACTIVE_POLL } else { PAUSED_POLL }
    }

    /// Returns `false` on shutdown.
    fn handle(&mut self, command: EngineCommand) -> bool {
        match command {
            EngineCommand::Play { voice, request } => self.play(voice, request),
            EngineCommand::Stop { id, fade } => {
                if self.voices.contains_key(&id) {
                    let fade_frames = frames_u32(duration_to_frames(fade, self.sample_rate));
                    self.send_mixer(MixerCommand::Stop { id, fade_frames });
                }
            }
            EngineCommand::Pause(id) => self.set_paused(id, true),
            EngineCommand::Resume(id) => self.set_paused(id, false),
            EngineCommand::Seek { id, position } => {
                if let Some(voice) = self.voices.get_mut(&id) {
                    let frames = duration_to_frames(position, self.sample_rate);
                    let generation = voice.shared.next_generation();
                    voice.stream.seek(frames, generation);
                    voice.shared.store_position(frames);
                }
            }
            EngineCommand::SetVolume { id, volume } => {
                if self.voices.contains_key(&id) {
                    self.send_mixer(MixerCommand::SetVolume { id, volume });
                }
            }
            EngineCommand::SetLooping { id, looping } => {
                if let Some(voice) = self.voices.get_mut(&id) {
                    voice.stream.set_looping(looping);
                }
            }
            EngineCommand::SetBusVolume { bus, volume } => self.send_mixer(MixerCommand::SetBusGain { bus, gain: volume }),
            EngineCommand::Shutdown => return false,
        }
        true
    }

    fn play(&mut self, handle: VoiceHandle, request: PlayRequest) {
        let id = handle.id();
        let shared = Arc::clone(handle.shared());
        let params = StreamParams {
            looping: request.looping,
            fade_in: duration_to_frames(request.fade_in, self.sample_rate),
            fade_out: duration_to_frames(request.fade_out, self.sample_rate),
        };
        let (mut stream, queues) = match Stream::open(&request.path, self.sample_rate, params) {
            Ok(opened) => opened,
            Err(error) => {
                log::warn!("{id}: {error}");
                shared.set_state(VoiceState::Failed);
                self.emit(EngineEvent::Failed { voice: id, error: error.to_string() });
                return;
            }
        };
        stream.fill(PREFILL_CHUNKS);
        let duration = stream.duration();
        shared.set_duration(duration);
        shared.set_state(VoiceState::Playing);
        let voice = Voice::new(id, request.bus, Arc::clone(&shared), queues, request.volume, self.sample_rate);
        self.send_mixer(MixerCommand::Add(Box::new(voice)));
        self.voices.insert(id, LiveVoice { stream, shared, paused: false });
        log::debug!("{id}: playing {}", request.path.display());
        let duration = duration.map(|frames| frames_to_duration(frames, self.sample_rate));
        self.emit(EngineEvent::Started { voice: id, duration });
    }

    fn set_paused(&mut self, id: VoiceId, paused: bool) {
        let Some(voice) = self.voices.get_mut(&id) else {
            return;
        };
        voice.paused = paused;
        voice.shared.set_state(if paused { VoiceState::Paused } else { VoiceState::Playing });
        self.send_mixer(if paused { MixerCommand::Pause(id) } else { MixerCommand::Resume(id) });
    }

    fn send_mixer(&mut self, command: MixerCommand) {
        if !self.backlog.is_empty() {
            self.backlog.push_back(command);
            return;
        }
        if let Err(PushError::Full(command)) = self.mixer.commands.push(command) {
            self.backlog.push_back(command);
        }
    }

    fn flush_backlog(&mut self) {
        while let Some(command) = self.backlog.pop_front() {
            if let Err(PushError::Full(command)) = self.mixer.commands.push(command) {
                self.backlog.push_front(command);
                break;
            }
        }
    }

    fn collect_mixer_events(&mut self) {
        while let Ok(event) = self.mixer.events.pop() {
            let (voice, event) = match event {
                MixerEvent::Finished { voice, reason } => {
                    let id = voice.id;
                    let event = match reason {
                        FinishReason::Ended => EngineEvent::Ended { voice: id },
                        FinishReason::Stopped => EngineEvent::Stopped { voice: id },
                    };
                    (voice, event)
                }
                MixerEvent::Rejected(voice) => {
                    let id = voice.id;
                    (voice, EngineEvent::Failed { voice: id, error: "too many sounds playing at once".into() })
                }
            };
            if voice.underruns() > 0 {
                log::debug!("{}: {} buffer underruns", voice.id, voice.underruns());
            }
            if let Some(live) = self.voices.remove(&voice.id) {
                let state = if matches!(event, EngineEvent::Failed { .. }) { VoiceState::Failed } else { VoiceState::Finished };
                live.shared.set_state(state);
            }
            // The voice and its stream are freed here, off the audio thread.
            drop(voice);
            self.emit(event);
        }
    }

    fn emit(&self, event: EngineEvent) {
        // The receiver may already be gone during shutdown.
        let _ = self.events.send(event);
    }
}
