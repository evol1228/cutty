//! 720p proxy generation.
//!
//! Proxies are H.264/AAC MP4s constrained to fit 1280×720, with a dense
//! keyframe interval so playback seeks stay under the 100 ms budget. They
//! live in the XDG cache directory, keyed by source path + size + mtime, so
//! an edited/replaced source file gets a fresh proxy automatically.

use std::path::{Path, PathBuf};

use ffmpeg_sidecar::command::FfmpegCommand;
use ffmpeg_sidecar::event::{FfmpegEvent, LogLevel};

use crate::error::MediaError;
use crate::tools::ensure_tools;

/// Progress of a running proxy generation.
#[derive(Debug, Clone, Copy)]
pub struct ProxyProgress {
    /// 0.0–100.0. Best-effort: requires the source duration to be known.
    pub percent: f32,
    /// Seconds of output encoded so far.
    pub out_time_sec: f64,
    /// Encode speed as a multiple of realtime.
    pub speed: f32,
}

/// Where a proxy for `src` lives (or will live) in the cache.
///
/// Returns `(final_path, exists)`.
pub fn proxy_path_for(src: &Path) -> Result<(PathBuf, bool), MediaError> {
    let meta = std::fs::metadata(src)?;
    let mtime_nanos = meta
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let file_name = proxy_cache_filename(&src.display().to_string(), meta.len(), mtime_nanos);
    let path = proxy_cache_dir()?.join(file_name);
    let exists = path.is_file();
    Ok((path, exists))
}

/// The proxy cache directory: `$XDG_CACHE_HOME/cutty/proxies`.
pub fn proxy_cache_dir() -> Result<PathBuf, MediaError> {
    let root = dirs::cache_dir().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "could not determine the XDG cache directory",
        )
    })?;
    Ok(root.join("cutty").join("proxies"))
}

/// Deterministic cache file name for a source file's identity.
///
/// Keyed on path + size + mtime so a modified source invalidates its proxy.
fn proxy_cache_filename(path: &str, size: u64, mtime_nanos: u128) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(path.as_bytes());
    hasher.update(&size.to_le_bytes());
    hasher.update(&mtime_nanos.to_le_bytes());
    let hash = hasher.finalize().to_hex();
    format!("{}.mp4", &hash.as_str()[..32])
}

/// The ffmpeg argument list for encoding `src` into a 720p proxy at `dst`.
///
/// Split out for testability. Notes:
/// - Fits within 1280×720 without upscaling, preserving aspect ratio
///   (vertical 9:16 sources become x×720).
/// - `-g 30` forces a keyframe at least every 30 frames so seeking any
///   position decodes at most one second of frames.
/// - Audio is normalized to stereo LC-AAC, which symphonia can decode.
fn proxy_args(src: &Path, dst: &Path) -> Vec<String> {
    [
        "-i",
        &src.display().to_string(),
        "-vf",
        "scale='min(1280,iw)':'min(720,ih)':force_original_aspect_ratio=decrease:force_divisible_by=2",
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-crf",
        "23",
        "-pix_fmt",
        "yuv420p",
        // Constant frame rate: playback timestamp math assumes frame n is
        // exactly at n/fps.
        "-fps_mode",
        "cfr",
        "-g",
        "30",
        "-c:a",
        "aac",
        "-b:a",
        "128k",
        "-ac",
        "2",
        "-y",
        &dst.display().to_string(),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Generate a 720p proxy for `src`, reporting progress via `on_progress`.
///
/// Blocking — run on a background thread. Returns the proxy path. If a
/// proxy for this exact source (path + size + mtime) is already cached, it
/// is returned immediately. The proxy is written to a `.part` file and
/// renamed on success, so an interrupted encode never poisons the cache.
pub fn generate_proxy(
    src: &Path,
    duration_hint: Option<f64>,
    mut on_progress: impl FnMut(ProxyProgress),
) -> Result<PathBuf, MediaError> {
    ensure_tools()?;

    let (final_path, exists) = proxy_path_for(src)?;
    if exists {
        on_progress(ProxyProgress {
            percent: 100.0,
            out_time_sec: 0.0,
            speed: 0.0,
        });
        return Ok(final_path);
    }

    std::fs::create_dir_all(proxy_cache_dir()?)?;
    let part_path = final_path.with_extension("part.mp4");

    let mut child = FfmpegCommand::new()
        .args(proxy_args(src, &part_path).iter().map(String::as_str))
        .spawn()
        .map_err(|source| MediaError::Spawn {
            tool: "ffmpeg",
            source,
        })?;

    let mut duration = duration_hint.unwrap_or(0.0);
    let mut errors: Vec<String> = Vec::new();

    let iter = child.iter().map_err(|e| MediaError::FfmpegFailed {
        context: Some("generating proxy".into()),
        message: e.to_string(),
    })?;

    for event in iter {
        match event {
            FfmpegEvent::ParsedDuration(d) => duration = d.duration,
            FfmpegEvent::Progress(p) => {
                let out_time_sec = parse_ffmpeg_time(&p.time);
                let percent = if duration > 0.0 {
                    ((out_time_sec / duration) * 100.0).clamp(0.0, 100.0) as f32
                } else {
                    0.0
                };
                on_progress(ProxyProgress {
                    percent,
                    out_time_sec,
                    speed: p.speed,
                });
            }
            FfmpegEvent::Log(LogLevel::Error | LogLevel::Fatal, msg) => errors.push(msg),
            _ => {}
        }
    }

    // Note: `FfmpegEvent::Done` is only emitted for stdout outputs; for
    // file outputs the sidecar even injects a spurious `Error("No streams
    // found")` event. The exit status is the ground truth here.
    let status = child.wait()?;
    if !status.success() {
        let _ = std::fs::remove_file(&part_path);
        return Err(MediaError::FfmpegFailed {
            context: Some(format!("generating proxy for {}", src.display())),
            message: if errors.is_empty() {
                format!("ffmpeg exited with {status}")
            } else {
                errors.join("; ")
            },
        });
    }

    std::fs::rename(&part_path, &final_path)?;
    on_progress(ProxyProgress {
        percent: 100.0,
        out_time_sec: duration,
        speed: 0.0,
    });
    Ok(final_path)
}

/// Parse an ffmpeg progress time string like `"00:03:29.04"` into seconds.
///
/// ffmpeg can emit `"N/A"` before the first timestamped frame — that (and
/// any other unparsable form) yields 0.0.
pub(crate) fn parse_ffmpeg_time(t: &str) -> f64 {
    let parts: Vec<&str> = t.split(':').collect();
    match parts.as_slice() {
        [h, m, s] => {
            let (Ok(h), Ok(m), Ok(s)) = (h.parse::<f64>(), m.parse::<f64>(), s.parse::<f64>())
            else {
                return 0.0;
            };
            h * 3600.0 + m * 60.0 + s
        }
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_filename_is_deterministic_and_keyed_on_identity() {
        let a = proxy_cache_filename("/videos/a.mp4", 1000, 42);
        let b = proxy_cache_filename("/videos/a.mp4", 1000, 42);
        assert_eq!(a, b);
        assert!(a.ends_with(".mp4"));
        assert_eq!(a.len(), 32 + 4);

        // Any identity component change yields a different proxy.
        assert_ne!(a, proxy_cache_filename("/videos/b.mp4", 1000, 42));
        assert_ne!(a, proxy_cache_filename("/videos/a.mp4", 1001, 42));
        assert_ne!(a, proxy_cache_filename("/videos/a.mp4", 1000, 43));
    }

    #[test]
    fn proxy_args_shape() {
        let args = proxy_args(Path::new("/in.mp4"), Path::new("/out/proxy.mp4"));
        let joined = args.join(" ");
        assert!(joined.contains("min(1280,iw)"), "must fit 1280 wide");
        assert!(joined.contains("min(720,ih)"), "must fit 720 tall");
        assert!(joined.contains("force_original_aspect_ratio=decrease"));
        assert!(joined.contains("-c:v libx264"));
        assert!(joined.contains("-g 30"), "dense keyframes for fast seek");
        assert!(joined.contains("-c:a aac"));
        assert!(joined.ends_with("/out/proxy.mp4"));
        assert_eq!(args.first().map(String::as_str), Some("-i"));
    }

    #[test]
    fn parses_progress_time() {
        assert_eq!(parse_ffmpeg_time("00:00:00.00"), 0.0);
        assert_eq!(parse_ffmpeg_time("00:03:29.04"), 209.04);
        assert_eq!(parse_ffmpeg_time("01:00:00.50"), 3600.5);
        assert_eq!(parse_ffmpeg_time("N/A"), 0.0);
        assert_eq!(parse_ffmpeg_time("garbage"), 0.0);
        assert_eq!(parse_ffmpeg_time("1:2"), 0.0);
    }

    /// Real end-to-end proxy generation from a generated 1080p clip.
    #[test]
    fn generates_a_real_proxy() {
        use ffmpeg_sidecar::paths::ffmpeg_path;
        use std::process::Command;

        let dir = std::env::temp_dir().join("cutty-media-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("proxy-src.mp4");

        let status = Command::new(ffmpeg_path())
            .args(["-y", "-f", "lavfi", "-i"])
            .arg("testsrc2=size=1920x1080:rate=30:duration=2")
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=2"])
            .args(["-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac"])
            .arg(&src)
            .status()
            .expect("system ffmpeg must be installed for tests");
        assert!(status.success());

        let mut saw_progress = false;
        let proxy = generate_proxy(&src, Some(2.0), |_| saw_progress = true).unwrap();
        assert!(proxy.is_file());
        assert!(saw_progress);

        // The proxy must fit 720p and stay decodable.
        let info = crate::probe(&proxy).unwrap();
        let v = info.video.expect("proxy has video");
        assert!(v.width <= 1280 && v.height <= 720, "{}x{}", v.width, v.height);
        assert_eq!(v.codec, "h264");
        assert!(info.audio.is_some(), "proxy keeps audio");
        assert!((info.duration_sec - 2.0).abs() < 0.3);

        // Second call must hit the cache (returns instantly with 100%).
        let mut pct = 0.0;
        let cached = generate_proxy(&src, None, |p| pct = p.percent).unwrap();
        assert_eq!(cached, proxy);
        assert_eq!(pct, 100.0);
    }
}
