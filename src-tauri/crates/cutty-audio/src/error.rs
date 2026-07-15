//! Error types for audio playback.

/// Errors produced by audio decode and output.
#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    /// The file has no audio track symphonia can find.
    #[error("no audio track found")]
    NoAudioTrack,

    /// The audio track is missing parameters we need (rate/channels/timebase).
    #[error("audio track is missing {0}")]
    MissingParams(&'static str),

    /// Demux/decode error from symphonia.
    #[error("audio decode: {0}")]
    Symphonia(#[from] symphonia::core::errors::Error),

    /// Output device error from cpal.
    #[error("audio output: {0}")]
    Cpal(#[from] cpal::Error),

    /// No usable output device.
    #[error("no audio output device available")]
    NoDevice,

    /// The default output device offers a configuration the mixer cannot
    /// drive (e.g. a non-f32 sample format).
    #[error("unsupported audio output configuration: {0}")]
    UnsupportedDevice(String),

    /// A clip failed to open or decode during an offline render. Unlike
    /// live playback (which degrades to silence), offline rendering fails
    /// loudly — exported audio must never be silently wrong.
    #[error("offline render of {path}: {message}")]
    OfflineRender { path: String, message: String },

    /// An offline render was cancelled by the caller.
    #[error("render cancelled")]
    Cancelled,

    /// Generic I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
