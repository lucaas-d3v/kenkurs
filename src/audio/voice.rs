//! Consumer side of a voice, run on the audio thread.
//!
//! A voice plays the chunks its [`Stream`](super::stream::Stream) prepares and applies the gains
//! that must react immediately to the user: volume (smoothed), pause/resume and seek de-clicking,
//! and the fade-out of a stop request. It never allocates or frees: consumed chunks go back to
//! the stream through the recycle queue.

use std::sync::Arc;

use super::api::{Bus, VoiceId, VoiceShared};
use super::ramp::Ramp;
use super::stream::{Chunk, ChunkQueues};
use super::CHANNELS;

/// Length of the anti-click ramps for pause, resume and seek.
const DECLICK_MS: u64 = 5;
/// Smoothing applied to volume changes.
const VOLUME_SMOOTHING_MS: u64 = 15;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FinishReason {
    /// Reached the end of the file.
    Ended,
    /// Stopped by request.
    Stopped,
}

pub(crate) struct Voice {
    pub(crate) id: VoiceId,
    pub(crate) bus: Bus,
    shared: Arc<VoiceShared>,
    queues: ChunkQueues,
    current: Option<Box<Chunk>>,
    offset: usize,
    generation: u32,
    volume: Ramp,
    /// Stop fade.
    fade: Ramp,
    /// Pause/seek de-click gate.
    gate: Ramp,
    paused: bool,
    stopping: bool,
    finish: Option<FinishReason>,
    declick: u32,
    smoothing: u32,
    underruns: u32,
}

impl Voice {
    pub(crate) fn new(id: VoiceId, bus: Bus, shared: Arc<VoiceShared>, queues: ChunkQueues, volume: f32, sample_rate: u32) -> Self {
        let ms = |ms: u64| u32::try_from(ms * u64::from(sample_rate) / 1000).unwrap_or(u32::MAX);
        Self {
            id,
            bus,
            shared,
            queues,
            current: None,
            offset: 0,
            generation: 0,
            volume: Ramp::new(sanitize(volume)),
            fade: Ramp::new(1.0),
            gate: Ramp::new(1.0),
            paused: false,
            stopping: false,
            finish: None,
            declick: ms(DECLICK_MS),
            smoothing: ms(VOLUME_SMOOTHING_MS),
            underruns: 0,
        }
    }

    pub(crate) fn finish(&self) -> Option<FinishReason> {
        self.finish
    }

    /// Callbacks that found no decoded audio ready (the engine thread fell behind).
    pub(crate) fn underruns(&self) -> u32 {
        self.underruns
    }

    pub(crate) fn pause(&mut self) {
        if !self.paused {
            self.paused = true;
            self.gate.set_target(0.0, self.declick);
        }
    }

    pub(crate) fn resume(&mut self) {
        if self.paused {
            self.paused = false;
            self.gate.set_target(1.0, self.declick);
        }
    }

    pub(crate) fn set_volume(&mut self, volume: f32) {
        self.volume.set_target(sanitize(volume), self.smoothing);
    }

    /// Fades out over `fade_frames`, then finishes as [`FinishReason::Stopped`].
    pub(crate) fn stop(&mut self, fade_frames: u32) {
        if self.finish.is_some() {
            return;
        }
        if fade_frames == 0 || self.paused {
            // A paused voice is already silent.
            self.finish = Some(FinishReason::Stopped);
            return;
        }
        self.stopping = true;
        self.fade.set_target(0.0, fade_frames);
    }

    /// Mixes the next `out.len() / 2` frames into `out` (interleaved stereo).
    pub(crate) fn render_add(&mut self, out: &mut [f32]) {
        if self.finish.is_some() {
            return;
        }
        let latest = self.shared.generation();
        if latest != self.generation {
            self.adopt_generation(latest);
        }
        let frames = out.len() / CHANNELS;
        let mut done = 0;
        while done < frames && self.finish.is_none() {
            if self.paused && !self.gate.is_ramping() {
                break; // fully paused: hold the position
            }
            if self.current.is_none() && !self.next_chunk() {
                self.underruns = self.underruns.saturating_add(1);
                break;
            }
            let chunk = self.current.as_ref().expect("ensured above");
            let available = chunk.frames - self.offset;
            if available == 0 {
                let last = chunk.last;
                self.recycle_current();
                if last {
                    self.finish = Some(FinishReason::Ended);
                }
                continue;
            }
            let mut span = available.min(frames - done);
            if self.paused {
                span = span.min(self.gate.remaining() as usize);
            }
            if self.stopping {
                span = span.min(self.fade.remaining().max(1) as usize);
            }
            let source = &chunk.samples[self.offset * CHANNELS..(self.offset + span) * CHANNELS];
            let target = &mut out[done * CHANNELS..(done + span) * CHANNELS];
            for (dst, src) in target.chunks_exact_mut(CHANNELS).zip(source.chunks_exact(CHANNELS)) {
                let gain = self.volume.tick() * self.fade.tick() * self.gate.tick();
                dst[0] += src[0] * gain;
                dst[1] += src[1] * gain;
            }
            self.offset += span;
            done += span;
            if self.stopping && !self.fade.is_ramping() {
                self.finish = Some(FinishReason::Stopped);
            }
        }
        if let Some(chunk) = &self.current {
            self.shared.store_position(chunk.position + self.offset as u64);
        }
    }

    fn next_chunk(&mut self) -> bool {
        while let Ok(chunk) = self.queues.chunks.pop() {
            if chunk.generation < self.generation {
                // Decoded before the latest seek.
                let _ = self.queues.recycle.push(chunk);
                continue;
            }
            if chunk.generation > self.generation {
                self.adopt_generation(chunk.generation);
            }
            self.current = Some(chunk);
            self.offset = 0;
            return true;
        }
        false
    }

    /// Switches to a new seek epoch: drops stale audio and fades the new audio in.
    fn adopt_generation(&mut self, generation: u32) {
        self.generation = generation;
        self.recycle_current();
        if !self.paused {
            self.gate.jump(0.0);
            self.gate.set_target(1.0, self.declick);
        }
    }

    fn recycle_current(&mut self) {
        if let Some(chunk) = self.current.take() {
            // Never full: the queue can hold every chunk of the stream.
            let _ = self.queues.recycle.push(chunk);
        }
        self.offset = 0;
    }
}

fn sanitize(volume: f32) -> f32 {
    if volume.is_finite() { volume.clamp(0.0, 1.0) } else { 0.0 }
}
