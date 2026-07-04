//! Locating and validating the external ffmpeg/ffprobe binaries.
//!
//! Cutty relies on the system ffmpeg (Arch: `pacman -S ffmpeg`). We never
//! auto-download binaries — the `ffmpeg-sidecar` download feature is disabled.

use ffmpeg_sidecar::{command::ffmpeg_is_installed, ffprobe::ffprobe_is_installed};

use crate::error::MediaError;

/// Verify that both `ffmpeg` and `ffprobe` are available on this system.
///
/// Returns an actionable error naming the missing tool. Call this before any
/// operation that shells out, so users get a clear message instead of a
/// cryptic spawn failure.
pub fn ensure_tools() -> Result<(), MediaError> {
    if !ffmpeg_is_installed() {
        return Err(MediaError::ToolMissing { tool: "ffmpeg" });
    }
    if !ffprobe_is_installed() {
        return Err(MediaError::ToolMissing { tool: "ffprobe" });
    }
    Ok(())
}
