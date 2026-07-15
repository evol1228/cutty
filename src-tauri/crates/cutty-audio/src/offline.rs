//! Offline (faster-than-realtime) rendering of the mixed timeline to a
//! WAV file — the audio side of export.
//!
//! This drives the *exact same* placement and block-mixing code as live
//! playback ([`crate::mixer`]), so the exported mix equals the preview mix
//! by construction: same segment rounding, same resampling, same volume
//! math, same headroom clamp. The only differences are the destination (a
//! WAV file instead of the device ring) and the failure policy — live
//! playback degrades a broken clip to silence, an offline render fails
//! loudly, because silently wrong exported audio is unacceptable.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::AudioError;
use crate::mixer::{place, render_block, ClipReader, MixerTimeline, BLOCK_FRAMES};
use crate::source::{AudioSource, SymphoniaSource};

/// Fixed output rate for exported audio. 48 kHz is the video-production
/// standard; the mixer resamples every source to it exactly as it does
/// for the playback device.
pub const EXPORT_SAMPLE_RATE: u32 = 48_000;

/// Render `timeline` to a stereo 32-bit-float WAV at `dst`.
///
/// Renders exactly `total_frames` stereo frames starting at timeline 0 —
/// the caller derives that count from the video frame grid so both tracks
/// of the muxed file end at the same instant. Blocking; call from a
/// worker thread. `cancel` is polled between blocks: when it returns
/// true, the partial file is removed and [`AudioError::Cancelled`] comes
/// back. `on_progress` receives `(frames_done, total_frames)` a few times
/// per rendered second.
pub fn render_timeline_to_wav(
    timeline: MixerTimeline,
    out_rate: u32,
    total_frames: u64,
    dst: &Path,
    cancel: &dyn Fn() -> bool,
    on_progress: &mut dyn FnMut(u64, u64),
) -> Result<(), AudioError> {
    let mut open = |seg: &crate::mixer::AudioSegment| -> Result<Box<dyn AudioSource>, AudioError> {
        Ok(Box::new(SymphoniaSource::open(&seg.path)?))
    };
    render_timeline_to_wav_with(
        timeline,
        out_rate,
        total_frames,
        dst,
        cancel,
        on_progress,
        &mut open,
    )
}

/// [`render_timeline_to_wav`] with an injectable source factory (tests
/// use synthetic sources).
pub(crate) fn render_timeline_to_wav_with(
    timeline: MixerTimeline,
    out_rate: u32,
    total_frames: u64,
    dst: &Path,
    cancel: &dyn Fn() -> bool,
    on_progress: &mut dyn FnMut(u64, u64),
    open: crate::mixer::OpenSource<'_>,
) -> Result<(), AudioError> {
    let result = write_wav(
        timeline,
        out_rate,
        total_frames,
        dst,
        cancel,
        on_progress,
        open,
    );
    if result.is_err() {
        let _ = std::fs::remove_file(dst);
    }
    result
}

fn write_wav(
    timeline: MixerTimeline,
    out_rate: u32,
    total_frames: u64,
    dst: &Path,
    cancel: &dyn Fn() -> bool,
    on_progress: &mut dyn FnMut(u64, u64),
    open: crate::mixer::OpenSource<'_>,
) -> Result<(), AudioError> {
    let segments = place(timeline, out_rate);
    let mut readers: HashMap<usize, Option<ClipReader>> = HashMap::new();

    // Fail-loud error policy: render_block reports per-segment problems
    // through a callback; the first one aborts the render. RefCell because
    // the callback is rebuilt per block while this loop also reads it.
    let first_error: std::cell::RefCell<Option<AudioError>> = std::cell::RefCell::new(None);

    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = BufWriter::new(File::create(dst)?);
    write_wav_header(&mut out, out_rate, total_frames)?;

    let mut block = vec![0f32; BLOCK_FRAMES * 2];
    let mut head: i64 = 0;
    let mut done: u64 = 0;
    // Progress cadence: every ~0.25 s of rendered audio.
    let report_every = u64::from(out_rate) / 4;
    let mut next_report = 0u64;

    while done < total_frames {
        if cancel() {
            return Err(AudioError::Cancelled);
        }
        let frames = BLOCK_FRAMES.min((total_frames - done) as usize);
        let buf = &mut block[..frames * 2];
        let mut on_error = |seg: &crate::mixer::AudioSegment, message: String| {
            let mut slot = first_error.borrow_mut();
            if slot.is_none() {
                *slot = Some(AudioError::OfflineRender {
                    path: seg.path.display().to_string(),
                    message,
                });
            }
        };
        render_block(
            &segments,
            &mut readers,
            open,
            &mut on_error,
            head,
            out_rate,
            buf,
        );
        if let Some(err) = first_error.borrow_mut().take() {
            return Err(err);
        }
        for sample in buf.iter() {
            out.write_all(&sample.to_le_bytes())?;
        }
        head += frames as i64;
        done += frames as u64;
        if done >= next_report {
            on_progress(done, total_frames);
            next_report = done + report_every;
        }
    }

    out.flush()?;
    // Sizes were written up front from `total_frames`; verify we honored
    // them (the loop above renders exactly that many frames).
    debug_assert_eq!(
        out.get_ref().metadata()?.len(),
        wav_total_size(total_frames)
    );
    on_progress(total_frames, total_frames);
    Ok(())
}

/// Bytes per stereo f32 frame.
const FRAME_BYTES: u64 = 8;
/// Header bytes before the data payload: RIFF(12) + fmt(8+18) + fact(8+4)
/// + data header(8).
const HEADER_BYTES: u64 = 12 + 26 + 12 + 8;

fn wav_total_size(total_frames: u64) -> u64 {
    HEADER_BYTES + total_frames * FRAME_BYTES
}

/// Write a WAVE_FORMAT_IEEE_FLOAT header for stereo f32 with exact sizes
/// (the frame count is known up front, so no post-hoc patching).
fn write_wav_header<W: Write + Seek>(
    out: &mut W,
    rate: u32,
    total_frames: u64,
) -> Result<(), AudioError> {
    let data_bytes = total_frames * FRAME_BYTES;
    let riff_size = wav_total_size(total_frames) - 8;
    if u32::try_from(data_bytes).is_err() {
        // > 4 GiB of f32 PCM ≈ 37 h of timeline. WAV can't hold it.
        return Err(AudioError::OfflineRender {
            path: "<wav>".into(),
            message: format!("timeline too long for WAV ({total_frames} frames)"),
        });
    }

    out.seek(SeekFrom::Start(0))?;
    out.write_all(b"RIFF")?;
    out.write_all(&(riff_size as u32).to_le_bytes())?;
    out.write_all(b"WAVE")?;

    // fmt chunk: 18 bytes (WAVE_FORMAT_IEEE_FLOAT wants cbSize present).
    out.write_all(b"fmt ")?;
    out.write_all(&18u32.to_le_bytes())?;
    out.write_all(&3u16.to_le_bytes())?; // IEEE float
    out.write_all(&2u16.to_le_bytes())?; // stereo
    out.write_all(&rate.to_le_bytes())?;
    out.write_all(&(rate * FRAME_BYTES as u32).to_le_bytes())?; // byte rate
    out.write_all(&(FRAME_BYTES as u16).to_le_bytes())?; // block align
    out.write_all(&32u16.to_le_bytes())?; // bits per sample
    out.write_all(&0u16.to_le_bytes())?; // cbSize

    // fact chunk (required for non-PCM formats): sample frames per channel.
    out.write_all(b"fact")?;
    out.write_all(&4u32.to_le_bytes())?;
    out.write_all(&(total_frames as u32).to_le_bytes())?;

    out.write_all(b"data")?;
    out.write_all(&(data_bytes as u32).to_le_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mixer::AudioSegment;
    use std::path::PathBuf;

    /// Constant-value source at 48 kHz mono, long enough for any test.
    struct ConstSource(f32);

    impl AudioSource for ConstSource {
        fn sample_rate(&self) -> u32 {
            EXPORT_SAMPLE_RATE
        }

        fn channels(&self) -> usize {
            1
        }

        fn seek(&mut self, _secs: f64) -> Result<(), AudioError> {
            Ok(())
        }

        fn read(&mut self, out: &mut [f32]) -> Result<usize, AudioError> {
            out.fill(self.0);
            Ok(out.len())
        }
    }

    fn seg(t_in: f64, t_out: f64, volume: f64) -> AudioSegment {
        AudioSegment {
            path: PathBuf::from("/fake"),
            timeline_in: t_in,
            timeline_out: t_out,
            source_in: 0.0,
            speed: 1.0,
            volume,
            fade_in: None,
            fade_out: None,
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("cutty-audio-tests");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    /// Parse the test WAVs we write: header sanity + all samples.
    fn read_wav_f32(path: &Path) -> (u32, Vec<f32>) {
        let bytes = std::fs::read(path).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[12..16], b"fmt ");
        let format = u16::from_le_bytes([bytes[20], bytes[21]]);
        assert_eq!(format, 3, "IEEE float");
        let channels = u16::from_le_bytes([bytes[22], bytes[23]]);
        assert_eq!(channels, 2);
        let rate = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]);
        let data_off = HEADER_BYTES as usize;
        assert_eq!(&bytes[data_off - 8..data_off - 4], b"data");
        let samples = bytes[data_off..]
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        (rate, samples)
    }

    #[test]
    fn renders_placement_volume_and_exact_length() {
        // 0.5 s segment at volume 0.5 inside a 1 s render: silence,
        // then 0.25 (const 0.5 × vol 0.5), then silence.
        let timeline = MixerTimeline {
            segments: vec![seg(0.25, 0.75, 0.5)],
        };
        let dst = tmp("offline-basic.wav");
        let total = u64::from(EXPORT_SAMPLE_RATE); // 1 s
        let mut progress = Vec::new();
        render_timeline_to_wav_with(
            timeline,
            EXPORT_SAMPLE_RATE,
            total,
            &dst,
            &|| false,
            &mut |d, t| progress.push((d, t)),
            &mut |_| Ok(Box::new(ConstSource(0.5))),
        )
        .unwrap();

        let (rate, samples) = read_wav_f32(&dst);
        assert_eq!(rate, EXPORT_SAMPLE_RATE);
        assert_eq!(samples.len() as u64, total * 2, "exact frame count");

        let frame = |t: f64| ((t * f64::from(EXPORT_SAMPLE_RATE)) as usize) * 2;
        assert_eq!(samples[frame(0.1)], 0.0, "silence before");
        assert!((samples[frame(0.5)] - 0.25).abs() < 1e-6, "const × volume");
        assert_eq!(samples[frame(0.9)], 0.0, "silence after");
        // The cut lands on the exact sample.
        let cut = (0.75 * f64::from(EXPORT_SAMPLE_RATE)).round() as usize;
        assert!(samples[(cut - 1) * 2] != 0.0);
        assert_eq!(samples[cut * 2], 0.0);

        assert_eq!(progress.last(), Some(&(total, total)));
    }

    #[test]
    fn overlapping_segments_sum_like_the_live_mixer() {
        let timeline = MixerTimeline {
            segments: vec![seg(0.0, 1.0, 0.5), seg(0.0, 1.0, 0.25)],
        };
        let dst = tmp("offline-sum.wav");
        render_timeline_to_wav_with(
            timeline,
            EXPORT_SAMPLE_RATE,
            1000,
            &dst,
            &|| false,
            &mut |_, _| {},
            &mut |_| Ok(Box::new(ConstSource(0.4))),
        )
        .unwrap();
        let (_, samples) = read_wav_f32(&dst);
        // 0.4×0.5 + 0.4×0.25 = 0.3 on both channels.
        assert!((samples[100] - 0.3).abs() < 1e-6);
        assert!((samples[101] - 0.3).abs() < 1e-6);
    }

    #[test]
    fn failing_source_aborts_and_removes_the_file() {
        let timeline = MixerTimeline {
            segments: vec![seg(0.0, 1.0, 1.0)],
        };
        let dst = tmp("offline-fail.wav");
        let err = render_timeline_to_wav_with(
            timeline,
            EXPORT_SAMPLE_RATE,
            48_000,
            &dst,
            &|| false,
            &mut |_, _| {},
            &mut |_| Err(AudioError::NoAudioTrack),
        )
        .unwrap_err();
        assert!(matches!(err, AudioError::OfflineRender { .. }), "{err}");
        assert!(!dst.exists(), "partial file must be cleaned up");
    }

    #[test]
    fn cancel_aborts_and_removes_the_file() {
        let timeline = MixerTimeline {
            segments: vec![seg(0.0, 10.0, 1.0)],
        };
        let dst = tmp("offline-cancel.wav");
        let err = render_timeline_to_wav_with(
            timeline,
            EXPORT_SAMPLE_RATE,
            48_000 * 10,
            &dst,
            &|| true,
            &mut |_, _| {},
            &mut |_| Ok(Box::new(ConstSource(0.1))),
        )
        .unwrap_err();
        assert!(matches!(err, AudioError::Cancelled), "{err}");
        assert!(!dst.exists());
    }

    /// Round-trip through symphonia: the WAVs we write must be readable
    /// by the same decoder stack that plays audio-only media.
    #[test]
    fn written_wav_decodes_with_symphonia() {
        let timeline = MixerTimeline {
            segments: vec![seg(0.0, 0.5, 1.0)],
        };
        let dst = tmp("offline-roundtrip.wav");
        let total = u64::from(EXPORT_SAMPLE_RATE) / 2;
        render_timeline_to_wav_with(
            timeline,
            EXPORT_SAMPLE_RATE,
            total,
            &dst,
            &|| false,
            &mut |_, _| {},
            &mut |_| Ok(Box::new(ConstSource(0.25))),
        )
        .unwrap();

        let mut src = SymphoniaSource::open(&dst).unwrap();
        assert_eq!(src.sample_rate(), EXPORT_SAMPLE_RATE);
        assert_eq!(src.channels(), 2);
        let mut buf = vec![0f32; 4096];
        let mut frames = 0usize;
        let mut sum = 0f64;
        loop {
            let n = src.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            frames += n / 2;
            sum += buf[..n].iter().map(|&s| f64::from(s)).sum::<f64>();
        }
        assert_eq!(frames as u64, total);
        let mean = sum / (frames as f64 * 2.0);
        assert!((mean - 0.25).abs() < 1e-6, "mean {mean}");
    }
}
