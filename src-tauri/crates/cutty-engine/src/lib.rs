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
//! - [`project_file`] — versioned `.cutty` save/load with migrations and
//!   relative media paths.
//! - [`autosave`] — debounced background autosave + crash-recovery scan.
//! - [`recents`] — the recent-projects list.
//!
//! All times are seconds (`f64`). The frontend never mutates state — it
//! sends commands over IPC and renders the snapshot events.

pub mod autosave;
pub mod command;
pub mod engine;
pub mod error;
pub mod model;
pub mod project_file;
pub mod recents;
pub mod resolve;
pub mod snap;

pub use autosave::{
    scan_recovery, slot_key, AutosaveConfig, AutosaveEvent, AutosaveTick, Autosaver,
    RecoveryCandidate,
};
pub use command::{
    AddClip, ApplyTransaction, ClipSpan, Command, DeleteClip, JoinClips, MoveClip, RemoveMedia,
    RestoreMedia, RippleDelete, RippleInsert, RippleMove, SplitClip, TrimClip,
};
pub use engine::{Engine, EngineEvent, TrimEdge};
pub use error::EngineError;
pub use model::{
    Clip, ClipId, MediaId, MediaRef, Project, ProjectSettings, Track, TrackId, TrackKind,
    Transform, EPS, MIN_CLIP_DURATION,
};
pub use project_file::ProjectFileError;
pub use recents::RecentProject;
pub use resolve::{active_video_clip, next_boundary_after, resolve, timeline_end, ActiveClip};
pub use snap::{snap, snap_candidates, snap_clip_move, snap_time, SnappedMove};

/// Cutty's XDG state directory (`$XDG_STATE_HOME/cutty`, usually
/// `~/.local/state/cutty`): autosaves, the recents list. `None` when the
/// environment defines no home.
pub fn state_dir() -> Option<std::path::PathBuf> {
    dirs::state_dir().map(|d| d.join("cutty"))
}
