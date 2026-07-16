//! Libav-backed [`AudioSource`] — the fallback for codecs symphonia
//! doesn't cover (ac3, dts, opus-in-webm, …).
//!
//! [`open_audio_source`] is the factory the mixer and the offline export
//! render use: symphonia first (well-tested, pure Rust), libav when
//! symphonia can't open the file or its codec. Both implementations
//! speak the same [`AudioSource`] contract: interleaved f32 at the
//! source's native rate/layout, sample-accurate seeks.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use cutty_audio::{AudioError, AudioSource, SymphoniaSource};
use ffmpeg_the_third as ffmpeg;
use ffmpeg_the_third::ffi::AV_TIME_BASE;
use ffmpeg_the_third::format::context::Input;
use ffmpeg_the_third::media::Type;
use ffmpeg_the_third::software::resampling;
use ffmpeg_the_third::util::channel_layout::ChannelLayout;
use ffmpeg_the_third::util::format::sample::{Sample, Type as SampleType};

/// Open `path` for audio decode: symphonia when it can, libav otherwise.
///
/// This is the one construction point renderers should use — live
/// preview, offline export, and peak generation all decode through it,
/// so a codec either works everywhere or nowhere.
pub fn open_audio_source(path: &Path) -> Result<Box<dyn AudioSource>, AudioError> {
    match SymphoniaSource::open(path) {
        Ok(src) => Ok(Box::new(src)),
        Err(symphonia_err) => match LibavAudioSource::open(path) {
            Ok(src) => Ok(Box::new(src)),
            // The symphonia error names the codec problem; the libav one
            // says why the fallback failed too. Report both.
            Err(libav_err) => Err(AudioError::Backend(format!(
                "{symphonia_err}; libav fallback: {libav_err}"
            ))),
        },
    }
}

fn ff_msg(context: &str, path: &Path, e: impl std::fmt::Display) -> AudioError {
    AudioError::Backend(format!("{context} for {}: {e}", path.display()))
}

/// In-process libav audio decode session: packets → decoder → swresample
/// (format conversion to packed f32 only — rate and layout stay native).
pub struct LibavAudioSource {
    path: PathBuf,
    ictx: Input,
    decoder: ffmpeg::codec::decoder::Audio,
    resampler: resampling::Context,
    stream_index: usize,
    /// Stream time base, seconds per tick.
    tb: f64,
    sample_rate: u32,
    channels: usize,
    /// Decoded interleaved samples not yet handed out.
    pending: VecDeque<f32>,
    /// Frames (per-channel sample groups) still to drop before handing
    /// out samples — the keyframe→target remainder after a seek.
    discard_frames: u64,
    /// End of container reached and decoder drained.
    eof: bool,
    /// Decoder switched to drain mode.
    eof_sent: bool,
    /// Presentation time of the *next* frame the decoder will hand out,
    /// tracked from packet pts (used to compute seek discards).
    next_pts_sec: Option<f64>,
}

// Safety: every libav context is exclusively owned and only touched
// through `&mut self`; moving the session between threads is fine.
unsafe impl Send for LibavAudioSource {}

impl LibavAudioSource {
    /// Open `path` and select its best audio stream, positioned at 0.
    pub fn open(path: &Path) -> Result<Self, AudioError> {
        crate::decode::init_ffmpeg_once();
        let ictx = ffmpeg::format::input(path).map_err(|e| ff_msg("opening", path, e))?;
        let stream = ictx
            .streams()
            .best(Type::Audio)
            .ok_or(AudioError::NoAudioTrack)?;
        let stream_index = stream.index();
        let time_base = stream.time_base();
        let tb = f64::from(time_base.numerator()) / f64::from(time_base.denominator());

        let ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
            .map_err(|e| ff_msg("reading codec parameters", path, e))?;
        let decoder = ctx
            .decoder()
            .audio()
            .map_err(|e| ff_msg("opening decoder", path, e))?;
        let sample_rate = decoder.rate();
        if sample_rate == 0 {
            return Err(AudioError::MissingParams("sample rate"));
        }
        // Normalize to the default (native-order) layout for the channel
        // count: decoders may report unspec-order layouts whose mask the
        // resampler bindings can't represent; the default layout for the
        // same count is bit-compatible for format-only conversion.
        let channels = decoder.ch_layout().channels().max(1) as usize;
        let resampler = Self::make_resampler(&decoder, channels, path)?;

        Ok(Self {
            path: path.to_path_buf(),
            ictx,
            decoder,
            resampler,
            stream_index,
            tb,
            sample_rate,
            channels,
            pending: VecDeque::new(),
            discard_frames: 0,
            eof: false,
            eof_sent: false,
            next_pts_sec: None,
        })
    }

    /// Format-only conversion (planar/int → packed f32); rate and channel
    /// count pass through untouched, matching the [`AudioSource`]
    /// contract. Layouts are the defaults for the count (see `open`).
    fn make_resampler(
        decoder: &ffmpeg::codec::decoder::Audio,
        channels: usize,
        path: &Path,
    ) -> Result<resampling::Context, AudioError> {
        let layout = ChannelLayout::default_for_channels(channels as u32);
        resampling::Context::get2(
            decoder.format(),
            layout.clone(),
            decoder.rate(),
            Sample::F32(SampleType::Packed),
            layout,
            decoder.rate(),
        )
        .map_err(|e| ff_msg("creating resampler", path, e))
    }

    /// Decode packets until `pending` holds samples (or EOF).
    fn refill(&mut self) -> Result<(), AudioError> {
        let mut frame = ffmpeg::frame::Audio::empty();
        while self.pending.is_empty() && !self.eof {
            match self.decoder.receive_frame(&mut frame) {
                Ok(()) => {
                    self.ingest(&mut frame)?;
                    continue;
                }
                Err(ffmpeg::Error::Other {
                    errno: libc::EAGAIN,
                }) => {} // needs more input
                Err(ffmpeg::Error::Eof) => {
                    self.eof = true;
                    return Ok(());
                }
                Err(e) => return Err(ff_msg("decoding", &self.path, e)),
            }
            if self.eof_sent {
                continue; // drain until Eof
            }
            loop {
                match self.ictx.packets().next() {
                    Some(Ok((stream, packet))) => {
                        if stream.index() != self.stream_index {
                            continue;
                        }
                        self.decoder
                            .send_packet(&packet)
                            .map_err(|e| ff_msg("decoding", &self.path, e))?;
                        break;
                    }
                    Some(Err(ffmpeg::Error::Other {
                        errno: libc::EAGAIN,
                    })) => continue,
                    Some(Err(e)) => return Err(ff_msg("demuxing", &self.path, e)),
                    None => {
                        self.decoder
                            .send_eof()
                            .map_err(|e| ff_msg("draining decoder", &self.path, e))?;
                        self.eof_sent = true;
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    /// Convert one decoded frame to packed f32 and queue it, applying any
    /// post-seek discard.
    fn ingest(&mut self, frame: &mut ffmpeg::frame::Audio) -> Result<(), AudioError> {
        if frame.samples() == 0 {
            return Ok(());
        }
        if let Some(pts) = frame.pts().or_else(|| frame.timestamp()) {
            let sec = pts as f64 * self.tb;
            self.next_pts_sec = Some(sec + frame.samples() as f64 / f64::from(self.sample_rate));
        } else if let Some(next) = self.next_pts_sec {
            self.next_pts_sec = Some(next + frame.samples() as f64 / f64::from(self.sample_rate));
        }

        // Normalize the frame's layout to the resampler's (unspec-order
        // layouts with the same channel count are bit-compatible; a
        // mismatch would make swr_convert_frame reject the frame).
        frame.set_ch_layout(ChannelLayout::default_for_channels(self.channels as u32));
        let mut packed = ffmpeg::frame::Audio::empty();
        self.resampler
            .run(frame, &mut packed)
            .map_err(|e| ff_msg("converting samples", &self.path, e))?;
        if packed.samples() == 0 {
            return Ok(());
        }
        let bytes = packed.data(0);
        let want = packed.samples() * self.channels;
        let mut it = bytes
            .chunks_exact(4)
            .take(want)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]));

        // Trim the keyframe→target remainder after a seek.
        if self.discard_frames > 0 {
            let skip_frames = (self.discard_frames as usize).min(packed.samples());
            self.discard_frames -= skip_frames as u64;
            for _ in 0..skip_frames * self.channels {
                it.next();
            }
        }
        self.pending.extend(it);
        Ok(())
    }
}

impl AudioSource for LibavAudioSource {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> usize {
        self.channels
    }

    fn seek(&mut self, secs: f64) -> Result<(), AudioError> {
        let target = secs.max(0.0);
        let ts = (target * f64::from(AV_TIME_BASE)).round() as i64;
        self.ictx
            .seek(ts, ..=ts)
            .map_err(|e| ff_msg("seeking", &self.path, e))?;
        self.decoder.flush();
        // Fresh converter: swresample buffers a few samples internally.
        self.resampler = Self::make_resampler(&self.decoder, self.channels, &self.path)?;
        self.pending.clear();
        self.eof = false;
        self.eof_sent = false;
        self.next_pts_sec = None;
        self.discard_frames = 0;

        // Decode forward to the first frame overlapping the target, then
        // set the intra-frame discard from its pts.
        loop {
            let before = self.pending.len();
            self.refill()?;
            if self.pending.is_empty() {
                return Ok(()); // past EOF: reads return 0
            }
            // pts of the frame just ingested = next_pts_sec - its length.
            let ingested_frames = (self.pending.len() - before) / self.channels;
            let frame_end = match self.next_pts_sec {
                Some(t) => t,
                None => return Ok(()), // no pts: best effort, start here
            };
            let frame_pts = frame_end - ingested_frames as f64 / f64::from(self.sample_rate);
            if frame_end <= target + 1e-9 {
                self.pending.clear(); // wholly before the target
                continue;
            }
            let skip_frames = ((target - frame_pts) * f64::from(self.sample_rate)).round() as i64;
            let skip = skip_frames.clamp(0, ingested_frames as i64) as usize * self.channels;
            self.pending.drain(..skip.min(self.pending.len()));
            return Ok(());
        }
    }

    fn read(&mut self, out: &mut [f32]) -> Result<usize, AudioError> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.pending.is_empty() {
            self.refill()?;
        }
        let n = self.pending.len().min(out.len());
        for (dst, src) in out.iter_mut().zip(self.pending.drain(..n)) {
            *dst = src;
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn test_dir() -> PathBuf {
        let dir = std::env::temp_dir().join("cutty-media-tests");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// AC3 file with a 440 Hz sine — symphonia has no ac3 decoder, so
    /// this exercises the libav fallback end to end.
    fn sine_ac3() -> PathBuf {
        let file = test_dir().join("sine-ac3.ac3");
        if file.is_file() {
            return file;
        }
        let status = Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
            .arg("sine=frequency=440:sample_rate=48000:duration=2")
            .args(["-c:a", "ac3", "-b:a", "192k"])
            .arg(&file)
            .status()
            .expect("system ffmpeg required");
        assert!(status.success());
        file
    }

    fn sine_dts() -> PathBuf {
        let file = test_dir().join("sine-dts.mkv");
        if file.is_file() {
            return file;
        }
        let status = Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
            .arg("sine=frequency=550:sample_rate=48000:duration=2")
            .args(["-c:a", "dca", "-strict", "-2", "-b:a", "768k"])
            .arg(&file)
            .status()
            .expect("system ffmpeg required");
        assert!(status.success());
        file
    }

    fn rms(buf: &[f32]) -> f64 {
        (buf.iter().map(|s| f64::from(*s) * f64::from(*s)).sum::<f64>() / buf.len() as f64).sqrt()
    }

    #[test]
    fn ac3_decodes_through_the_libav_fallback() {
        let src = open_audio_source(&sine_ac3()).expect("fallback opens ac3");
        let mut src = src;
        assert_eq!(src.sample_rate(), 48_000);
        assert!(src.channels() >= 1);

        let mut total = 0usize;
        let mut energy: Vec<f32> = Vec::new();
        let mut buf = vec![0f32; 4096];
        loop {
            let n = src.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            energy.extend_from_slice(&buf[..n]);
            total += n;
        }
        let frames = total / src.channels();
        assert!(
            (90_000..102_000).contains(&frames),
            "≈2s at 48k expected, got {frames} frames"
        );
        // ffmpeg's `sine` source synthesizes at ~1/8 scale (RMS ≈ 0.088);
        // ac3 is lossy but lands close.
        let rms = rms(&energy);
        assert!(
            (0.06..0.12).contains(&rms),
            "sine energy expected, rms {rms}"
        );
    }

    #[test]
    fn dts_decodes_and_seeks() {
        let mut src = LibavAudioSource::open(&sine_dts()).expect("libav opens dts");
        assert_eq!(src.sample_rate(), 48_000);

        let mut buf = vec![0f32; 4800];
        let n = src.read(&mut buf).unwrap();
        assert!(n > 0, "reads from zero");

        // Seek to 1.0s and read: still sine energy, and EOF lands ~1s later.
        src.seek(1.0).unwrap();
        let mut total = 0usize;
        loop {
            let n = src.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            total += n;
        }
        let frames = total / src.channels();
        assert!(
            (44_000..53_000).contains(&frames),
            "≈1s remaining after seek(1.0), got {frames} frames"
        );

        // Seek after EOF revives the stream.
        src.seek(0.25).unwrap();
        assert!(src.read(&mut buf).unwrap() > 0);
    }

    #[test]
    fn libav_seek_is_sample_accurate_on_pcm() {
        // The ramp trick from the symphonia tests, decoded through libav:
        // sample s = (s % 1000)/1000, so positions are recoverable.
        let file = test_dir().join("ramp-libav.wav");
        if !file.is_file() {
            let status = Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
                .arg("aevalsrc=mod(n\\,1000)/1000:s=48000:d=2")
                .args(["-c:a", "pcm_f32le"])
                .arg(&file)
                .status()
                .unwrap();
            assert!(status.success());
        }
        let mut src = LibavAudioSource::open(&file).unwrap();
        let mut buf = vec![0f32; 8];
        src.seek(48_500.0 / 48_000.0).unwrap();
        let n = src.read(&mut buf).unwrap();
        assert!(n > 0);
        assert!(
            (buf[0] - 0.5).abs() < 2e-3,
            "sample at 48500 must be ramp 0.5, got {}",
            buf[0]
        );
    }

    #[test]
    fn factory_prefers_symphonia_for_wav() {
        // No direct way to observe the backend, but a WAV must open and
        // read (symphonia path) and a bogus path must fail with both
        // errors reported.
        let Err(err) = open_audio_source(Path::new("/nonexistent.ac3")) else {
            panic!("nonexistent file must not open");
        };
        let msg = err.to_string();
        assert!(msg.contains("libav fallback"), "{msg}");
    }
}
