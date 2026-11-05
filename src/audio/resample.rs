//! Streaming polyphase windowed-sinc sample-rate converter for interleaved stereo.
//!
//! Output frame `j` is the band-limited interpolation of the input at position `j · M / L` input
//! frames, where `L / M` is the output/input rate ratio in lowest terms. The Kaiser-windowed sinc
//! kernel is precomputed for each of the `L` fractional phases (and shared between resamplers),
//! so each output sample costs `taps` multiply-adds. Input before the first frame and after the
//! end is silence, and output frame 0 lines up exactly with input frame 0: positions convert
//! between the two rates without any delay to compensate.

use std::collections::HashMap;
use std::f64::consts::PI;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

/// Kernel length when upsampling; scaled by the decimation factor when downsampling.
const BASE_TAPS: usize = 48;
const MAX_TAPS: usize = 256;
/// Largest phase count stored exactly; awkward ratios are approximated (negligible pitch error).
const MAX_PHASES: u64 = 4096;
/// Passband edge relative to the narrower Nyquist frequency.
const PASSBAND: f64 = 0.9;
/// Kaiser window shape (~80 dB stopband attenuation).
const KAISER_BETA: f64 = 8.0;
/// Consumed input frames allowed to accumulate before the history buffer is compacted.
const COMPACT_THRESHOLD: usize = 4096;

struct Kernel {
    up: u64,
    down: u64,
    taps: usize,
    /// `up` rows of `taps` coefficients, each row normalized to unity DC gain.
    coeffs: Box<[f32]>,
}

impl Kernel {
    fn new(input_rate: u32, output_rate: u32) -> Self {
        let (up, down) = rational_ratio(input_rate, output_rate);
        let decimation = down as f64 / up as f64;
        let taps = if decimation > 1.0 {
            ((BASE_TAPS as f64 * decimation).ceil() as usize).next_multiple_of(2).min(MAX_TAPS)
        } else {
            BASE_TAPS
        };
        // Cutoff in cycles per input frame, just below the narrower Nyquist frequency.
        let cutoff = 0.5 * PASSBAND * (up as f64 / down as f64).min(1.0);
        let half = (taps / 2) as f64;
        let i0_beta = bessel_i0(KAISER_BETA);
        let mut coeffs = vec![0.0f32; up as usize * taps];
        let mut row = vec![0.0f64; taps];
        for (phase, out) in coeffs.chunks_exact_mut(taps).enumerate() {
            let fraction = phase as f64 / up as f64;
            let mut sum = 0.0;
            for (tap, value) in row.iter_mut().enumerate() {
                // Distance (in input frames) from this tap to the interpolation point.
                let x = half - 1.0 - tap as f64 + fraction;
                *value = sinc(2.0 * cutoff * x) * kaiser(x / half, i0_beta);
                sum += *value;
            }
            for (dst, value) in out.iter_mut().zip(&row) {
                *dst = (value / sum) as f32;
            }
        }
        Self { up, down, taps, coeffs: coeffs.into_boxed_slice() }
    }
}

/// Converts interleaved stereo audio from one sample rate to another, incrementally.
pub(crate) struct Resampler {
    kernel: Arc<Kernel>,
    /// Interleaved input frames; `input[0..2]` is input frame `base`.
    input: Vec<f32>,
    base: i64,
    /// Real input frames received since the last reset.
    received: i64,
    /// Integer part of the next output frame's position in the input.
    position: i64,
    /// Fractional part of that position, in units of `1 / up` input frames.
    phase: u64,
    finished: bool,
}

impl Resampler {
    pub(crate) fn new(input_rate: u32, output_rate: u32) -> Self {
        let mut resampler = Self {
            kernel: kernel_for(input_rate, output_rate),
            input: Vec::new(),
            base: 0,
            received: 0,
            position: 0,
            phase: 0,
            finished: false,
        };
        resampler.reset();
        resampler
    }

    /// Output/input rate ratio as `(up, down)`.
    pub(crate) fn ratio(&self) -> (u64, u64) {
        (self.kernel.up, self.kernel.down)
    }

    /// Forgets all input (after a seek). The next input frame is aligned with output frame 0.
    pub(crate) fn reset(&mut self) {
        let half = self.kernel.taps / 2;
        self.input.clear();
        self.input.resize(half * 2, 0.0); // silence before the first frame
        self.base = -(half as i64);
        self.received = 0;
        self.position = 0;
        self.phase = 0;
        self.finished = false;
    }

    /// Appends interleaved stereo input.
    pub(crate) fn push(&mut self, frames: &[f32]) {
        debug_assert!(!self.finished, "push after finish");
        debug_assert_eq!(frames.len() % 2, 0);
        self.input.extend_from_slice(frames);
        self.received += (frames.len() / 2) as i64;
    }

    /// Declares the end of the input; the remaining output can then be drained.
    pub(crate) fn finish(&mut self) {
        if !self.finished {
            let half = self.kernel.taps / 2;
            self.input.resize(self.input.len() + half * 2, 0.0);
            self.finished = true;
        }
    }

    /// True once finished and every output frame has been produced.
    pub(crate) fn is_drained(&self) -> bool {
        self.finished && self.position >= self.received
    }

    /// Appends up to `max_frames` output frames to `out`; returns how many were produced.
    pub(crate) fn produce(&mut self, out: &mut Vec<f32>, max_frames: usize) -> usize {
        let kernel = &*self.kernel;
        let half = (kernel.taps / 2) as i64;
        let available = self.base + (self.input.len() / 2) as i64;
        let mut produced = 0;
        while produced < max_frames {
            if (self.finished && self.position >= self.received) || self.position + half >= available {
                break;
            }
            let first = (self.position - half + 1 - self.base) as usize;
            let coeffs = &kernel.coeffs[self.phase as usize * kernel.taps..][..kernel.taps];
            let window = &self.input[first * 2..(first + kernel.taps) * 2];
            let (mut left, mut right) = (0.0f32, 0.0f32);
            for (coeff, frame) in coeffs.iter().zip(window.chunks_exact(2)) {
                left += coeff * frame[0];
                right += coeff * frame[1];
            }
            out.push(left);
            out.push(right);
            produced += 1;
            self.phase += kernel.down;
            if self.phase >= kernel.up {
                let whole = self.phase / kernel.up;
                self.position += whole as i64;
                self.phase -= whole * kernel.up;
            }
        }
        self.compact();
        produced
    }

    /// Drops input history that no future output frame needs.
    fn compact(&mut self) {
        let half = (self.kernel.taps / 2) as i64;
        let needed_from = self.position - half + 1;
        let frames = (self.input.len() / 2) as i64;
        let removable = (needed_from - self.base).clamp(0, frames) as usize;
        if removable >= COMPACT_THRESHOLD {
            self.input.drain(..removable * 2);
            self.base += removable as i64;
        }
    }
}

fn kernel_for(input_rate: u32, output_rate: u32) -> Arc<Kernel> {
    static CACHE: OnceLock<Mutex<HashMap<(u32, u32), Arc<Kernel>>>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap_or_else(PoisonError::into_inner);
    Arc::clone(
        cache
            .entry((input_rate, output_rate))
            .or_insert_with(|| Arc::new(Kernel::new(input_rate, output_rate))),
    )
}

/// Output/input ratio `(up, down)` in lowest terms, approximated when `up` would exceed
/// [`MAX_PHASES`].
fn rational_ratio(input_rate: u32, output_rate: u32) -> (u64, u64) {
    let (input, output) = (u64::from(input_rate.max(1)), u64::from(output_rate.max(1)));
    let divisor = gcd(input, output);
    let (up, down) = (output / divisor, input / divisor);
    if up <= MAX_PHASES {
        return (up, down);
    }
    let target = output as f64 / input as f64;
    let mut best = (1, 1, f64::INFINITY);
    for candidate_up in 1..=MAX_PHASES {
        let candidate_down = ((candidate_up as f64 / target).round() as u64).max(1);
        let error = (candidate_up as f64 / candidate_down as f64 / target - 1.0).abs();
        if error < best.2 {
            best = (candidate_up, candidate_down, error);
        }
    }
    (best.0, best.1)
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a.max(1)
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 { 1.0 } else { (PI * x).sin() / (PI * x) }
}

fn kaiser(v: f64, i0_beta: f64) -> f64 {
    if v.abs() >= 1.0 { 0.0 } else { bessel_i0(KAISER_BETA * (1.0 - v * v).sqrt()) / i0_beta }
}

/// Zeroth-order modified Bessel function of the first kind (power series).
fn bessel_i0(x: f64) -> f64 {
    let quarter_x2 = x * x / 4.0;
    let (mut sum, mut term) = (1.0, 1.0);
    for k in 1..64 {
        term *= quarter_x2 / (k * k) as f64;
        sum += term;
        if term < sum * 1e-15 {
            break;
        }
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert(resampler: &mut Resampler, input: &[f32], piece_frames: usize) -> Vec<f32> {
        let mut out = Vec::new();
        for piece in input.chunks(piece_frames * 2) {
            resampler.push(piece);
            while resampler.produce(&mut out, 333) > 0 {}
        }
        resampler.finish();
        while resampler.produce(&mut out, 333) > 0 {}
        assert!(resampler.is_drained());
        out
    }

    fn sine(frames: usize, rate: f64, frequency: f64) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let s = (2.0 * PI * frequency * i as f64 / rate).sin() as f32;
                [s, -s]
            })
            .collect()
    }

    #[test]
    fn output_length_follows_the_ratio() {
        let out = convert(&mut Resampler::new(44_100, 48_000), &vec![0.0; 44_100 * 2], 1000);
        assert_eq!(out.len() / 2, 48_000);
        let out = convert(&mut Resampler::new(48_000, 44_100), &vec![0.0; 1000 * 2], 64);
        assert_eq!(out.len() / 2, (1000u64 * 147).div_ceil(160) as usize);
    }

    #[test]
    fn preserves_dc() {
        let out = convert(&mut Resampler::new(44_100, 48_000), &vec![0.5; 4000 * 2], 500);
        for sample in &out[200..out.len() - 200] {
            assert!((sample - 0.5).abs() < 1e-3, "{sample}");
        }
    }

    #[test]
    fn preserves_a_sine_with_no_delay() {
        let input = sine(44_100 / 2, 44_100.0, 1000.0);
        let out = convert(&mut Resampler::new(44_100, 48_000), &input, 777);
        let expected = sine(out.len() / 2, 48_000.0, 1000.0);
        let start = 200 * 2;
        let end = out.len() - 200 * 2;
        let worst = out[start..end].iter().zip(&expected[start..end]).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(worst < 5e-3, "max deviation {worst}");
    }

    #[test]
    fn chunking_does_not_change_the_output() {
        let input = sine(5000, 22_050.0, 440.0);
        let one_shot = convert(&mut Resampler::new(22_050, 48_000), &input, 5000);
        let chunked = convert(&mut Resampler::new(22_050, 48_000), &input, 37);
        assert_eq!(one_shot, chunked);
    }

    #[test]
    fn downsampling_removes_content_above_the_new_nyquist() {
        let input = sine(96_000 / 4, 96_000.0, 30_000.0);
        let out = convert(&mut Resampler::new(96_000, 48_000), &input, 1024);
        let middle = &out[400..out.len() - 400];
        let rms = (middle.iter().map(|s| s * s).sum::<f32>() / middle.len() as f32).sqrt();
        assert!(rms < 1e-3, "alias rms {rms}");
    }

    #[test]
    fn reset_restarts_alignment() {
        let mut resampler = Resampler::new(44_100, 48_000);
        let input = sine(3000, 44_100.0, 500.0);
        let first = convert(&mut resampler, &input, 100);
        resampler.reset();
        let second = convert(&mut resampler, &input, 300);
        assert_eq!(first, second);
    }

    #[test]
    fn awkward_ratios_are_approximated_closely() {
        let (up, down) = rational_ratio(44_056, 48_000);
        assert!(up <= MAX_PHASES);
        let error = (up as f64 / down as f64) / (48_000.0 / 44_056.0) - 1.0;
        assert!(error.abs() < 1e-6, "{error}");
        assert_eq!(rational_ratio(44_100, 48_000), (160, 147));
    }
}
