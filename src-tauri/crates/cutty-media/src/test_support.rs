//! Shared helpers for tests that need real media files (system ffmpeg).

use std::path::PathBuf;
use std::process::Command;

use ffmpeg_sidecar::paths::ffmpeg_path;

/// Generate (once per test run) a small H.264+AAC clip with lavfi sources.
/// Returns its path. Panics on failure — tests require system ffmpeg.
pub fn generate_test_clip(name: &str, width: u32, height: u32, fps: u32, secs: u32) -> PathBuf {
    let dir = std::env::temp_dir().join("cutty-media-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(format!("{name}-{width}x{height}-{fps}fps-{secs}s.mp4"));
    if file.is_file() {
        return file;
    }
    let status = Command::new(ffmpeg_path())
        .args(["-y", "-f", "lavfi", "-i"])
        .arg(format!(
            "testsrc2=size={width}x{height}:rate={fps}:duration={secs}"
        ))
        .args(["-f", "lavfi", "-i"])
        .arg(format!(
            "sine=frequency=440:sample_rate=48000:duration={secs}"
        ))
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-g", "30"])
        .args(["-c:a", "aac", "-b:a", "128k", "-shortest"])
        .arg(&file)
        .status()
        .expect("system ffmpeg must be installed for tests");
    assert!(status.success(), "test clip generation failed");
    file
}

