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

    /// The output device does not support the file's sample rate.
    #[error("output device does not support {0} Hz (resampling lands post-Phase 0)")]
    UnsupportedRate(u32),

    /// Generic I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
