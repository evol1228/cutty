//! The command system. Every timeline mutation is a [`Command`] with
//! `apply`/`invert`; the engine owns the undo/redo stacks.
//!
//! Commands are **self-contained**: they capture, at construction time,
//! both the old and the new values they touch. `invert` therefore never
//! recomputes anything — undo restores the exact stored f64s, which is what
//! makes undo/redo round-trips bit-identical under serialization (no
//! `x - a + a` float drift).

use crate::error::EngineError;
use crate::model::{
    BlendMode, Clip, ClipId, MediaRef, Project, TextSpec, Track, TrackId, Transform, Transition,
};

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

/// Remove a media file from the pool together with every clip that
/// references it — the one place a pool mutation is undoable, because
/// silently dropping timeline clips must be reversible. Captures the
/// media ref and all removed clips verbatim.
#[derive(Debug, Clone)]
pub struct RemoveMedia {
    pub media: MediaRef,
    /// Every removed clip with its host track.
    pub removed: Vec<(TrackId, Clip)>,
}

impl Command for RemoveMedia {
    fn apply(&self, project: &mut Project) -> Result<(), EngineError> {
        for (track_id, clip) in &self.removed {
            take_clip(project, *track_id, clip.id)?;
        }
        let idx = project
            .media
            .iter()
            .position(|m| m.id == self.media.id)
            .ok_or(EngineError::UnknownMedia(self.media.id))?;
        project.media.remove(idx);
        Ok(())
    }

    fn invert(&self) -> Box<dyn Command> {
        Box::new(RestoreMedia {
            media: self.media.clone(),
            removed: self.removed.clone(),
        })
    }

    fn name(&self) -> &'static str {
        "RemoveMedia"
    }
}

/// Re-register a media file and re-insert its clips — the inverse of
/// [`RemoveMedia`], only ever constructed as such.
#[derive(Debug, Clone)]
pub struct RestoreMedia {
    pub media: MediaRef,
    pub removed: Vec<(TrackId, Clip)>,
}

impl Command for RestoreMedia {
    fn apply(&self, project: &mut Project) -> Result<(), EngineError> {
        project.media.push(self.media.clone());
        for (track_id, clip) in &self.removed {
            insert_clip(project, *track_id, clip.clone())?;
        }
        Ok(())
    }

    fn invert(&self) -> Box<dyn Command> {
        Box::new(RemoveMedia {
            media: self.media.clone(),
            removed: self.removed.clone(),
        })
    }

    fn name(&self) -> &'static str {
        "RestoreMedia"
    }
}

/// Reposition a clip onto another track (source range and duration
/// unchanged; the timeline position may change in the same step — a drag
/// moves on both axes at once). Old and new placements are captured
/// verbatim.
#[derive(Debug, Clone)]
pub struct MoveClipToTrack {
    pub clip_id: ClipId,
    pub old_track: TrackId,
    pub new_track: TrackId,
    pub old_timeline_in: f64,
    pub old_timeline_out: f64,
    pub new_timeline_in: f64,
    pub new_timeline_out: f64,
}

impl Command for MoveClipToTrack {
    fn apply(&self, project: &mut Project) -> Result<(), EngineError> {
        let mut clip = take_clip(project, self.old_track, self.clip_id)?;
        clip.timeline_in = self.new_timeline_in;
        clip.timeline_out = self.new_timeline_out;
        insert_clip(project, self.new_track, clip)
    }

    fn invert(&self) -> Box<dyn Command> {
        Box::new(MoveClipToTrack {
            clip_id: self.clip_id,
            old_track: self.new_track,
            new_track: self.old_track,
            old_timeline_in: self.new_timeline_in,
            old_timeline_out: self.new_timeline_out,
            new_timeline_in: self.old_timeline_in,
            new_timeline_out: self.old_timeline_out,
        })
    }

    fn name(&self) -> &'static str {
        "MoveClipToTrack"
    }
}

/// Insert a track at a panel position. Inverse of [`RemoveTrack`]. The
/// track is captured whole (usually empty on user "add track", but the
/// undo of a remove restores every clip verbatim).
#[derive(Debug, Clone)]
pub struct AddTrack {
    /// Panel index to insert at (0 = top).
    pub index: usize,
    pub track: Track,
}

impl Command for AddTrack {
    fn apply(&self, project: &mut Project) -> Result<(), EngineError> {
        if self.index > project.tracks.len() {
            return Err(EngineError::UnknownTrack(self.track.id));
        }
        project.tracks.insert(self.index, self.track.clone());
        Ok(())
    }

    fn invert(&self) -> Box<dyn Command> {
        Box::new(RemoveTrack {
            index: self.index,
            track: self.track.clone(),
        })
    }

    fn name(&self) -> &'static str {
        "AddTrack"
    }
}

/// Remove a track (with all its clips — they are stored verbatim so the
/// inverse restores them).
#[derive(Debug, Clone)]
pub struct RemoveTrack {
    pub index: usize,
    pub track: Track,
}

impl Command for RemoveTrack {
    fn apply(&self, project: &mut Project) -> Result<(), EngineError> {
        if project.tracks.get(self.index).map(|t| t.id) != Some(self.track.id) {
            return Err(EngineError::UnknownTrack(self.track.id));
        }
        project.tracks.remove(self.index);
        Ok(())
    }

    fn invert(&self) -> Box<dyn Command> {
        Box::new(AddTrack {
            index: self.index,
            track: self.track.clone(),
        })
    }

    fn name(&self) -> &'static str {
        "RemoveTrack"
    }
}

/// Move a track to another panel position (render order follows panel
/// order, so this restacks the composite).
#[derive(Debug, Clone)]
pub struct MoveTrack {
    pub track_id: TrackId,
    pub from: usize,
    pub to: usize,
}

impl Command for MoveTrack {
    fn apply(&self, project: &mut Project) -> Result<(), EngineError> {
        if project.tracks.get(self.from).map(|t| t.id) != Some(self.track_id)
            || self.to >= project.tracks.len()
        {
            return Err(EngineError::UnknownTrack(self.track_id));
        }
        let track = project.tracks.remove(self.from);
        project.tracks.insert(self.to, track);
        Ok(())
    }

    fn invert(&self) -> Box<dyn Command> {
        Box::new(MoveTrack {
            track_id: self.track_id,
            from: self.to,
            to: self.from,
        })
    }

    fn name(&self) -> &'static str {
        "MoveTrack"
    }
}

/// Which per-track switch a [`SetTrackFlag`] toggles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TrackFlag {
    /// Rejects edits (enforced by the engine's public operations).
    Locked,
    /// Silences audio (audio tracks and video clips' embedded audio).
    Muted,
    /// Excludes a video track from the composite.
    Hidden,
}

/// Flip one of a track's flags. Old and new captured verbatim.
#[derive(Debug, Clone)]
pub struct SetTrackFlag {
    pub track_id: TrackId,
    pub flag: TrackFlag,
    pub old: bool,
    pub new: bool,
}

impl Command for SetTrackFlag {
    fn apply(&self, project: &mut Project) -> Result<(), EngineError> {
        let track = project
            .track_mut(self.track_id)
            .ok_or(EngineError::UnknownTrack(self.track_id))?;
        match self.flag {
            TrackFlag::Locked => track.locked = self.new,
            TrackFlag::Muted => track.muted = self.new,
            TrackFlag::Hidden => track.hidden = self.new,
        }
        Ok(())
    }

    fn invert(&self) -> Box<dyn Command> {
        Box::new(SetTrackFlag {
            track_id: self.track_id,
            flag: self.flag,
            old: self.new,
            new: self.old,
        })
    }

    fn name(&self) -> &'static str {
        "SetTrackFlag"
    }
}

/// Set a clip's 2D placement. The whole transform is captured both ways
/// (a gizmo drag changes several fields per step; one command keeps the
/// wire and the undo story simple).
#[derive(Debug, Clone)]
pub struct SetClipTransform {
    pub track_id: TrackId,
    pub clip_id: ClipId,
    pub old: Transform,
    pub new: Transform,
}

impl Command for SetClipTransform {
    fn apply(&self, project: &mut Project) -> Result<(), EngineError> {
        let track = project
            .track_mut(self.track_id)
            .ok_or(EngineError::UnknownTrack(self.track_id))?;
        let clip = track
            .clip_mut(self.clip_id)
            .ok_or(EngineError::UnknownClip(self.clip_id))?;
        clip.transform = self.new.clone();
        Ok(())
    }

    fn invert(&self) -> Box<dyn Command> {
        Box::new(SetClipTransform {
            track_id: self.track_id,
            clip_id: self.clip_id,
            old: self.new.clone(),
            new: self.old.clone(),
        })
    }

    fn name(&self) -> &'static str {
        "SetClipTransform"
    }
}

/// Set a clip's opacity (0.0..=1.0).
#[derive(Debug, Clone)]
pub struct SetClipOpacity {
    pub track_id: TrackId,
    pub clip_id: ClipId,
    pub old: f64,
    pub new: f64,
}

impl Command for SetClipOpacity {
    fn apply(&self, project: &mut Project) -> Result<(), EngineError> {
        let track = project
            .track_mut(self.track_id)
            .ok_or(EngineError::UnknownTrack(self.track_id))?;
        let clip = track
            .clip_mut(self.clip_id)
            .ok_or(EngineError::UnknownClip(self.clip_id))?;
        clip.opacity = self.new;
        Ok(())
    }

    fn invert(&self) -> Box<dyn Command> {
        Box::new(SetClipOpacity {
            track_id: self.track_id,
            clip_id: self.clip_id,
            old: self.new,
            new: self.old,
        })
    }

    fn name(&self) -> &'static str {
        "SetClipOpacity"
    }
}

/// Set how a clip blends with the layers below it.
#[derive(Debug, Clone)]
pub struct SetClipBlendMode {
    pub track_id: TrackId,
    pub clip_id: ClipId,
    pub old: BlendMode,
    pub new: BlendMode,
}

impl Command for SetClipBlendMode {
    fn apply(&self, project: &mut Project) -> Result<(), EngineError> {
        let track = project
            .track_mut(self.track_id)
            .ok_or(EngineError::UnknownTrack(self.track_id))?;
        let clip = track
            .clip_mut(self.clip_id)
            .ok_or(EngineError::UnknownClip(self.clip_id))?;
        clip.blend_mode = self.new;
        Ok(())
    }

    fn invert(&self) -> Box<dyn Command> {
        Box::new(SetClipBlendMode {
            track_id: self.track_id,
            clip_id: self.clip_id,
            old: self.new,
            new: self.old,
        })
    }

    fn name(&self) -> &'static str {
        "SetClipBlendMode"
    }
}

/// Change a clip's audio gain. Old and new values are captured verbatim,
/// so undo restores the exact float.
#[derive(Debug, Clone)]
pub struct SetClipVolume {
    pub track_id: TrackId,
    pub clip_id: ClipId,
    pub old: f64,
    pub new: f64,
}

impl Command for SetClipVolume {
    fn apply(&self, project: &mut Project) -> Result<(), EngineError> {
        let track = project
            .track_mut(self.track_id)
            .ok_or(EngineError::UnknownTrack(self.track_id))?;
        let clip = track
            .clip_mut(self.clip_id)
            .ok_or(EngineError::UnknownClip(self.clip_id))?;
        clip.volume = self.new;
        Ok(())
    }

    fn invert(&self) -> Box<dyn Command> {
        Box::new(SetClipVolume {
            track_id: self.track_id,
            clip_id: self.clip_id,
            old: self.new,
            new: self.old,
        })
    }

    fn name(&self) -> &'static str {
        "SetClipVolume"
    }
}

/// Replace a text clip's whole payload (content and/or style). One
/// command for both keeps the wire simple: the Inspector coalesces a
/// typing burst into one transaction, and any single change is exactly
/// reversible from the captured old payload.
#[derive(Debug, Clone)]
pub struct SetClipText {
    pub track_id: TrackId,
    pub clip_id: ClipId,
    pub old: TextSpec,
    pub new: TextSpec,
}

impl Command for SetClipText {
    fn apply(&self, project: &mut Project) -> Result<(), EngineError> {
        let track = project
            .track_mut(self.track_id)
            .ok_or(EngineError::UnknownTrack(self.track_id))?;
        let clip = track
            .clip_mut(self.clip_id)
            .ok_or(EngineError::UnknownClip(self.clip_id))?;
        if clip.text.is_none() {
            return Err(EngineError::InvalidText {
                clip: self.clip_id,
                reason: "not a text clip",
            });
        }
        clip.text = Some(self.new.clone());
        Ok(())
    }

    fn invert(&self) -> Box<dyn Command> {
        Box::new(SetClipText {
            track_id: self.track_id,
            clip_id: self.clip_id,
            old: self.new.clone(),
            new: self.old.clone(),
        })
    }

    fn name(&self) -> &'static str {
        "SetClipText"
    }
}

/// Set (add, replace, or remove) the transition at a clip's out cut. Old
/// and new values captured verbatim, so one command type covers the whole
/// lifecycle and undo restores the exact prior state.
#[derive(Debug, Clone)]
pub struct SetTransition {
    pub track_id: TrackId,
    pub clip_id: ClipId,
    pub old: Option<Transition>,
    pub new: Option<Transition>,
}

impl Command for SetTransition {
    fn apply(&self, project: &mut Project) -> Result<(), EngineError> {
        let track = project
            .track_mut(self.track_id)
            .ok_or(EngineError::UnknownTrack(self.track_id))?;
        let clip = track
            .clip_mut(self.clip_id)
            .ok_or(EngineError::UnknownClip(self.clip_id))?;
        clip.transition_out = self.new.clone();
        Ok(())
    }

    fn invert(&self) -> Box<dyn Command> {
        Box::new(SetTransition {
            track_id: self.track_id,
            clip_id: self.clip_id,
            old: self.new.clone(),
            new: self.old.clone(),
        })
    }

    fn name(&self) -> &'static str {
        "SetTransition"
    }
}

/// A structural command plus its consequences, applied as one atomic,
/// invertible unit — e.g. a clip move together with the removal of the
/// transitions whose cuts that move destroys. Parts apply in order; the
/// inverse applies the inverted parts in reverse order. Constructed only
/// by the engine (`execute_structural`), never sent over the wire.
#[derive(Debug)]
pub struct Compound {
    /// The primary command's name (compounds are invisible in labels).
    pub name: &'static str,
    pub parts: Vec<Box<dyn Command>>,
}

impl Command for Compound {
    fn apply(&self, project: &mut Project) -> Result<(), EngineError> {
        for part in &self.parts {
            part.apply(project)?;
        }
        Ok(())
    }

    fn invert(&self) -> Box<dyn Command> {
        Box::new(Compound {
            name: self.name,
            parts: self.parts.iter().rev().map(|p| p.invert()).collect(),
        })
    }

    fn name(&self) -> &'static str {
        self.name
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
