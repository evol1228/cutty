//! Single-frame thumbnail extraction for media-pool items and clip labels.
//!
//! Thumbnails are small JPEGs (320px wide, aspect preserved, display
//! rotation applied by ffmpeg) cached in `$XDG_CACHE_HOME/cutty/thumbs`,
//! keyed by source path + size + mtime like proxies. Full filmstrips are a
//! Phase 2 concern — this is one representative frame per media file.

use std::path::{Path, PathBuf};
use std::process::Command;

use ffmpeg_sidecar::paths::ffmpeg_path;

use crate::cache::cache_entry_for;
use crate::error::MediaError;
use crate::tools::ensure_tools;

/// Thumbnail width in pixels. Height follows the source aspect ratio.
pub const THUMBNAIL_WIDTH: u32 = 320;

/// Where a thumbnail for `src` lives (or will live) in the cache.
///
/// Returns `(final_path, exists)`.
pub fn thumbnail_path_for(src: &Path) -> Result<(PathBuf, bool), MediaError> {
    cache_entry_for(src, "thumbs", "jpg")
}

/// Pick the frame to thumbnail: 10% into the media (capped at 3s) so we
/// skip black lead-ins without seeking far into long files. Unknown
/// durations use the first frame.
fn thumbnail_seek_sec(duration_hint: Option<f64>) -> f64 {
    match duration_hint {
        Some(d) if d.is_finite() && d > 0.0 => (d * 0.1).min(3.0),
        _ => 0.0,
    }
}

/// The ffmpeg argument list for extracting a thumbnail of `src` at `dst`.
///
/// Split out for testability. `-ss` before `-i` is a fast keyframe seek —
/// frame-exact placement is irrelevant for a thumbnail. ffmpeg applies
/// display-matrix rotation when transcoding, so phone footage comes out
/// upright.
fn thumbnail_args(src: &Path, dst: &Path, seek_sec: f64) -> Vec<String> {
    [
        "-ss",
        &format!("{seek_sec:.3}"),
        "-i",
        &src.display().to_string(),
        "-frames:v",
        "1",
        "-vf",
        &format!("scale={THUMBNAIL_WIDTH}:-2"),
        "-q:v",
        "4",
        "-y",
        &dst.display().to_string(),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Extract (or fetch from cache) the thumbnail JPEG for `src`.
///
/// Blocking — run on a background thread. Returns the thumbnail path.
/// Fails for sources without a decodable video stream (the frontend only
/// requests thumbnails for media with video).
pub fn generate_thumbnail(src: &Path, duration_hint: Option<f64>) -> Result<PathBuf, MediaError> {
    ensure_tools()?;

    let (final_path, exists) = thumbnail_path_for(src)?;
    if exists {
        return Ok(final_path);
    }

    std::fs::create_dir_all(crate::cache::cache_dir("thumbs")?)?;
    // Unique per invocation so concurrent generations for the same source
    // never scribble on each other's partial file (last rename wins).
    static PART_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let part_path = final_path.with_extension(format!(
        "part-{}-{}.jpg",
        std::process::id(),
        PART_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));

    let output = Command::new(ffmpeg_path())
        .args(thumbnail_args(src, &part_path, thumbnail_seek_sec(duration_hint)).iter())
        .output()
        .map_err(|source| MediaError::Spawn {
            tool: "ffmpeg",
            source,
        })?;

    // ffmpeg can exit 0 without producing output (e.g. seek past EOF), so
    // the file's existence is part of the success condition.
    if !output.status.success() || !part_path.is_file() {
        let _ = std::fs::remove_file(&part_path);
        return Err(MediaError::FfmpegFailed {
            context: Some(format!("extracting thumbnail for {}", src.display())),
            message: String::from_utf8_lossy(&output.stderr)
                .lines()
                .last()
                .unwrap_or("no output produced")
                .to_string(),
        });
    }

    std::fs::rename(&part_path, &final_path)?;
    Ok(final_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seek_time_skips_lead_in_but_stays_early() {
        assert_eq!(thumbnail_seek_sec(None), 0.0);
        assert_eq!(thumbnail_seek_sec(Some(0.0)), 0.0);
        assert_eq!(thumbnail_seek_sec(Some(f64::NAN)), 0.0);
        assert_eq!(thumbnail_seek_sec(Some(10.0)), 1.0);
        assert_eq!(thumbnail_seek_sec(Some(90.0)), 3.0, "capped at 3s");
        assert!(
            thumbnail_seek_sec(Some(0.5)) < 0.5,
            "stays inside the media"
        );
    }

    #[test]
    fn thumbnail_args_shape() {
        let args = thumbnail_args(Path::new("/in.mp4"), Path::new("/out/t.jpg"), 1.5);
        let joined = args.join(" ");
        assert!(joined.starts_with("-ss 1.500 -i /in.mp4"), "{joined}");
        assert!(joined.contains("-frames:v 1"));
        assert!(joined.contains("scale=320:-2"));
        assert!(joined.ends_with("/out/t.jpg"));
    }

    /// Real end-to-end thumbnail from a generated clip, plus cache hit.
    #[test]
    fn generates_a_real_thumbnail() {
        let dir = std::env::temp_dir().join("cutty-media-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("thumb-src.mp4");

        let status = Command::new(ffmpeg_path())
            .args(["-y", "-f", "lavfi", "-i"])
            .arg("testsrc2=size=1280x720:rate=30:duration=2")
            .args(["-c:v", "libx264", "-preset", "ultrafast"])
            .arg(&src)
            .status()
            .expect("system ffmpeg must be installed for tests");
        assert!(status.success());

        let thumb = generate_thumbnail(&src, Some(2.0)).unwrap();
        assert!(thumb.is_file());
        let bytes = std::fs::read(&thumb).unwrap();
        assert!(bytes.starts_with(&[0xFF, 0xD8]), "JPEG magic");

        // Second call must hit the cache (same path, no re-encode).
        let cached = generate_thumbnail(&src, None).unwrap();
        assert_eq!(cached, thumb);

        // Audio-only sources have no frame to extract.
        let audio = dir.join("thumb-audio.wav");
        let status = Command::new(ffmpeg_path())
            .args(["-y", "-f", "lavfi", "-i", "sine=frequency=440:duration=1"])
            .arg(&audio)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(generate_thumbnail(&audio, Some(1.0)).is_err());
    }
}
