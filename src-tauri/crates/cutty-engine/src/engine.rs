//! The engine: owns the [`Project`], routes every mutation through the
//! command system, keeps the undo/redo stacks, and emits state events.

use serde::Serialize;

use crate::command::{
    AddClip, ApplyTransaction, ClipSpan, Command, DeleteClip, MoveClip, RemoveMedia, RippleDelete,
    RippleMove, SetClipVolume, SplitClip, TrimClip,
};
use crate::error::EngineError;
use crate::model::{
    BlendMode, Clip, ClipId, MediaId, MediaRef, Project, ProjectSettings, TrackId, Transform, EPS,
    MIN_CLIP_DURATION,
};

/// Which clip edge a trim operation drags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrimEdge {
    /// The left edge (`timeline_in` / `source_in`).
    Start,
    /// The right edge (`timeline_out` / `source_out`).
    End,
}

/// State events emitted by the engine after each committed change.
///
/// Currently a full project snapshot every time: projects are small (a few
/// KB of JSON), so snapshots keep the frontend trivially consistent.
/// Granular diff events are a later optimization once profiling demands it.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum EngineEvent {
    /// The project changed (committed command, transient transaction
    /// mutation, undo, redo, or media registration).
    ProjectChanged { project: Project },
}

/// The single owner of all editable state.
///
/// Every timeline mutation goes through [`Command`]s applied by this type —
/// there is no other mutation path. Commands are applied to a clone of the
/// project and committed only after full invariant validation, so a failed
/// command never leaves partial state behind.
#[derive(Debug)]
pub struct Engine {
    project: Project,
    undo_stack: Vec<Box<dyn Command>>,
    redo_stack: Vec<Box<dyn Command>>,
    /// Snapshot taken at `begin_transaction`; `Some` while a transaction is
    /// open. Commands executed while open become transient (no undo
    /// entries) until `commit_transaction` folds them into one entry.
    transaction_before: Option<Project>,
    /// Monotonic id source for media, tracks, and clips. Deliberately not
    /// part of [`Project`] so undo/redo round-trips serialize identically.
    id_counter: u64,
    /// Pending events; the IPC layer drains and forwards these. Undrained
    /// events accumulate, so hosts must call [`Engine::drain_events`]
    /// regularly (tests may ignore them).
    events: Vec<EngineEvent>,
}

impl Engine {
    /// A fresh engine with an empty Phase 1 project (one video + one audio
    /// track).
    pub fn new(settings: ProjectSettings) -> Self {
        let project = Project::new(settings, TrackId(1), TrackId(2));
        Self {
            id_counter: project.max_id(),
            project,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            transaction_before: None,
            events: Vec::new(),
        }
    }

    /// Adopt a previously saved project (validates it first). Undo history
    /// starts empty — history is not persisted in `.cutty` files.
    pub fn from_project(project: Project) -> Result<Self, EngineError> {
        project.validate()?;
        Ok(Self {
            id_counter: project.max_id(),
            project,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            transaction_before: None,
            events: Vec::new(),
        })
    }

    /// Read access to the current project state.
    pub fn project(&self) -> &Project {
        &self.project
    }

    /// Number of entries on the undo stack.
    pub fn undo_depth(&self) -> usize {
        self.undo_stack.len()
    }

    /// Number of entries on the redo stack.
    pub fn redo_depth(&self) -> usize {
        self.redo_stack.len()
    }

    /// Whether a transaction is currently open.
    pub fn transaction_active(&self) -> bool {
        self.transaction_before.is_some()
    }

    /// Take all pending events (oldest first).
    pub fn drain_events(&mut self) -> Vec<EngineEvent> {
        std::mem::take(&mut self.events)
    }

    fn next_id(&mut self) -> u64 {
        self.id_counter += 1;
        self.id_counter
    }

    fn emit_snapshot(&mut self) {
        self.events.push(EngineEvent::ProjectChanged {
            project: self.project.clone(),
        });
    }

    /// Apply a command atomically: run it on a clone, validate every model
    /// invariant, then commit. Outside a transaction the command lands on
    /// the undo stack; inside one it is transient (the transaction commit
    /// produces the single undo entry).
    fn execute(&mut self, command: Box<dyn Command>) -> Result<(), EngineError> {
        let mut candidate = self.project.clone();
        command.apply(&mut candidate)?;
        candidate.validate()?;
        self.project = candidate;
        if self.transaction_before.is_none() {
            self.undo_stack.push(command);
            self.redo_stack.clear();
        }
        // Transient transaction mutations also emit: the UI needs live
        // preview state during a drag.
        self.emit_snapshot();
        Ok(())
    }

    // ------------------------------------------------------------------
    // Media pool
    // ------------------------------------------------------------------

    /// Register a media file in the project's media pool.
    ///
    /// Media registration is not a timeline mutation and is deliberately
    /// not undoable (matching every mainstream editor); clips referencing
    /// the media are what undo tracks. Removal is different — see
    /// [`Engine::remove_media`].
    pub fn add_media(
        &mut self,
        path: impl Into<String>,
        duration: f64,
        has_video: bool,
        has_audio: bool,
    ) -> Result<MediaId, EngineError> {
        if !duration.is_finite() || duration <= 0.0 {
            return Err(EngineError::InvalidProperty {
                clip: ClipId(0),
                property: "media.duration",
                value: duration,
            });
        }
        let id = MediaId(self.next_id());
        self.project.media.push(MediaRef {
            id,
            path: path.into(),
            duration,
            has_video,
            has_audio,
        });
        self.emit_snapshot();
        Ok(id)
    }

    /// Remove a media file from the pool **and every clip referencing it**,
    /// as a single undoable command. Unlike [`Engine::add_media`] this goes
    /// through the command system: dropping timeline clips must be
    /// reversible, so undo restores the media ref and all its clips
    /// verbatim.
    pub fn remove_media(&mut self, media_id: MediaId) -> Result<(), EngineError> {
        let media = self
            .project
            .media(media_id)
            .ok_or(EngineError::UnknownMedia(media_id))?
            .clone();
        let removed: Vec<_> = self
            .project
            .tracks
            .iter()
            .flat_map(|t| {
                t.clips
                    .iter()
                    .filter(|c| c.media_id == media_id)
                    .map(|c| (t.id, c.clone()))
            })
            .collect();
        self.execute(Box::new(RemoveMedia { media, removed }))
    }

    // ------------------------------------------------------------------
    // Timeline operations (all routed through the command system)
    // ------------------------------------------------------------------

    /// Place a new clip on a track. `timeline_out` is derived from the
    /// source range (speed is fixed at 1.0 in Phase 1). Fails if the clip
    /// would overlap an existing clip or exceed media bounds.
    pub fn add_clip(
        &mut self,
        track_id: TrackId,
        media_id: MediaId,
        timeline_in: f64,
        source_in: f64,
        source_out: f64,
    ) -> Result<ClipId, EngineError> {
        self.project
            .media(media_id)
            .ok_or(EngineError::UnknownMedia(media_id))?;
        if self.project.track(track_id).is_none() {
            return Err(EngineError::UnknownTrack(track_id));
        }
        let speed = 1.0;
        let id = ClipId(self.next_id());
        let clip = Clip {
            id,
            media_id,
            timeline_in,
            timeline_out: timeline_in + (source_out - source_in) / speed,
            source_in,
            source_out,
            transform: Transform::default(),
            opacity: 1.0,
            blend_mode: BlendMode::default(),
            speed,
            volume: 1.0,
        };
        self.execute(Box::new(AddClip { track_id, clip }))?;
        Ok(id)
    }

    /// Move a clip to a new timeline position (clamped to `>= 0`); duration
    /// and source range are unchanged. Fails on overlap.
    pub fn move_clip(&mut self, clip_id: ClipId, timeline_in: f64) -> Result<(), EngineError> {
        let (track, clip) = self
            .project
            .find_clip(clip_id)
            .ok_or(EngineError::UnknownClip(clip_id))?;
        if !timeline_in.is_finite() {
            return Err(EngineError::InvalidTimeRange {
                clip: clip_id,
                timeline_in,
                timeline_out: timeline_in,
            });
        }
        let new_in = timeline_in.max(0.0);
        self.execute(Box::new(MoveClip {
            track_id: track.id,
            clip_id,
            old_timeline_in: clip.timeline_in,
            old_timeline_out: clip.timeline_out,
            new_timeline_in: new_in,
            new_timeline_out: new_in + clip.duration(),
        }))
    }

    /// Drag one edge of a clip to (approximately) timeline time `to`,
    /// adjusting the source range correspondingly. The requested time is
    /// clamped to media bounds and to [`MIN_CLIP_DURATION`]; the clamped
    /// edge time actually applied is returned. Fails if the result would
    /// overlap a neighboring clip.
    pub fn trim_clip(
        &mut self,
        clip_id: ClipId,
        edge: TrimEdge,
        to: f64,
    ) -> Result<f64, EngineError> {
        let (track, clip) = self
            .project
            .find_clip(clip_id)
            .ok_or(EngineError::UnknownClip(clip_id))?;
        if !to.is_finite() {
            return Err(EngineError::InvalidTimeRange {
                clip: clip_id,
                timeline_in: to,
                timeline_out: to,
            });
        }
        let media = self
            .project
            .media(clip.media_id)
            .ok_or(EngineError::UnknownMedia(clip.media_id))?;
        let old = ClipSpan::of(clip);
        let mut new = old;

        let applied = match edge {
            TrimEdge::Start => {
                // Media headroom to the left, and timeline 0, bound the drag.
                let lo = (clip.timeline_in - clip.source_in / clip.speed).max(0.0);
                let hi = clip.timeline_out - MIN_CLIP_DURATION;
                if lo > hi {
                    return Err(EngineError::InvalidTimeRange {
                        clip: clip_id,
                        timeline_in: lo,
                        timeline_out: hi,
                    });
                }
                let t = to.clamp(lo, hi);
                new.timeline_in = t;
                new.source_in = (clip.source_in + (t - clip.timeline_in) * clip.speed).max(0.0);
                t
            }
            TrimEdge::End => {
                let lo = clip.timeline_in + MIN_CLIP_DURATION;
                let hi = clip.timeline_out + (media.duration - clip.source_out) / clip.speed;
                if lo > hi {
                    return Err(EngineError::InvalidTimeRange {
                        clip: clip_id,
                        timeline_in: lo,
                        timeline_out: hi,
                    });
                }
                let t = to.clamp(lo, hi);
                new.timeline_out = t;
                new.source_out =
                    (clip.source_out + (t - clip.timeline_out) * clip.speed).min(media.duration);
                t
            }
        };

        self.execute(Box::new(TrimClip {
            track_id: track.id,
            clip_id,
            old,
            new,
        }))?;
        Ok(applied)
    }

    /// Split a clip at timeline time `at` into two clips sharing the same
    /// media, transform, and properties. The left half keeps the original
    /// id; the new right half's id is returned. `at` must be strictly
    /// inside the clip (at least [`MIN_CLIP_DURATION`] from each edge) —
    /// splitting at an exact clip edge is rejected.
    pub fn split_clip(&mut self, clip_id: ClipId, at: f64) -> Result<ClipId, EngineError> {
        let (track, clip) = self
            .project
            .find_clip(clip_id)
            .ok_or(EngineError::UnknownClip(clip_id))?;
        if !at.is_finite()
            || at - clip.timeline_in < MIN_CLIP_DURATION
            || clip.timeline_out - at < MIN_CLIP_DURATION
        {
            return Err(EngineError::SplitOutOfRange { clip: clip_id, at });
        }
        let track_id = track.id;
        let original = clip.clone();
        let source_at = original.source_in + (at - original.timeline_in) * original.speed;

        let mut left = original.clone();
        left.timeline_out = at;
        left.source_out = source_at;

        let mut right = original.clone();
        right.id = ClipId(self.next_id());
        right.timeline_in = at;
        right.source_in = source_at;
        let right_id = right.id;

        self.execute(Box::new(SplitClip {
            track_id,
            original,
            left,
            right,
        }))?;
        Ok(right_id)
    }

    /// Set a clip's audio gain (linear; 1.0 = unity, 0.0 = silent). The
    /// mixer applies this both in preview and in export, so it is the one
    /// per-clip audio control of Phase 1.
    pub fn set_clip_volume(&mut self, clip_id: ClipId, volume: f64) -> Result<(), EngineError> {
        let (track, clip) = self
            .project
            .find_clip(clip_id)
            .ok_or(EngineError::UnknownClip(clip_id))?;
        if !volume.is_finite() || volume < 0.0 {
            return Err(EngineError::InvalidProperty {
                clip: clip_id,
                property: "volume",
                value: volume,
            });
        }
        self.execute(Box::new(SetClipVolume {
            track_id: track.id,
            clip_id,
            old: clip.volume,
            new: volume,
        }))
    }

    /// Remove a clip from its track, leaving a gap.
    pub fn delete_clip(&mut self, clip_id: ClipId) -> Result<(), EngineError> {
        let (track, clip) = self
            .project
            .find_clip(clip_id)
            .ok_or(EngineError::UnknownClip(clip_id))?;
        self.execute(Box::new(DeleteClip {
            track_id: track.id,
            clip: clip.clone(),
        }))
    }

    /// Remove a clip and shift every later clip on the same track left by
    /// the removed clip's duration (gaps between later clips are
    /// preserved).
    pub fn ripple_delete(&mut self, clip_id: ClipId) -> Result<(), EngineError> {
        let (track, clip) = self
            .project
            .find_clip(clip_id)
            .ok_or(EngineError::UnknownClip(clip_id))?;
        let duration = clip.duration();
        let moves: Vec<RippleMove> = track
            .clips
            .iter()
            .filter(|c| c.id != clip_id && c.timeline_in >= clip.timeline_out - EPS)
            .map(|c| RippleMove {
                clip_id: c.id,
                old_timeline_in: c.timeline_in,
                old_timeline_out: c.timeline_out,
                new_timeline_in: c.timeline_in - duration,
                new_timeline_out: c.timeline_out - duration,
            })
            .collect();
        self.execute(Box::new(RippleDelete {
            track_id: track.id,
            clip: clip.clone(),
            moves,
        }))
    }

    // ------------------------------------------------------------------
    // Undo / redo
    // ------------------------------------------------------------------

    /// Undo the most recent committed command. Returns `Ok(false)` when
    /// there is nothing to undo. Not allowed while a transaction is open.
    pub fn undo(&mut self) -> Result<bool, EngineError> {
        if self.transaction_active() {
            return Err(EngineError::TransactionActive);
        }
        let Some(command) = self.undo_stack.pop() else {
            return Ok(false);
        };
        let inverse = command.invert();
        let mut candidate = self.project.clone();
        inverse.apply(&mut candidate)?;
        candidate.validate()?;
        self.project = candidate;
        self.redo_stack.push(command);
        self.emit_snapshot();
        Ok(true)
    }

    /// Re-apply the most recently undone command. Returns `Ok(false)` when
    /// there is nothing to redo. Not allowed while a transaction is open.
    pub fn redo(&mut self) -> Result<bool, EngineError> {
        if self.transaction_active() {
            return Err(EngineError::TransactionActive);
        }
        let Some(command) = self.redo_stack.pop() else {
            return Ok(false);
        };
        let mut candidate = self.project.clone();
        command.apply(&mut candidate)?;
        candidate.validate()?;
        self.project = candidate;
        self.undo_stack.push(command);
        self.emit_snapshot();
        Ok(true)
    }

    // ------------------------------------------------------------------
    // Transactions (gesture coalescing)
    // ------------------------------------------------------------------

    /// Open a transaction: subsequent operations mutate live state (and
    /// emit preview events) but create no undo entries until
    /// [`Engine::commit_transaction`] folds them into exactly one. The UI
    /// calls this on mousedown of a drag gesture.
    pub fn begin_transaction(&mut self) -> Result<(), EngineError> {
        if self.transaction_active() {
            return Err(EngineError::TransactionActive);
        }
        self.transaction_before = Some(self.project.clone());
        Ok(())
    }

    /// Close the transaction, producing a single undo entry covering
    /// everything since [`Engine::begin_transaction`]. A no-op transaction
    /// (state unchanged) produces no undo entry. The UI calls this on
    /// mouseup.
    pub fn commit_transaction(&mut self) -> Result<(), EngineError> {
        let before = self
            .transaction_before
            .take()
            .ok_or(EngineError::NoTransaction)?;
        if before == self.project {
            return Ok(());
        }
        self.undo_stack.push(Box::new(ApplyTransaction {
            before,
            after: self.project.clone(),
        }));
        self.redo_stack.clear();
        self.emit_snapshot();
        Ok(())
    }

    /// Abort the transaction, restoring the state from
    /// [`Engine::begin_transaction`] (e.g. the user pressed Escape
    /// mid-drag).
    pub fn rollback_transaction(&mut self) -> Result<(), EngineError> {
        let before = self
            .transaction_before
            .take()
            .ok_or(EngineError::NoTransaction)?;
        self.project = before;
        self.emit_snapshot();
        Ok(())
    }
}
