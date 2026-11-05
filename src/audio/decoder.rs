//! File decoding with Symphonia: any supported container/codec → interleaved stereo `f32` at the
//! file's own sample rate.

use std::fs::File;
use std::io;
use std::path::Path;

use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{Time, TimeBase};

use super::api::AudioError;

/// Consecutive corrupt packets tolerated before a file is declared undecodable.
const MAX_CONSECUTIVE_DECODE_ERRORS: u32 = 64;

pub(crate) struct Decoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    sample_rate: u32,
    time_base: Option<TimeBase>,
    frames: Option<u64>,
    /// Interleaved samples of the last packet, in the file's channel layout.
    samples: Vec<f32>,
    /// Frames still to discard after an accurate seek.
    skip: u64,
}

impl Decoder {
    pub(crate) fn open(path: &Path) -> Result<Self, AudioError> {
        let file = File::open(path).map_err(|source| AudioError::Open { path: path.to_owned(), source })?;
        let stream = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
        let mut hint = Hint::new();
        if let Some(extension) = path.extension().and_then(|extension| extension.to_str()) {
            hint.with_extension(extension);
        }
        let format = symphonia::default::get_probe()
            .probe(&hint, stream, FormatOptions::default(), MetadataOptions::default())
            .map_err(|error| match error {
                SymphoniaError::IoError(source) => AudioError::Open { path: path.to_owned(), source },
                other => AudioError::Unsupported(other.to_string()),
            })?;

        let track = format.default_track(TrackType::Audio).ok_or(AudioError::NoAudioTrack)?;
        let Some(CodecParameters::Audio(params)) = &track.codec_params else {
            return Err(AudioError::NoAudioTrack);
        };
        let sample_rate = params
            .sample_rate
            .filter(|&rate| rate > 0)
            .ok_or_else(|| AudioError::Unsupported("unknown sample rate".into()))?;
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map_err(|error| AudioError::Unsupported(error.to_string()))?;

        let track_id = track.id;
        let time_base = track.time_base;
        // Playable frames (encoder delay and padding excluded), else the container duration.
        let frames = track.num_frames.or_else(|| {
            let seconds = time_base?.calc_duration(track.duration?)?.as_secs_f64();
            Some((seconds * f64::from(sample_rate)).round() as u64)
        });

        Ok(Self { format, decoder, track_id, sample_rate, time_base, frames, samples: Vec::new(), skip: 0 })
    }

    pub(crate) fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Length in frames at [`sample_rate`](Self::sample_rate), if the container states it.
    pub(crate) fn frames(&self) -> Option<u64> {
        self.frames
    }

    /// Decodes the next packet, appending stereo interleaved frames to `out`.
    /// Returns `Ok(false)` at the end of the stream.
    pub(crate) fn decode_into(&mut self, out: &mut Vec<f32>) -> Result<bool, AudioError> {
        let mut errors = 0;
        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(packet)) => packet,
                Ok(None) => return Ok(false),
                Err(SymphoniaError::IoError(error)) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(false),
                Err(error) => return Err(AudioError::Decode(error.to_string())),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(buffer) => {
                    let channels = buffer.spec().channels().count();
                    let frames = buffer.frames();
                    if channels == 0 || frames == 0 {
                        continue;
                    }
                    buffer.copy_to_vec_interleaved(&mut self.samples);
                    let skipped = self.skip.min(frames as u64) as usize;
                    self.skip -= skipped as u64;
                    push_stereo(&self.samples[skipped * channels..], channels, out);
                    return Ok(true);
                }
                Err(SymphoniaError::DecodeError(reason)) => {
                    // A corrupt frame: skip it, like every media player does.
                    errors += 1;
                    if errors > MAX_CONSECUTIVE_DECODE_ERRORS {
                        return Err(AudioError::Decode(reason.to_string()));
                    }
                    log::debug!("skipping corrupt packet: {reason}");
                }
                Err(SymphoniaError::IoError(error)) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(false),
                Err(error) => return Err(AudioError::Decode(error.to_string())),
            }
        }
    }

    /// Positions the decoder so that the next decoded frame is exactly `frame`.
    pub(crate) fn seek(&mut self, frame: u64) -> Result<(), AudioError> {
        let seconds = frame as f64 / f64::from(self.sample_rate);
        let time = Time::try_from_secs_f64(seconds).ok_or_else(|| AudioError::Seek(format!("bad position {seconds}s")))?;
        let seeked = self
            .format
            .seek(SeekMode::Accurate, SeekTo::Time { time, track_id: Some(self.track_id) })
            .map_err(|error| AudioError::Seek(error.to_string()))?;
        self.decoder.reset();
        // Accurate seeks land on a packet boundary at or before the target.
        let ticks = seeked.required_ts.get().saturating_sub(seeked.actual_ts.get()).max(0) as u64;
        self.skip = self.ticks_to_frames(ticks);
        Ok(())
    }

    fn ticks_to_frames(&self, ticks: u64) -> u64 {
        match self.time_base {
            Some(base) => {
                let frames = u128::from(ticks) * u128::from(base.numer.get()) * u128::from(self.sample_rate)
                    / u128::from(base.denom.get());
                u64::try_from(frames).unwrap_or(u64::MAX)
            }
            None => ticks,
        }
    }
}

/// Appends `samples` (interleaved, `channels` per frame) as stereo. Mono is duplicated to both
/// sides; multichannel keeps the front pair.
fn push_stereo(samples: &[f32], channels: usize, out: &mut Vec<f32>) {
    match channels {
        1 => out.extend(samples.iter().flat_map(|&sample| [sample, sample])),
        2 => out.extend_from_slice(samples),
        _ => out.extend(samples.chunks_exact(channels).flat_map(|frame| [frame[0], frame[1]])),
    }
}

#[cfg(test)]
pub(crate) mod test_wav {
    //! Writes small PCM WAV files for tests.

    use std::path::{Path, PathBuf};

    /// A unique path in the system temp directory.
    pub(crate) fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("kenkurs-{name}-{}.wav", crate::util::id::new_id()))
    }

    /// Writes 16-bit PCM. `samples` are interleaved, in `-1.0..=1.0`.
    pub(crate) fn write(path: &Path, sample_rate: u32, channels: u16, samples: &[f32]) {
        let data_len = (samples.len() * 2) as u32;
        let mut bytes = Vec::with_capacity(44 + data_len as usize);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
        bytes.extend_from_slice(&channels.to_le_bytes());
        bytes.extend_from_slice(&sample_rate.to_le_bytes());
        bytes.extend_from_slice(&(sample_rate * u32::from(channels) * 2).to_le_bytes());
        bytes.extend_from_slice(&(channels * 2).to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        for sample in samples {
            bytes.extend_from_slice(&((sample.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes());
        }
        std::fs::write(path, bytes).unwrap();
    }

    /// The value 16-bit PCM stores for `sample`, converted back to `f32` the way Symphonia does.
    pub(crate) fn quantized(sample: f32) -> f32 {
        ((sample.clamp(-1.0, 1.0) * 32767.0).round() as i16) as f32 / 32768.0
    }
}

#[cfg(test)]
mod tests {
    use super::test_wav::{quantized, temp_path, write};
    use super::*;

    fn decode_all(decoder: &mut Decoder) -> Vec<f32> {
        let mut out = Vec::new();
        while decoder.decode_into(&mut out).unwrap() {}
        out
    }

    #[test]
    fn decodes_stereo_wav_exactly() {
        let path = temp_path("stereo");
        let samples: Vec<f32> = (0..2000).map(|i| ((i % 200) as f32 - 100.0) / 128.0).collect();
        write(&path, 48_000, 2, &samples);
        let mut decoder = Decoder::open(&path).unwrap();
        assert_eq!(decoder.sample_rate(), 48_000);
        assert_eq!(decoder.frames(), Some(1000));
        let decoded = decode_all(&mut decoder);
        let expected: Vec<f32> = samples.iter().map(|&s| quantized(s)).collect();
        assert_eq!(decoded, expected);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn mono_is_duplicated_to_both_channels() {
        let path = temp_path("mono");
        write(&path, 44_100, 1, &[0.25, -0.5, 0.75]);
        let mut decoder = Decoder::open(&path).unwrap();
        let decoded = decode_all(&mut decoder);
        let (a, b, c) = (quantized(0.25), quantized(-0.5), quantized(0.75));
        assert_eq!(decoded, [a, a, b, b, c, c]);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn seeks_are_sample_accurate() {
        let path = temp_path("seek");
        let samples: Vec<f32> = (0..48_000).map(|i| (i % 1000) as f32 / 1000.0).collect();
        write(&path, 48_000, 1, &samples);
        let mut decoder = Decoder::open(&path).unwrap();
        for target in [12_345u64, 0, 47_999, 30_000] {
            decoder.seek(target).unwrap();
            let mut out = Vec::new();
            assert!(decoder.decode_into(&mut out).unwrap());
            assert_eq!(out[0], quantized(samples[target as usize]), "seek to {target}");
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn reports_missing_and_invalid_files() {
        assert!(matches!(Decoder::open(Path::new("/definitely/not/here.mp3")), Err(AudioError::Open { .. })));
        let path = temp_path("garbage");
        std::fs::write(&path, b"this is not audio at all, just some bytes").unwrap();
        assert!(Decoder::open(&path).is_err());
        std::fs::remove_file(path).unwrap();
    }
}
