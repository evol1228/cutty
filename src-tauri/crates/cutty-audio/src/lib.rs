//! # cutty-audio
//!
//! Audio decode (symphonia) and output (cpal). The audio thread owns the
//! master clock — video frame presentation chases it, never the reverse.
//!
//! Phase 1: [`TimelineAudio`] mixes every audio-contributing clip at the
//! playhead sample-accurately (cuts land on exact samples, gaps render
//! silence) and drives the [`PlaybackClock`] from the device callback.

pub mod clock;
pub mod error;
pub mod mixer;
pub mod offline;
pub mod source;

pub use clock::PlaybackClock;
pub use error::AudioError;
pub use mixer::{
    soft_clip, symphonia_factory, AudioSegment, Easing, EnvelopePoint, FadeRamp, MixerTimeline,
    SourceFactory, TimelineAudio, VolumeEnvelope, SOFT_CLIP_KNEE,
};
pub use offline::{
    render_timeline_to_wav, render_timeline_to_wav_with_factory, EXPORT_SAMPLE_RATE,
};
pub use source::{AudioSource, SymphoniaSource};
