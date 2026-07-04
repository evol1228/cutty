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
        .arg(format!("sine=frequency=440:sample_rate=48000:duration={secs}"))
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-g", "30"])
        .args(["-c:a", "aac", "-b:a", "128k", "-shortest"])
        .arg(&file)
        .status()
        .expect("system ffmpeg must be installed for tests");
    assert!(status.success(), "test clip generation failed");
    file
}

/// A clip whose audio track is shorter than its video track — exercises
/// the master-clock stall path (audio ends, video must keep pacing).
pub fn generate_short_audio_clip(name: &str, video_secs: u32, audio_secs: u32) -> PathBuf {
    let dir = std::env::temp_dir().join("cutty-media-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(format!("{name}-v{video_secs}s-a{audio_secs}s.mp4"));
    if file.is_file() {
        return file;
    }
    let status = Command::new(ffmpeg_path())
        .args(["-y", "-f", "lavfi", "-i"])
        .arg(format!(
            "testsrc2=size=320x180:rate=30:duration={video_secs}"
        ))
        .args(["-f", "lavfi", "-i"])
        .arg(format!(
            "sine=frequency=440:sample_rate=48000:duration={audio_secs}"
        ))
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-g", "30"])
        .args(["-c:a", "aac", "-b:a", "128k"])
        .arg(&file)
        .status()
        .expect("system ffmpeg must be installed for tests");
    assert!(status.success(), "test clip generation failed");
    file
}

/// Same as [`generate_test_clip`] but with no audio track.
pub fn generate_video_only_clip(name: &str, fps: u32, secs: u32) -> PathBuf {
    let dir = std::env::temp_dir().join("cutty-media-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(format!("{name}-videoonly-{fps}fps-{secs}s.mp4"));
    if file.is_file() {
        return file;
    }
    let status = Command::new(ffmpeg_path())
        .args(["-y", "-f", "lavfi", "-i"])
        .arg(format!("testsrc2=size=320x180:rate={fps}:duration={secs}"))
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-g", "30", "-an"])
        .arg(&file)
        .status()
        .expect("system ffmpeg must be installed for tests");
    assert!(status.success(), "test clip generation failed");
    file
}
