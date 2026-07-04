//! # cutty-audio
//!
//! Audio decode (symphonia) and output (cpal). The audio thread owns the
//! master clock — video frame presentation chases it, never the reverse.
//!
//! Phase 0: single-file playback with a sample-accurate position clock.

pub mod clock;
pub mod error;
pub mod player;

pub use clock::PlaybackClock;
pub use error::AudioError;
pub use player::AudioPlayer;
