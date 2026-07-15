//! Error types for media operations.

/// Errors produced by media probing, proxy generation, decoding, and export.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    /// A required external tool (ffmpeg/ffprobe) is not installed.
    #[error(
        "{tool} was not found on this system. Cutty needs the system {tool} binary — \
         install it with `sudo pacman -S ffmpeg` (Arch) or your distro's package manager."
    )]
    ToolMissing { tool: &'static str },

    /// Failed to spawn or communicate with an external tool.
    #[error("failed to run {tool}: {source}")]
    Spawn {
        tool: &'static str,
        #[source]
        source: std::io::Error,
    },

    /// ffprobe exited with an error for the given file.
    #[error("could not read media file {path}: {stderr}")]
    ProbeFailed { path: String, stderr: String },

    /// ffprobe output was not in the expected JSON shape.
    #[error("could not parse ffprobe output: {0}")]
    ProbeParse(#[from] serde_json::Error),

    /// The file contains no video or audio stream Cutty can use.
    #[error("no decodable video or audio streams found in {path}")]
    NoStreams { path: String },

    /// ffmpeg exited with an error.
    #[error("ffmpeg failed{}: {message}", context.as_ref().map(|c| format!(" while {c}")).unwrap_or_default())]
    FfmpegFailed {
        context: Option<String>,
        message: String,
    },

    /// JPEG (turbojpeg) encoding failed.
    #[error("JPEG encoding failed: {0}")]
    Jpeg(String),

    /// The project cannot be exported yet (e.g. empty timeline, proxies
    /// still generating). The message is user-actionable.
    #[error("{message}")]
    ExportNotReady { message: String },

    /// The export was cancelled by the user. Not a failure — callers
    /// clean up and report "cancelled", never "error".
    #[error("export cancelled")]
    ExportCancelled,

    /// GPU compositor failure (init, composite, or readback).
    #[error("GPU compositor: {0}")]
    Gpu(#[from] cutty_gpu::GpuError),

    /// Offline audio rendering failed.
    #[error("audio render failed: {0}")]
    Audio(#[from] cutty_audio::AudioError),

    /// Generic I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
