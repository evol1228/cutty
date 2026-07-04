//! The command system. Every timeline mutation is a [`Command`] with
//! `apply`/`invert`; the engine owns the undo/redo stacks.
//!
//! Commands are **self-contained**: they capture, at construction time,
//! both the old and the new values they touch. `invert` therefore never
//! recomputes anything — undo restores the exact stored f64s, which is what
//! makes undo/redo round-trips bit-identical under serialization (no
//! `x - a + a` float drift).

use crate::error::EngineError;
use crate::model::{Clip, ClipId, Project, TrackId};

/// A reversible timeline mutation.
///
/// `apply` must either fully succeed or leave `project` in a state the
/// engine will discard (the engine applies commands to a clone and
/// validates before committing, so partial mutations never escape).
pub trait Command: std::fmt::Debug + Send {
    /// Mutate the project. Invariant checking beyond identity lookups is
    /// the engine's job (full [`Project::validate`] after every apply).
    fn apply(&self, project: &mut Project) -> Result<(), EngineError>;

    /// The command that exactly reverses this one.
    fn invert(&self) -> Box<dyn Command>;

    /// Imperative name for logs and (later) undo-menu labels.
    fn name(&self) -> &'static str;
}

fn take_clip(project: &mut Project, track: TrackId, clip: ClipId) -> Result<Clip, EngineError> {
    let track = project
        .track_mut(track)
        .ok_or(EngineError::UnknownTrack(track))?;
    let idx = track
        .clips
        .iter()
        .position(|c| c.id == clip)
        .ok_or(EngineError::UnknownClip(clip))?;
    Ok(track.clips.remove(idx))
}

fn insert_clip(project: &mut Project, track: TrackId, clip: Clip) -> Result<(), EngineError> {
    let track = project
        .track_mut(track)
        .ok_or(EngineError::UnknownTrack(track))?;
    track.clips.push(clip);
    track.sort_clips();
    Ok(())
}

/// Timeline + source placement of a clip, captured verbatim so that
/// undo restores the exact original floats.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClipSpan {
    pub timeline_in: f64,
    pub timeline_out: f64,
    pub source_in: f64,
    pub source_out: f64,
}

impl ClipSpan {
    pub fn of(clip: &Clip) -> Self {
        Self {
            timeline_in: clip.timeline_in,
            timeline_out: clip.timeline_out,
            source_in: clip.source_in,
            source_out: clip.source_out,
        }
    }

    fn write_to(&self, clip: &mut Clip) {
        clip.timeline_in = self.timeline_in;
        clip.timeline_out = self.timeline_out;
        clip.source_in = self.source_in;
        clip.source_out = self.source_out;
    }
}

/// Add a fully-formed clip to a track. Inverse of [`DeleteClip`].
#[derive(Debug, Clone)]
pub struct AddClip {
    pub track_id: TrackId,
    pub clip: Clip,
}

impl Command for AddClip {
    fn apply(&self, project: &mut Project) -> Result<(), EngineError> {
        insert_clip(project, self.track_id, self.clip.clone())
    }

    fn invert(&self) -> Box<dyn Command> {
        Box::new(DeleteClip {
            track_id: self.track_id,
            clip: self.clip.clone(),
        })
    }

    fn name(&self) -> &'static str {
        "AddClip"
    }
}

/// Remove a clip from a track. Stores the full clip so the inverse can
/// restore it verbatim.
#[derive(Debug, Clone)]
pub struct DeleteClip {
    pub track_id: TrackId,
    pub clip: Clip,
}

impl Command for DeleteClip {
    fn apply(&self, project: &mut Project) -> Result<(), EngineError> {
        take_clip(project, self.track_id, self.clip.id).map(|_| ())
    }

    fn invert(&self) -> Box<dyn Command> {
        Box::new(AddClip {
            track_id: self.track_id,
            clip: self.clip.clone(),
        })
    }

    fn name(&self) -> &'static str {
        "DeleteClip"
    }
}

/// Reposition a clip on its track (source range unchanged). Phase 1 has a
/// single track per kind, so cross-track moves don't exist yet.
#[derive(Debug, Clone)]
pub struct MoveClip {
    pub track_id: TrackId,
    pub clip_id: ClipId,
    pub old_timeline_in: f64,
    pub old_timeline_out: f64,
    pub new_timeline_in: f64,
    pub new_timeline_out: f64,
}

impl Command for MoveClip {
    fn apply(&self, project: &mut Project) -> Result<(), EngineError> {
        let track = project
            .track_mut(self.track_id)
            .ok_or(EngineError::UnknownTrack(self.track_id))?;
        let clip = track
            .clip_mut(self.clip_id)
            .ok_or(EngineError::UnknownClip(self.clip_id))?;
        clip.timeline_in = self.new_timeline_in;
        clip.timeline_out = self.new_timeline_out;
        track.sort_clips();
        Ok(())
    }

    fn invert(&self) -> Box<dyn Command> {
        Box::new(MoveClip {
            track_id: self.track_id,
            clip_id: self.clip_id,
            old_timeline_in: self.new_timeline_in,
            old_timeline_out: self.new_timeline_out,
            new_timeline_in: self.old_timeline_in,
            new_timeline_out: self.old_timeline_out,
        })
    }

    fn name(&self) -> &'static str {
        "MoveClip"
    }
}

/// Re-span a clip (either edge trimmed; timeline and source move together).
/// Old and new spans are both stored, so the inverse is just a swap.
#[derive(Debug, Clone)]
pub struct TrimClip {
    pub track_id: TrackId,
    pub clip_id: ClipId,
    pub old: ClipSpan,
    pub new: ClipSpan,
}

impl Command for TrimClip {
    fn apply(&self, project: &mut Project) -> Result<(), EngineError> {
        let track = project
            .track_mut(self.track_id)
            .ok_or(EngineError::UnknownTrack(self.track_id))?;
        let clip = track
            .clip_mut(self.clip_id)
            .ok_or(EngineError::UnknownClip(self.clip_id))?;
        self.new.write_to(clip);
        track.sort_clips();
        Ok(())
    }

    fn invert(&self) -> Box<dyn Command> {
        Box::new(TrimClip {
            track_id: self.track_id,
            clip_id: self.clip_id,
            old: self.new,
            new: self.old,
        })
    }

    fn name(&self) -> &'static str {
        "TrimClip"
    }
}

/// Split one clip into two at a timeline point. Stores the original clip
/// and both halves verbatim; the inverse ([`JoinClips`]) restores the
/// original.
#[derive(Debug, Clone)]
pub struct SplitClip {
    pub track_id: TrackId,
    /// The clip as it was before the split (keeps its id in the left half).
    pub original: Clip,
    pub left: Clip,
    pub right: Clip,
}

impl Command for SplitClip {
    fn apply(&self, project: &mut Project) -> Result<(), EngineError> {
        take_clip(project, self.track_id, self.original.id)?;
        insert_clip(project, self.track_id, self.left.clone())?;
        insert_clip(project, self.track_id, self.right.clone())
    }

    fn invert(&self) -> Box<dyn Command> {
        Box::new(JoinClips {
            track_id: self.track_id,
            original: self.original.clone(),
            left: self.left.clone(),
            right: self.right.clone(),
        })
    }

    fn name(&self) -> &'static str {
        "SplitClip"
    }
}

/// Replace the two halves of a previous split with the original clip.
/// Only ever constructed as the inverse of [`SplitClip`].
#[derive(Debug, Clone)]
pub struct JoinClips {
    pub track_id: TrackId,
    pub original: Clip,
    pub left: Clip,
    pub right: Clip,
}

impl Command for JoinClips {
    fn apply(&self, project: &mut Project) -> Result<(), EngineError> {
        take_clip(project, self.track_id, self.left.id)?;
        take_clip(project, self.track_id, self.right.id)?;
        insert_clip(project, self.track_id, self.original.clone())
    }

    fn invert(&self) -> Box<dyn Command> {
        Box::new(SplitClip {
            track_id: self.track_id,
            original: self.original.clone(),
            left: self.left.clone(),
            right: self.right.clone(),
        })
    }

    fn name(&self) -> &'static str {
        "JoinClips"
    }
}

/// One clip's before/after placement inside a ripple operation. Both
/// positions stored verbatim (undo must not re-derive via arithmetic).
#[derive(Debug, Clone, Copy)]
pub struct RippleMove {
    pub clip_id: ClipId,
    pub old_timeline_in: f64,
    pub old_timeline_out: f64,
    pub new_timeline_in: f64,
    pub new_timeline_out: f64,
}

impl RippleMove {
    fn flipped(&self) -> Self {
        Self {
            clip_id: self.clip_id,
            old_timeline_in: self.new_timeline_in,
            old_timeline_out: self.new_timeline_out,
            new_timeline_in: self.old_timeline_in,
            new_timeline_out: self.old_timeline_out,
        }
    }
}

fn apply_ripple_moves(
    project: &mut Project,
    track_id: TrackId,
    moves: &[RippleMove],
) -> Result<(), EngineError> {
    let track = project
        .track_mut(track_id)
        .ok_or(EngineError::UnknownTrack(track_id))?;
    for mv in moves {
        let clip = track
            .clip_mut(mv.clip_id)
            .ok_or(EngineError::UnknownClip(mv.clip_id))?;
        clip.timeline_in = mv.new_timeline_in;
        clip.timeline_out = mv.new_timeline_out;
    }
    track.sort_clips();
    Ok(())
}

/// Delete a clip and shift every later clip on the track left by the
/// deleted clip's duration (gaps between the later clips are preserved).
#[derive(Debug, Clone)]
pub struct RippleDelete {
    pub track_id: TrackId,
    pub clip: Clip,
    pub moves: Vec<RippleMove>,
}

impl Command for RippleDelete {
    fn apply(&self, project: &mut Project) -> Result<(), EngineError> {
        take_clip(project, self.track_id, self.clip.id)?;
        apply_ripple_moves(project, self.track_id, &self.moves)
    }

    fn invert(&self) -> Box<dyn Command> {
        Box::new(RippleInsert {
            track_id: self.track_id,
            clip: self.clip.clone(),
            moves: self.moves.iter().map(RippleMove::flipped).collect(),
        })
    }

    fn name(&self) -> &'static str {
        "RippleDelete"
    }
}

/// Shift clips back right and re-insert a clip — the inverse of
/// [`RippleDelete`], only ever constructed as such.
#[derive(Debug, Clone)]
pub struct RippleInsert {
    pub track_id: TrackId,
    pub clip: Clip,
    pub moves: Vec<RippleMove>,
}

impl Command for RippleInsert {
    fn apply(&self, project: &mut Project) -> Result<(), EngineError> {
        apply_ripple_moves(project, self.track_id, &self.moves)?;
        insert_clip(project, self.track_id, self.clip.clone())
    }

    fn invert(&self) -> Box<dyn Command> {
        Box::new(RippleDelete {
            track_id: self.track_id,
            clip: self.clip.clone(),
            moves: self.moves.iter().map(RippleMove::flipped).collect(),
        })
    }

    fn name(&self) -> &'static str {
        "RippleInsert"
    }
}

/// The single undo entry produced by committing a transaction (e.g. a drag
/// gesture of many transient micro-moves). Snapshot-based: projects are
/// small, and a verbatim before/after pair is exactly reversible by
/// construction.
#[derive(Debug, Clone)]
pub struct ApplyTransaction {
    pub before: Project,
    pub after: Project,
}

impl Command for ApplyTransaction {
    fn apply(&self, project: &mut Project) -> Result<(), EngineError> {
        *project = self.after.clone();
        Ok(())
    }

    fn invert(&self) -> Box<dyn Command> {
        Box::new(ApplyTransaction {
            before: self.after.clone(),
            after: self.before.clone(),
        })
    }

    fn name(&self) -> &'static str {
        "ApplyTransaction"
    }
}
