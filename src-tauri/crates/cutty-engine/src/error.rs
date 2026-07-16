//! Engine error type.

use crate::model::{ClipId, MediaId, TrackId};

/// Errors produced by timeline commands and engine operations.
///
/// Every failed command leaves the project untouched — commands are applied
/// to a clone and only committed after full invariant validation.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum EngineError {
    /// Referenced media does not exist in the project's media pool.
    #[error("unknown media id {0:?}")]
    UnknownMedia(MediaId),

    /// Referenced track does not exist.
    #[error("unknown track id {0:?}")]
    UnknownTrack(TrackId),

    /// Referenced clip does not exist (on the given track, if one was named).
    #[error("unknown clip id {0:?}")]
    UnknownClip(ClipId),

    /// A clip's timeline range is empty, inverted, negative, or non-finite.
    #[error("invalid time range on clip {clip:?}: [{timeline_in}, {timeline_out})")]
    InvalidTimeRange {
        clip: ClipId,
        timeline_in: f64,
        timeline_out: f64,
    },

    /// Two clips on the same track occupy overlapping timeline ranges.
    #[error("clip {a:?} overlaps clip {b:?} on track {track:?}")]
    ClipOverlap {
        track: TrackId,
        a: ClipId,
        b: ClipId,
    },

    /// A clip's source range lies outside `[0, media duration]`.
    #[error(
        "clip {clip:?} source range [{source_in}, {source_out}) exceeds media duration {media_duration}"
    )]
    SourceOutOfBounds {
        clip: ClipId,
        source_in: f64,
        source_out: f64,
        media_duration: f64,
    },

    /// Source span does not equal timeline span × speed.
    #[error(
        "clip {clip:?}: source span {source_span} != timeline span {timeline_span} * speed {speed}"
    )]
    SpeedMismatch {
        clip: ClipId,
        timeline_span: f64,
        source_span: f64,
        speed: f64,
    },

    /// A clip property (opacity, volume, speed, transform) is out of range
    /// or non-finite.
    #[error("clip {clip:?}: invalid property {property}: {value}")]
    InvalidProperty {
        clip: ClipId,
        property: &'static str,
        value: f64,
    },

    /// Media kind is incompatible with the track kind (e.g. audio-only
    /// media on a video track).
    #[error("media {media:?} is incompatible with track {track:?}")]
    IncompatibleMedia { track: TrackId, media: MediaId },

    /// Split point is not strictly inside the clip.
    #[error("split point {at} is outside clip {clip:?} (must be strictly inside)")]
    SplitOutOfRange { clip: ClipId, at: f64 },

    /// A transition is malformed or has no cut to bind to (no touching
    /// next clip on the track, or the clip is not on a video track).
    #[error("invalid transition on clip {clip:?}: {reason}")]
    InvalidTransition { clip: ClipId, reason: &'static str },

    /// A text payload is malformed, or a clip's payload doesn't match
    /// its track kind (text clips live on text tracks and carry no
    /// media; every other clip is a media clip).
    #[error("invalid text on clip {clip:?}: {reason}")]
    InvalidText { clip: ClipId, reason: &'static str },

    /// A keyframe lane violates its invariants (ordering, separation,
    /// empty lane, or a prop the clip cannot animate).
    #[error("invalid keyframes on clip {clip:?}: {reason}")]
    InvalidKeyframes { clip: ClipId, reason: &'static str },

    /// No keyframe at (or near) the given clip-relative time.
    #[error("no keyframe at {t}s on clip {clip:?}")]
    UnknownKeyframe { clip: ClipId, t: f64 },

    /// The clip cannot have its audio extracted (not a video clip, or
    /// its media has no audio stream).
    #[error("cannot extract audio from clip {clip:?}: {reason}")]
    ExtractAudio { clip: ClipId, reason: &'static str },

    /// The operation targets a locked track. `name` is carried so the UI
    /// can say which track without a second lookup.
    #[error("track \"{name}\" is locked — unlock it to edit")]
    TrackLocked { track: TrackId, name: String },

    /// Removing this track would leave the project without any track of
    /// its kind (the editor always keeps at least one video and one
    /// audio lane).
    #[error("cannot remove the last {kind} track")]
    LastTrackOfKind { track: TrackId, kind: &'static str },

    /// An id appears more than once in the project.
    #[error("duplicate id {0} in project")]
    DuplicateId(u64),

    /// `begin_transaction` was called while a transaction is already open.
    #[error("a transaction is already active")]
    TransactionActive,

    /// `commit`/`rollback` was called with no open transaction.
    #[error("no transaction is active")]
    NoTransaction,
}
