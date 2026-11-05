//! Producer side of a voice, run on the engine thread.
//!
//! A [`Stream`] decodes its file ahead of playback into fixed-size [`Chunk`]s and passes them to
//! the voice through a lock-free queue; the voice hands empty chunks back for reuse, so steady
//! state playback allocates nothing. Everything that depends only on the file's own timeline is
//! rendered here rather than on the audio thread:
//!
//! * the fade-in at the start of playback;
//! * gapless loops (the decoder rewinds and the resampler keeps its state across the seam);
//! * Kenku FM's crossfade loops: `fade_out` before the end, a second decoder starts from the
//!   beginning fading in while the first fades out;
//! * the fade-out before the natural end of a non-looping sound.
//!
//! Chunks never straddle a timeline discontinuity (loop restart, seek), so each chunk carries the
//! exact position of its first frame.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;

use rtrb::{Consumer, Producer, PushError, RingBuffer};

use super::api::AudioError;
use super::decoder::Decoder;
use super::frames_u32;
use super::ramp::Ramp;
use super::resample::Resampler;
use super::CHANNELS;

/// Frames per chunk (~21 ms at 48 kHz).
pub(crate) const CHUNK_FRAMES: usize = 1024;
/// Chunks per voice: how far ahead of playback the engine decodes (~0.5 s at 48 kHz).
pub(crate) const CHUNK_COUNT: usize = 24;
/// Chunks decoded before a voice is handed to the mixer.
pub(crate) const PREFILL_CHUNKS: usize = 4;
/// Output frames produced per resampler call.
const REFILL_FRAMES: usize = 1024;

/// A block of decoded audio travelling from the engine thread to a voice.
pub(crate) struct Chunk {
    /// Seek epoch the chunk was decoded in; voices drop chunks from older epochs.
    pub(crate) generation: u32,
    /// Timeline position (frames) of the first frame.
    pub(crate) position: u64,
    /// Valid frames in `samples`.
    pub(crate) frames: usize,
    /// No chunk follows: the voice ends after this one.
    pub(crate) last: bool,
    /// Interleaved stereo, `CHUNK_FRAMES` frames of capacity.
    pub(crate) samples: Box<[f32]>,
}

impl Chunk {
    fn boxed() -> Box<Self> {
        Box::new(Self {
            generation: 0,
            position: 0,
            frames: 0,
            last: false,
            samples: vec![0.0; CHUNK_FRAMES * CHANNELS].into_boxed_slice(),
        })
    }
}

/// The voice's ends of a stream's queues.
pub(crate) struct ChunkQueues {
    pub(crate) chunks: Consumer<Box<Chunk>>,
    pub(crate) recycle: Producer<Box<Chunk>>,
}

/// How a stream shapes its timeline. Durations are in output frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StreamParams {
    pub(crate) looping: bool,
    pub(crate) fade_in: u64,
    pub(crate) fade_out: u64,
}

pub(crate) struct Stream {
    path: Arc<Path>,
    sample_rate: u32,
    params: StreamParams,
    duration: Option<u64>,
    /// Active decoding passes; the last one is the primary and owns the timeline. More than one
    /// only while crossfading.
    instances: Vec<Instance>,
    generation: u32,
    finished: bool,
    to_voice: Producer<Box<Chunk>>,
    recycled: Consumer<Box<Chunk>>,
    spare: Vec<Box<Chunk>>,
    scratch: Vec<f32>,
}

struct Instance {
    reader: Reader,
    gain: Ramp,
    /// Its fade-out has started (crossfade or end fade); it will be dropped once silent.
    fading_out: bool,
}

impl Stream {
    /// Opens the file and allocates the chunk queues shared with the voice.
    pub(crate) fn open(path: &Path, sample_rate: u32, params: StreamParams) -> Result<(Self, ChunkQueues), AudioError> {
        let path: Arc<Path> = Arc::from(path);
        let reader = Reader::open(Arc::clone(&path), sample_rate)?;
        let duration = reader.duration();
        let mut gain = Ramp::new(if params.fade_in > 0 { 0.0 } else { 1.0 });
        gain.set_target(1.0, frames_u32(params.fade_in));
        let (to_voice, chunks) = RingBuffer::new(CHUNK_COUNT);
        let (recycle, recycled) = RingBuffer::new(CHUNK_COUNT);
        let mut stream = Self {
            path,
            sample_rate,
            params,
            duration,
            instances: vec![Instance { reader, gain, fading_out: false }],
            generation: 0,
            finished: false,
            to_voice,
            recycled,
            spare: (0..CHUNK_COUNT).map(|_| Chunk::boxed()).collect(),
            scratch: Vec::with_capacity(CHUNK_FRAMES * CHANNELS),
        };
        stream.apply_loop_policy();
        Ok((stream, ChunkQueues { chunks, recycle }))
    }

    /// Length of one iteration of the file in output frames, if known.
    pub(crate) fn duration(&self) -> Option<u64> {
        self.duration
    }

    /// Decodes up to `max_chunks` chunks into the voice's queue; returns how many were queued.
    pub(crate) fn fill(&mut self, max_chunks: usize) -> usize {
        let mut queued = 0;
        while queued < max_chunks && !self.finished {
            let Some(mut chunk) = self.spare.pop().or_else(|| self.recycled.pop().ok()) else {
                break;
            };
            self.render_chunk(&mut chunk);
            if chunk.frames == 0 && !chunk.last {
                self.spare.push(chunk);
                break;
            }
            match self.to_voice.push(chunk) {
                Ok(()) => queued += 1,
                // Cannot happen: the queue holds every chunk this stream owns.
                Err(PushError::Full(chunk)) => {
                    self.spare.push(chunk);
                    break;
                }
            }
        }
        queued
    }

    /// Restarts decoding at `position` (output frames) in a new seek epoch.
    pub(crate) fn seek(&mut self, position: u64, generation: u32) {
        self.generation = generation;
        self.finished = false;
        // Any crossfade in progress is abandoned: keep only the primary decoder.
        let primary = self.instances.pop();
        self.instances.clear();
        let reader = match primary {
            Some(instance) => Ok(instance.reader),
            None => Reader::open(Arc::clone(&self.path), self.sample_rate),
        };
        let result = reader.and_then(|mut reader| reader.seek(position).map(|()| reader));
        match result {
            Ok(reader) => {
                self.instances.push(Instance { reader, gain: Ramp::new(1.0), fading_out: false });
                self.apply_loop_policy();
            }
            // With no instance left, the next chunk is marked last and the voice ends.
            Err(error) => log::warn!("{}: {error}", self.path.display()),
        }
    }

    pub(crate) fn set_looping(&mut self, looping: bool) {
        if self.params.looping != looping {
            self.params.looping = looping;
            self.apply_loop_policy();
        }
    }

    fn crossfade_enabled(&self) -> bool {
        let StreamParams { looping, fade_in, fade_out } = self.params;
        // Each iteration must play alone for a while, or instances would pile up.
        looping && fade_in > 0 && fade_out > 0 && self.duration.is_some_and(|d| d > fade_out.saturating_mul(2))
    }

    /// Gapless looping is done by the primary decoder itself; crossfade loops by this stream.
    fn apply_loop_policy(&mut self) {
        let gapless = self.params.looping && !self.crossfade_enabled();
        if let Some(primary) = self.instances.last_mut() {
            primary.reader.set_looping(gapless);
        }
    }

    fn position(&self) -> u64 {
        self.instances.last().map_or(0, |instance| instance.reader.position())
    }

    /// Timeline position at which the primary starts fading out, if any.
    fn trigger_point(&self) -> Option<u64> {
        let primary = self.instances.last()?;
        if primary.fading_out || self.params.fade_out == 0 {
            return None;
        }
        let duration = self.duration?;
        if self.crossfade_enabled() {
            Some(duration - self.params.fade_out)
        } else if self.params.looping {
            None
        } else {
            Some(duration.saturating_sub(self.params.fade_out))
        }
    }

    /// Starts the fade due at the primary's position. Returns `true` when a new decoder took over
    /// the timeline (crossfade loop).
    fn fire_trigger(&mut self) -> bool {
        let Some(point) = self.trigger_point() else {
            return false;
        };
        let crossfade = self.crossfade_enabled();
        let duration = self.duration.unwrap_or(0);
        let fade_out = self.params.fade_out;
        let primary = self.instances.last_mut().expect("a trigger implies a primary");
        let position = primary.reader.position();
        if position < point {
            return false;
        }
        // Normally exactly `fade_out`; shorter when starting late (e.g. after a seek).
        let fade = duration.saturating_sub(position).clamp(1, fade_out.max(1));
        primary.fading_out = true;
        primary.gain.set_target(0.0, frames_u32(fade));
        if !crossfade {
            return false;
        }
        primary.reader.set_looping(false);
        match Reader::open(Arc::clone(&self.path), self.sample_rate) {
            Ok(reader) => {
                let mut gain = Ramp::new(0.0);
                gain.set_target(1.0, frames_u32(self.params.fade_in));
                self.instances.push(Instance { reader, gain, fading_out: false });
                self.apply_loop_policy();
                true
            }
            Err(error) => {
                log::warn!("{}: cannot reopen for the loop crossfade: {error}", self.path.display());
                false
            }
        }
    }

    /// Frames the primary can play before something changes (loop point, fade start).
    fn frames_until_event(&self) -> usize {
        let primary = self.instances.last().expect("caller checked");
        let mut frames = primary.reader.frames_until_boundary();
        if let Some(point) = self.trigger_point() {
            frames = frames.min(point.saturating_sub(primary.reader.position()));
        }
        usize::try_from(frames).unwrap_or(usize::MAX).max(1)
    }

    fn render_chunk(&mut self, chunk: &mut Chunk) {
        chunk.generation = self.generation;
        chunk.frames = 0;
        chunk.last = false;
        chunk.samples.fill(0.0);
        chunk.position = self.position();
        loop {
            if self.instances.is_empty() {
                chunk.last = true;
                self.finished = true;
                return;
            }
            let mut discontinuity = self.fire_trigger();
            let primary = self.instances.last_mut().expect("checked above");
            if primary.reader.end_at_boundary() {
                // Looping was switched off after the decoder had already wrapped around.
                self.instances.clear();
                continue;
            }
            while primary.reader.at_boundary() {
                primary.reader.cross_boundary();
                discontinuity = true;
            }
            if discontinuity {
                if chunk.frames > 0 {
                    return;
                }
                chunk.position = self.position();
            }
            let span = (CHUNK_FRAMES - chunk.frames).min(self.frames_until_event());
            let start = chunk.frames * CHANNELS;
            let produced = self.mix(&mut chunk.samples[start..start + span * CHANNELS], span);
            chunk.frames += produced;
            if chunk.frames == CHUNK_FRAMES {
                return;
            }
            if produced == 0 && !self.instances.is_empty() {
                debug_assert!(false, "stream made no progress");
                self.instances.clear();
            }
        }
    }

    /// Mixes `frames` frames of every instance into `out`; returns how many the primary produced.
    fn mix(&mut self, out: &mut [f32], frames: usize) -> usize {
        let Some((primary, older)) = self.instances.split_last_mut() else {
            return 0;
        };
        self.scratch.clear();
        let produced = primary.reader.read(&mut self.scratch, frames);
        add_scaled(out, &self.scratch, &mut primary.gain);
        for instance in older {
            self.scratch.clear();
            instance.reader.read(&mut self.scratch, produced);
            add_scaled(out, &self.scratch, &mut instance.gain);
        }
        let done = |instance: &Instance| instance.reader.is_exhausted() || (instance.fading_out && instance.gain.is_silent());
        if self.instances.last().is_some_and(done) {
            // The timeline owner ended: so does the stream.
            self.instances.clear();
        } else {
            self.instances.retain(|instance| !done(instance));
        }
        produced
    }
}

fn add_scaled(out: &mut [f32], source: &[f32], gain: &mut Ramp) {
    for (dst, src) in out.chunks_exact_mut(CHANNELS).zip(source.chunks_exact(CHANNELS)) {
        let gain = gain.tick();
        dst[0] += src[0] * gain;
        dst[1] += src[1] * gain;
    }
}

/// One decoding pass over a file at the engine's sample rate, with optional gapless looping.
struct Reader {
    path: Arc<Path>,
    decoder: Decoder,
    resampler: Option<Resampler>,
    /// Output/input rate ratio.
    up: u64,
    down: u64,
    duration: Option<u64>,
    /// Converted frames not yet read.
    pending: Vec<f32>,
    pending_pos: usize,
    decoded: Vec<f32>,
    looping: bool,
    /// Looping was disabled after wrapping: stop at the next loop boundary.
    end_at_boundary: bool,
    /// Frames read since the last seek.
    delivered: u64,
    /// Input frames decoded since the last seek.
    fed: u64,
    /// Input frames decoded in the current iteration (guards against looping empty files).
    iteration_frames: u64,
    /// `position = delivered - origin`.
    origin: i64,
    /// Values of `delivered` at which a new loop iteration begins.
    boundaries: VecDeque<u64>,
    input_done: bool,
    exhausted: bool,
}

impl Reader {
    fn open(path: Arc<Path>, output_rate: u32) -> Result<Self, AudioError> {
        let decoder = Decoder::open(&path)?;
        let input_rate = decoder.sample_rate();
        let resampler = (input_rate != output_rate).then(|| Resampler::new(input_rate, output_rate));
        let (up, down) = resampler.as_ref().map_or((1, 1), Resampler::ratio);
        let duration = decoder.frames().map(|frames| (frames * up).div_ceil(down));
        Ok(Self {
            path,
            decoder,
            resampler,
            up,
            down,
            duration,
            pending: Vec::with_capacity(REFILL_FRAMES * CHANNELS),
            pending_pos: 0,
            decoded: Vec::new(),
            looping: false,
            end_at_boundary: false,
            delivered: 0,
            fed: 0,
            iteration_frames: 0,
            origin: 0,
            boundaries: VecDeque::new(),
            input_done: false,
            exhausted: false,
        })
    }

    fn duration(&self) -> Option<u64> {
        self.duration
    }

    /// Timeline position (output frames) of the next frame.
    fn position(&self) -> u64 {
        u64::try_from(self.delivered as i64 - self.origin).unwrap_or(0)
    }

    fn set_looping(&mut self, looping: bool) {
        self.looping = looping;
        // Frames of the next iteration may already be decoded; honour the change at the seam
        // instead of playing a whole extra iteration.
        self.end_at_boundary = !looping && !self.boundaries.is_empty();
    }

    fn frames_until_boundary(&self) -> u64 {
        self.boundaries.front().map_or(u64::MAX, |&boundary| boundary.saturating_sub(self.delivered))
    }

    fn at_boundary(&self) -> bool {
        self.boundaries.front().is_some_and(|&boundary| boundary <= self.delivered)
    }

    fn end_at_boundary(&self) -> bool {
        self.end_at_boundary && self.at_boundary()
    }

    /// Starts the next loop iteration: the position restarts at zero.
    fn cross_boundary(&mut self) {
        if let Some(boundary) = self.boundaries.pop_front() {
            self.origin = boundary as i64;
        }
    }

    fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    fn seek(&mut self, position: u64) -> Result<(), AudioError> {
        let source_frame = position * self.down / self.up;
        self.decoder.seek(source_frame)?;
        if let Some(resampler) = self.resampler.as_mut() {
            resampler.reset();
        }
        self.pending.clear();
        self.pending_pos = 0;
        self.delivered = 0;
        self.fed = 0;
        self.iteration_frames = 0;
        self.origin = -((source_frame * self.up / self.down) as i64);
        self.boundaries.clear();
        self.end_at_boundary = false;
        self.input_done = false;
        self.exhausted = false;
        Ok(())
    }

    /// Appends up to `max_frames` frames to `out`, stopping early at a loop boundary or at the end.
    fn read(&mut self, out: &mut Vec<f32>, max_frames: usize) -> usize {
        let mut read = 0;
        while read < max_frames {
            if self.pending_pos >= self.pending.len() && !self.refill() {
                break;
            }
            // Re-evaluated after each refill, which may discover a new loop boundary.
            let until_boundary = usize::try_from(self.frames_until_boundary()).unwrap_or(usize::MAX);
            if until_boundary == 0 {
                break;
            }
            let available = (self.pending.len() - self.pending_pos) / CHANNELS;
            let take = available.min(max_frames - read).min(until_boundary);
            out.extend_from_slice(&self.pending[self.pending_pos..][..take * CHANNELS]);
            self.pending_pos += take * CHANNELS;
            self.delivered += take as u64;
            read += take;
        }
        read
    }

    /// Converts more audio into `pending`. Returns `false` once the reader is exhausted.
    fn refill(&mut self) -> bool {
        self.pending.clear();
        self.pending_pos = 0;
        if self.exhausted {
            return false;
        }
        loop {
            if let Some(resampler) = self.resampler.as_mut() {
                if resampler.produce(&mut self.pending, REFILL_FRAMES) > 0 {
                    return true;
                }
                if resampler.is_drained() {
                    self.exhausted = true;
                    return false;
                }
            } else if self.input_done {
                self.exhausted = true;
                return false;
            }

            self.decoded.clear();
            let more = match self.decoder.decode_into(&mut self.decoded) {
                Ok(more) => more,
                Err(error) => {
                    log::warn!("{}: {error}", self.path.display());
                    false
                }
            };
            if more {
                let frames = (self.decoded.len() / CHANNELS) as u64;
                self.fed += frames;
                self.iteration_frames += frames;
                match self.resampler.as_mut() {
                    Some(resampler) => resampler.push(&self.decoded),
                    None => {
                        std::mem::swap(&mut self.pending, &mut self.decoded);
                        if !self.pending.is_empty() {
                            return true;
                        }
                    }
                }
                continue;
            }

            // End of the file.
            if self.looping && self.iteration_frames > 0 && self.rewind() {
                // First output frame whose input position is at or past the seam.
                self.boundaries.push_back((self.fed * self.up).div_ceil(self.down));
                self.iteration_frames = 0;
                continue;
            }
            self.input_done = true;
            if let Some(resampler) = self.resampler.as_mut() {
                resampler.finish();
            }
        }
    }

    fn rewind(&mut self) -> bool {
        if self.decoder.seek(0).is_ok() {
            return true;
        }
        // Some containers cannot seek; reopening is the fallback.
        match Decoder::open(&self.path) {
            Ok(decoder) => {
                self.decoder = decoder;
                true
            }
            Err(error) => {
                log::warn!("{}: cannot restart the loop: {error}", self.path.display());
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::decoder::test_wav::{quantized, temp_path, write};
    use super::*;

    const RATE: u32 = 48_000;

    struct Output {
        /// Left channel of every frame, in order.
        left: Vec<f32>,
        /// `(position, frames, generation, last)` of every chunk.
        chunks: Vec<(u64, usize, u32, bool)>,
    }

    /// Drains the stream completely (or up to `limit` frames), recycling chunks like a voice.
    fn drain(stream: &mut Stream, queues: &mut ChunkQueues, limit: usize) -> Output {
        let mut output = Output { left: Vec::new(), chunks: Vec::new() };
        loop {
            stream.fill(CHUNK_COUNT);
            let mut progressed = false;
            while let Ok(chunk) = queues.chunks.pop() {
                progressed = true;
                output.chunks.push((chunk.position, chunk.frames, chunk.generation, chunk.last));
                output.left.extend(chunk.samples[..chunk.frames * 2].iter().step_by(2));
                let last = chunk.last;
                queues.recycle.push(chunk).unwrap();
                if last || output.left.len() >= limit {
                    return output;
                }
            }
            assert!(progressed, "stream stalled");
        }
    }

    /// A mono WAV whose sample `i` encodes `i` (so positions are recoverable from the audio).
    fn ramp_file(name: &str, frames: usize, rate: u32) -> (std::path::PathBuf, Vec<f32>) {
        let path = temp_path(name);
        let samples: Vec<f32> = (0..frames).map(|i| (i % 30_000) as f32 / 32_768.0).collect();
        write(&path, rate, 1, &samples);
        let decoded = samples.iter().map(|&s| quantized(s)).collect();
        (path, decoded)
    }

    fn params(looping: bool, fade_in: u64, fade_out: u64) -> StreamParams {
        StreamParams { looping, fade_in, fade_out }
    }

    #[test]
    fn plays_the_file_once_with_exact_positions() {
        let (path, expected) = ramp_file("plain", 5000, RATE);
        let (mut stream, mut queues) = Stream::open(&path, RATE, params(false, 0, 0)).unwrap();
        assert_eq!(stream.duration(), Some(5000));
        let out = drain(&mut stream, &mut queues, usize::MAX);
        assert_eq!(out.left, expected);
        let mut position = 0;
        for &(chunk_position, frames, generation, _) in &out.chunks {
            assert_eq!((chunk_position, generation), (position, 0));
            position += frames as u64;
        }
        assert!(out.chunks.last().unwrap().3);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn gapless_loop_repeats_seamlessly_and_restarts_positions() {
        let (path, expected) = ramp_file("gapless", 1500, RATE);
        let (mut stream, mut queues) = Stream::open(&path, RATE, params(true, 0, 0)).unwrap();
        let out = drain(&mut stream, &mut queues, 1500 * 3);
        let repeated: Vec<f32> = expected.iter().cycle().take(out.left.len()).copied().collect();
        assert_eq!(out.left, repeated);
        // Chunks are split at the seam and restart at position 0.
        let restarts = out.chunks.iter().filter(|chunk| chunk.0 == 0).count();
        assert!(restarts >= 3, "{:?}", out.chunks);
        for &(position, frames, _, _) in &out.chunks {
            assert!(position + frames as u64 <= 1500);
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn disabling_the_loop_ends_at_the_next_seam() {
        let (path, _) = ramp_file("unloop", 700, RATE);
        let (mut stream, mut queues) = Stream::open(&path, RATE, params(true, 0, 0)).unwrap();
        // Decodes ahead past several seams, then turns looping off.
        stream.fill(3);
        stream.set_looping(false);
        let out = drain(&mut stream, &mut queues, usize::MAX);
        let total = out.left.len();
        assert_eq!(total % 700, 0, "ended mid-iteration: {total}");
        assert!(total <= 3 * CHUNK_FRAMES + 700);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn fades_in_at_the_start_and_out_at_the_end() {
        let path = temp_path("fades");
        write(&path, RATE, 1, &vec![0.5; 4000]);
        let level = quantized(0.5);
        let (mut stream, mut queues) = Stream::open(&path, RATE, params(false, 100, 200)).unwrap();
        let out = drain(&mut stream, &mut queues, usize::MAX);
        assert_eq!(out.left.len(), 4000);
        assert!((out.left[0] - level * 0.01).abs() < 1e-6);
        assert!((out.left[99] - level).abs() < 1e-6);
        assert!((out.left[2000] - level).abs() < 1e-6);
        assert!((out.left[3800] - level * (199.0 / 200.0)).abs() < 1e-5);
        assert!(out.left[3999].abs() < 1e-6);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn crossfade_loop_overlaps_iterations() {
        let (path, expected) = ramp_file("xfade", 3000, RATE);
        let (fade_in, fade_out) = (300u64, 500u64);
        let (mut stream, mut queues) = Stream::open(&path, RATE, params(true, fade_in, fade_out)).unwrap();
        let out = drain(&mut stream, &mut queues, 3000 + 2500 + 1000);
        let point = 3000 - fade_out as usize; // second iteration starts here
        // Before the crossfade: the first iteration alone (after its own fade-in).
        for i in fade_in as usize..point {
            assert!((out.left[i] - expected[i]).abs() < 1e-6, "frame {i}");
        }
        // During the crossfade: old iteration fading out + new iteration fading in.
        for k in 0..fade_out as usize {
            let old = expected[point + k] * (1.0 - (k + 1) as f32 / fade_out as f32);
            let new = expected[k] * ((k + 1) as f32 / fade_in as f32).min(1.0);
            assert!((out.left[point + k] - (old + new)).abs() < 1e-4, "crossfade frame {k}");
        }
        // After it: the second iteration alone, and the timeline restarted at the crossfade.
        for k in fade_out as usize..2000 {
            assert!((out.left[point + k] - expected[k]).abs() < 1e-6, "second iteration frame {k}");
        }
        let restart = out.chunks.iter().skip(1).position(|chunk| chunk.0 == 0).map(|index| index + 1).unwrap();
        let before: usize = out.chunks[..restart].iter().map(|chunk| chunk.1).sum();
        assert_eq!(before, point);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn seek_starts_a_new_generation_at_the_target() {
        let (path, expected) = ramp_file("seek", 10_000, RATE);
        let (mut stream, mut queues) = Stream::open(&path, RATE, params(false, 0, 0)).unwrap();
        stream.fill(2);
        // Drop what was queued, as a voice does after a seek.
        while let Ok(chunk) = queues.chunks.pop() {
            queues.recycle.push(chunk).unwrap();
        }
        stream.seek(6000, 1);
        let out = drain(&mut stream, &mut queues, usize::MAX);
        assert_eq!(out.chunks[0].0, 6000);
        assert!(out.chunks.iter().all(|chunk| chunk.2 == 1));
        assert_eq!(out.left, expected[6000..]);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn resampled_streams_keep_the_timeline_in_output_frames() {
        let path = temp_path("resampled");
        write(&path, 44_100, 1, &vec![0.25; 44_100]);
        let (mut stream, mut queues) = Stream::open(&path, RATE, params(true, 0, 0)).unwrap();
        assert_eq!(stream.duration(), Some(48_000));
        let out = drain(&mut stream, &mut queues, 48_000 * 2 + 5000);
        let level = quantized(0.25);
        // The loop seam is seamless: no dip anywhere past the initial filter warm-up.
        for (i, sample) in out.left.iter().enumerate().skip(100) {
            assert!((sample - level).abs() < 2e-3, "frame {i}: {sample}");
        }
        let seam = out.chunks.iter().skip(1).position(|chunk| chunk.0 == 0).map(|index| index + 1).unwrap();
        let before: usize = out.chunks[..seam].iter().map(|chunk| chunk.1).sum();
        assert!((before as i64 - 48_000).abs() <= 1, "seam at {before}");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn opening_a_missing_file_fails() {
        assert!(Stream::open(Path::new("/no/such/file.ogg"), RATE, params(false, 0, 0)).is_err());
    }
}
