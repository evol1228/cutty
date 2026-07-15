//! Pull-based decode of one file's audio track (symphonia).
//!
//! [`AudioSource`] is the seam between the mixer and the codec layer: the
//! mixer pulls interleaved f32 at the source's native rate/layout and does
//! its own resampling/mixdown. [`SymphoniaSource`] is the real
//! implementation; tests inject synthetic sources.
//!
//! Sample accuracy: seeks go through symphonia's `SeekMode::Accurate`
//! (keyframe seek + counted discard), and AAC encoder priming
//! (`track.delay`) is folded into positions, so presentation time 0 is the
//! first audible sample — same scheme the Phase 0 player validated.

use std::fs::File;
use std::path::Path;

use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, Track, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{Time, TimeBase, Timestamp};

use crate::error::AudioError;

/// A seekable stream of interleaved f32 samples at a fixed rate/layout.
pub trait AudioSource: Send {
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> usize;

    /// Position the stream so the next [`AudioSource::read`] returns
    /// samples starting at presentation time `secs`.
    fn seek(&mut self, secs: f64) -> Result<(), AudioError>;

    /// Fill `out` with interleaved samples; returns the number of samples
    /// (not frames) written. `0` means end of stream. Short reads are
    /// allowed and carry no meaning beyond "call again".
    fn read(&mut self, out: &mut [f32]) -> Result<usize, AudioError>;
}

/// Everything the decode loop needs to know about the selected track.
#[derive(Clone)]
struct TrackInfo {
    track_id: u32,
    time_base: TimeBase,
    sample_rate: u32,
    channels: usize,
    /// Encoder priming frames (AAC delay). Presentation 0 = track frame
    /// `delay`. symphonia 0.6's isomp4 reader does not apply edit lists,
    /// so we handle it ourselves.
    delay: u64,
}

impl TryFrom<&Track> for TrackInfo {
    type Error = AudioError;

    fn try_from(track: &Track) -> Result<Self, AudioError> {
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or(AudioError::MissingParams("audio codec parameters"))?;
        Ok(TrackInfo {
            track_id: track.id,
            time_base: track
                .time_base
                .ok_or(AudioError::MissingParams("time base"))?,
            sample_rate: params
                .sample_rate
                .ok_or(AudioError::MissingParams("sample rate"))?,
            channels: params
                .channels
                .as_ref()
                .map(|c| c.count())
                .ok_or(AudioError::MissingParams("channel count"))?,
            delay: u64::from(track.delay.unwrap_or(0)),
        })
    }
}

/// Convert a track timestamp to PCM frames. For MP4 audio the timebase is
/// normally 1/sample_rate (ticks == frames), but don't assume it.
fn ts_to_frames(ts: Timestamp, tb: TimeBase, rate: u32) -> u64 {
    let t = ts.get().max(0) as u128;
    (t * u128::from(tb.numer.get()) * u128::from(rate) / u128::from(tb.denom.get())) as u64
}

/// Real file-backed [`AudioSource`] built on symphonia.
pub struct SymphoniaSource {
    path: std::path::PathBuf,
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    info: TrackInfo,
    /// Decoded samples not yet handed out.
    pending: Vec<f32>,
    pending_off: usize,
    /// Frames still to drop before handing out samples (priming, or the
    /// keyframe→target remainder after a seek).
    discard_frames: u64,
    eof: bool,
    /// The demuxer errored (several readers end this way at EOF) — it may
    /// be unusable afterwards, so the next seek reopens the file.
    wedged: bool,
}

impl SymphoniaSource {
    /// Open `path` and select its default audio track, positioned at 0.
    pub fn open(path: &Path) -> Result<Self, AudioError> {
        let file = File::open(path)?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let format = symphonia::default::get_probe().probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )?;

        let track = format
            .default_track(TrackType::Audio)
            .ok_or(AudioError::NoAudioTrack)?;
        let info = TrackInfo::try_from(track)?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or(AudioError::MissingParams("audio codec parameters"))?
            .clone();
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(&params, &AudioDecoderOptions::default())?;

        Ok(Self {
            path: path.to_path_buf(),
            format,
            decoder,
            discard_frames: info.delay,
            info,
            pending: Vec::new(),
            pending_off: 0,
            eof: false,
            wedged: false,
        })
    }

    /// Decode packets until `pending` holds samples (or EOF).
    fn refill(&mut self) -> Result<(), AudioError> {
        self.pending.clear();
        self.pending_off = 0;
        while !self.eof {
            let packet = match self.format.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => {
                    self.eof = true;
                    return Ok(());
                }
                // Demux errors end the stream but don't kill playback:
                // several symphonia readers (isomp4 included) report the
                // natural end of some files as an error rather than
                // `Ok(None)`, and mid-file corruption degrading to
                // silence is the right preview behavior anyway.
                Err(e) => {
                    eprintln!("cutty-audio: demux ended with error: {e}");
                    self.eof = true;
                    self.wedged = true;
                    return Ok(());
                }
            };
            if packet.track_id != self.info.track_id {
                continue;
            }
            let decoded: GenericAudioBufferRef<'_> = match self.decoder.decode(&packet) {
                Ok(buf) => buf,
                // Recoverable: skip the packet (decoder clears its buffer).
                Err(SymError::DecodeError(e)) => {
                    eprintln!("cutty-audio: decode error (skipping packet): {e}");
                    continue;
                }
                Err(e) => {
                    self.eof = true;
                    return Err(e.into());
                }
            };
            if decoded.frames() == 0 {
                continue;
            }
            decoded.copy_to_vec_interleaved(&mut self.pending);

            // Trim priming / post-seek remainder.
            if self.discard_frames > 0 {
                let skip = (self.discard_frames as usize).min(decoded.frames());
                self.discard_frames -= skip as u64;
                self.pending_off = skip * self.info.channels;
                if self.pending_off >= self.pending.len() {
                    self.pending.clear();
                    self.pending_off = 0;
                    continue; // the whole packet was discard
                }
            }
            return Ok(());
        }
        Ok(())
    }
}

impl AudioSource for SymphoniaSource {
    fn sample_rate(&self) -> u32 {
        self.info.sample_rate
    }

    fn channels(&self) -> usize {
        self.info.channels
    }

    fn seek(&mut self, secs: f64) -> Result<(), AudioError> {
        // Reopen instead of seeking a spent reader: several symphonia
        // demuxers (isomp4 included) cannot rewind once their packet
        // iterator hit end-of-stream — the post-seek reads just error.
        if self.wedged || self.eof {
            *self = Self::open(&self.path)?;
        }
        // Presentation secs → track time (fold the priming in).
        let track_secs = secs.max(0.0) + self.info.delay as f64 / f64::from(self.info.sample_rate);
        let time =
            Time::try_from_secs_f64(track_secs).ok_or(AudioError::MissingParams("seek time"))?;
        let seeked = self.format.seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time,
                track_id: Some(self.info.track_id),
            },
        )?;
        self.decoder.reset();
        let req = ts_to_frames(seeked.required_ts, self.info.time_base, self.info.sample_rate);
        let act = ts_to_frames(seeked.actual_ts, self.info.time_base, self.info.sample_rate);
        self.discard_frames = req.saturating_sub(act);
        self.pending.clear();
        self.pending_off = 0;
        self.eof = false;
        Ok(())
    }

    fn read(&mut self, out: &mut [f32]) -> Result<usize, AudioError> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.pending_off >= self.pending.len() {
            self.refill()?;
        }
        let available = &self.pending[self.pending_off.min(self.pending.len())..];
        let n = available.len().min(out.len());
        out[..n].copy_from_slice(&available[..n]);
        self.pending_off += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    /// Generate (once) a mono 48k WAV with sample s = (s % 1000)/1000 ramps
    /// so absolute positions are recoverable from sample values.
    fn ramp_wav() -> PathBuf {
        let dir = std::env::temp_dir().join("cutty-audio-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("ramp-48k-2s.wav");
        if file.is_file() {
            return file;
        }
        // aevalsrc ramp: value cycles 0→1 every 1000 samples.
        let status = Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
            .arg("aevalsrc=mod(n\\,1000)/1000:s=48000:d=2")
            .args(["-c:a", "pcm_f32le"])
            .arg(&file)
            .status()
            .expect("system ffmpeg must be installed for tests");
        assert!(status.success());
        file
    }

    fn sine_mp4() -> PathBuf {
        let dir = std::env::temp_dir().join("cutty-audio-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("sine-44k-2s.m4a");
        if file.is_file() {
            return file;
        }
        let status = Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
            .arg("sine=frequency=440:sample_rate=44100:duration=2")
            .args(["-c:a", "aac", "-b:a", "128k"])
            .arg(&file)
            .status()
            .expect("system ffmpeg must be installed for tests");
        assert!(status.success());
        file
    }

    #[test]
    fn wav_reads_from_zero_and_seeks_sample_accurately() {
        let mut src = SymphoniaSource::open(&ramp_wav()).unwrap();
        assert_eq!(src.sample_rate(), 48_000);
        assert_eq!(src.channels(), 1);

        let mut buf = vec![0f32; 8];
        let n = src.read(&mut buf).unwrap();
        assert!(n > 0);
        assert!(buf[0].abs() < 1e-4, "first sample must be ramp 0, got {}", buf[0]);

        // Seek to exactly 1.0s = sample 48000; 48000 % 1000 = 0 → ramp 0,
        // next sample 1/1000.
        src.seek(1.0).unwrap();
        let n = src.read(&mut buf).unwrap();
        assert!(n >= 2);
        assert!(
            buf[0].abs() < 2e-3,
            "sample at 1.0s must be ramp 0, got {}",
            buf[0]
        );
        assert!(
            (buf[1] - 0.001).abs() < 2e-3,
            "next sample must be ramp 1/1000, got {}",
            buf[1]
        );

        // Mid-ramp seek: sample 48_500 → ramp 500/1000 = 0.5.
        src.seek(48_500.0 / 48_000.0).unwrap();
        let n = src.read(&mut buf).unwrap();
        assert!(n > 0);
        assert!(
            (buf[0] - 0.5).abs() < 2e-3,
            "sample at 48500 must be 0.5, got {}",
            buf[0]
        );
    }

    #[test]
    fn aac_decodes_and_reports_eof() {
        let mut src = SymphoniaSource::open(&sine_mp4()).unwrap();
        assert_eq!(src.sample_rate(), 44_100);

        let mut total = 0usize;
        let mut buf = vec![0f32; 4096];
        loop {
            let n = src.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            total += n;
        }
        let frames = total / src.channels();
        // ~2s of audio, allow AAC padding slack.
        assert!(
            (80_000..100_000).contains(&frames),
            "expected ≈88200 frames, got {frames}"
        );

        // Reads after EOF keep returning 0 without error.
        assert_eq!(src.read(&mut buf).unwrap(), 0);

        // And a seek revives the stream.
        src.seek(0.5).unwrap();
        assert!(src.read(&mut buf).unwrap() > 0);
    }

    #[test]
    fn missing_file_is_an_error() {
        assert!(SymphoniaSource::open(Path::new("/nonexistent.wav")).is_err());
    }
}
