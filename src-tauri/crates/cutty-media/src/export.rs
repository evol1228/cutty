//! Lossless trim export via ffmpeg stream copy.
//!
//! Stream copy cannot cut mid-GOP, so the in point snaps to the keyframe
//! at or before `in_sec` — we resolve that keyframe ourselves (ffprobe)
//! and cut exactly there, which makes the output duration deterministic:
//! `out_sec - actual_start`. (Naively passing `-ss in_sec` to ffmpeg would
//! instead *include* the keyframe pre-roll on top of `-t`, silently
//! inflating the duration.) Frame-accurate cutting requires the smart-cut
//! re-encode path, which lands with the full export pipeline in Phase 1.

use std::path::Path;
use std::process::Command;

use ffmpeg_sidecar::command::FfmpegCommand;
use ffmpeg_sidecar::event::{FfmpegEvent, LogLevel};
use ffmpeg_sidecar::ffprobe::ffprobe_path;

use crate::error::MediaError;
use crate::tools::ensure_tools;

/// What a lossless trim actually produced.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrimResult {
    /// The keyframe the cut actually starts on (≤ requested `in_sec`).
    pub actual_start_sec: f64,
    /// Expected output duration: `out_sec - actual_start_sec`.
    pub duration_sec: f64,
}

/// How far back we scan for a keyframe before falling back to 0.
const KEYFRAME_SCAN_WINDOW_SEC: f64 = 30.0;

/// Find the video keyframe at or before `t` (seconds).
///
/// Scans packet flags in a window before `t`; falls back to 0.0 (start of
/// file — always a valid cut point) when none is found, e.g. for very
/// sparse keyframes or audio-only files.
fn keyframe_at_or_before(src: &Path, t: f64) -> Result<f64, MediaError> {
    let start = (t - KEYFRAME_SCAN_WINDOW_SEC).max(0.0);
    let output = Command::new(ffprobe_path())
        .args(["-v", "error", "-select_streams", "v:0"])
        .args(["-show_entries", "packet=pts_time,flags", "-of", "csv=p=0"])
        .arg("-read_intervals")
        .arg(format!("{start:.6}%{:.6}", t + 0.5))
        .arg(src)
        .output()
        .map_err(|source| MediaError::Spawn {
            tool: "ffprobe",
            source,
        })?;
    if !output.status.success() {
        return Err(MediaError::ProbeFailed {
            path: src.display().to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }

    // Lines look like "6.950000,K__". Take the latest keyframe ≤ t
    // (with a half-frame grace so t exactly on a keyframe matches it).
    let best = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let (pts, flags) = line.split_once(',')?;
            if !flags.contains('K') {
                return None;
            }
            pts.trim().parse::<f64>().ok()
        })
        .filter(|&pts| pts <= t + 0.001)
        .fold(None::<f64>, |acc, pts| Some(acc.map_or(pts, |a| a.max(pts))));

    Ok(best.unwrap_or(0.0))
}

/// The ffmpeg argument list for a stream-copy trim. Split out for tests.
fn trim_args(src: &Path, dst: &Path, start_sec: f64, duration_sec: f64) -> Vec<String> {
    [
        // start_sec is a resolved keyframe, so this lands exactly on it.
        "-ss",
        &format!("{start_sec:.6}"),
        "-i",
        &src.display().to_string(),
        "-t",
        &format!("{duration_sec:.6}"),
        "-c",
        "copy",
        // Stream-copied packets keep their source timestamps; rebase to 0
        // so strict players don't see negative timestamps.
        "-avoid_negative_ts",
        "make_zero",
        "-y",
        &dst.display().to_string(),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Losslessly trim `src` to `[in_sec, out_sec]`, writing `dst`.
///
/// The in point snaps to the nearest keyframe at or before `in_sec`; the
/// returned [`TrimResult`] reports the actual bounds. Blocking — run on a
/// background thread (it is fast: no re-encode).
pub fn export_trim(
    src: &Path,
    dst: &Path,
    in_sec: f64,
    out_sec: f64,
) -> Result<TrimResult, MediaError> {
    ensure_tools()?;
    if !(in_sec >= 0.0 && out_sec > in_sec) {
        return Err(MediaError::FfmpegFailed {
            context: Some("trim export".into()),
            message: format!("invalid trim range {in_sec}..{out_sec}"),
        });
    }

    let actual_start_sec = keyframe_at_or_before(src, in_sec)?;
    let duration_sec = out_sec - actual_start_sec;

    let mut child = FfmpegCommand::new()
        .args(
            trim_args(src, dst, actual_start_sec, duration_sec)
                .iter()
                .map(String::as_str),
        )
        .spawn()
        .map_err(|source| MediaError::Spawn {
            tool: "ffmpeg",
            source,
        })?;

    let mut errors: Vec<String> = Vec::new();
    let iter = child.iter().map_err(|e| MediaError::FfmpegFailed {
        context: Some("trim export".into()),
        message: e.to_string(),
    })?;
    for event in iter {
        if let FfmpegEvent::Log(LogLevel::Error | LogLevel::Fatal, msg) = event {
            errors.push(msg);
        }
    }

    let status = child.wait()?;
    if !status.success() {
        let _ = std::fs::remove_file(dst);
        return Err(MediaError::FfmpegFailed {
            context: Some(format!("trimming {}", src.display())),
            message: if errors.is_empty() {
                format!("ffmpeg exited with {status}")
            } else {
                errors.join("; ")
            },
        });
    }
    Ok(TrimResult {
        actual_start_sec,
        duration_sec,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::generate_test_clip;

    #[test]
    fn trim_args_shape() {
        let args = trim_args(Path::new("/a.mp4"), Path::new("/b.mp4"), 1.5, 2.0);
        let joined = args.join(" ");
        assert!(joined.starts_with("-ss 1.500000 -i /a.mp4"), "{joined}");
        assert!(joined.contains("-t 2.000000"));
        assert!(joined.contains("-c copy"));
        assert!(joined.contains("-avoid_negative_ts make_zero"));
        assert!(joined.ends_with("/b.mp4"));
    }

    #[test]
    fn rejects_invalid_ranges() {
        let src = Path::new("/a.mp4");
        let dst = Path::new("/b.mp4");
        assert!(export_trim(src, dst, 2.0, 1.0).is_err());
        assert!(export_trim(src, dst, -1.0, 1.0).is_err());
        assert!(export_trim(src, dst, 1.0, 1.0).is_err());
    }

    #[test]
    fn finds_the_keyframe_at_or_before_a_time() {
        // Test clips are encoded with -g 30 at 30 fps → keyframes at 0s,
        // 1s, 2s.
        let src = generate_test_clip("export-kf", 320, 180, 30, 3);
        let kf = keyframe_at_or_before(&src, 1.5).unwrap();
        assert!((kf - 1.0).abs() < 0.05, "keyframe for 1.5 was {kf}");
        let kf = keyframe_at_or_before(&src, 1.0).unwrap();
        assert!((kf - 1.0).abs() < 0.05, "keyframe for 1.0 was {kf}");
        let kf = keyframe_at_or_before(&src, 0.4).unwrap();
        assert!(kf.abs() < 0.05, "keyframe for 0.4 was {kf}");
    }

    /// Real trim on a keyframe boundary: cut [1.0, 2.0) from a 3s clip and
    /// verify with ffprobe.
    #[test]
    fn trims_a_real_clip_with_correct_duration() {
        let src = generate_test_clip("export-trim", 320, 180, 30, 3);
        let dst = std::env::temp_dir().join("cutty-media-tests/export-trim-out.mp4");

        let result = export_trim(&src, &dst, 1.0, 2.0).unwrap();
        assert!((result.actual_start_sec - 1.0).abs() < 0.05);
        assert!((result.duration_sec - 1.0).abs() < 0.05);

        let info = crate::probe(&dst).unwrap();
        assert!(
            (info.duration_sec - 1.0).abs() < 0.2,
            "trimmed duration {} ≠ 1.0s",
            info.duration_sec
        );
        let v = info.video.expect("video stream kept");
        // Stream copy must not re-encode: same codec + dimensions.
        assert_eq!(v.codec, "h264");
        assert_eq!((v.width, v.height), (320, 180));
        assert!(info.audio.is_some(), "audio stream kept");
    }

    /// Off-keyframe in point: must snap back to the previous keyframe and
    /// still produce exactly the predicted duration.
    #[test]
    fn off_keyframe_trim_snaps_back_deterministically() {
        let src = generate_test_clip("export-offkf", 320, 180, 30, 3);
        let dst = std::env::temp_dir().join("cutty-media-tests/export-offkf-out.mp4");

        let result = export_trim(&src, &dst, 1.5, 2.5).unwrap();
        assert!(
            (result.actual_start_sec - 1.0).abs() < 0.05,
            "in point should snap 1.5 → 1.0, got {}",
            result.actual_start_sec
        );
        assert!((result.duration_sec - 1.5).abs() < 0.05);

        let info = crate::probe(&dst).unwrap();
        assert!(
            (info.duration_sec - result.duration_sec).abs() < 0.2,
            "actual duration {} ≠ predicted {}",
            info.duration_sec,
            result.duration_sec
        );
    }
}
