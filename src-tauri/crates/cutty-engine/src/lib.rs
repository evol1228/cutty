//! # cutty-engine
//!
//! Owns all project and timeline state: the project model, timeline
//! operations, the command system (apply/invert), undo/redo, transactions,
//! snapping, and playback resolution.
//!
//! ## Architecture
//!
//! - [`model`] — the serde-serializable project model ([`Project`],
//!   [`Track`], [`Clip`]) and its invariants.
//! - [`command`] — the [`Command`] trait and the concrete commands. Every
//!   mutation goes through a command; there is no other mutation path.
//! - [`engine`] — the [`Engine`]: applies commands atomically, owns the
//!   undo/redo stacks, coalesces drag gestures via transactions, and emits
//!   [`EngineEvent`] snapshots after each committed change.
//! - [`snap`] — pure snapping over candidate times (UI converts pixels to
//!   seconds first).
//! - [`resolve`] — timeline time → active clips + source positions, the
//!   basis for playback.
//!
//! All times are seconds (`f64`). The frontend never mutates state — it
//! sends commands over IPC and renders the snapshot events.

pub mod command;
pub mod engine;
pub mod error;
pub mod model;
pub mod resolve;
pub mod snap;

pub use command::{
    AddClip, ApplyTransaction, ClipSpan, Command, DeleteClip, JoinClips, MoveClip, RippleDelete,
    RippleInsert, RippleMove, SplitClip, TrimClip,
};
pub use engine::{Engine, EngineEvent, TrimEdge};
pub use error::EngineError;
pub use model::{
    Clip, ClipId, MediaId, MediaRef, Project, ProjectSettings, Track, TrackId, TrackKind,
    Transform, EPS, MIN_CLIP_DURATION,
};
pub use resolve::{resolve, ActiveClip};
pub use snap::{snap, snap_candidates};
