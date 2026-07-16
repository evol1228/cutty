//! The engine: owns the [`Project`], routes every mutation through the
//! command system, keeps the undo/redo stacks, and emits state events.

use serde::Serialize;

use crate::command::{
    AddClip, AddKeyframe, AddTrack, ApplyTransaction, ClipSpan, Command, Compound, DeleteClip,
    MoveClip, MoveClipToTrack, MoveKeyframe, MoveTrack, RemoveKeyframe, RemoveMedia, RemoveTrack,
    RippleDelete, RippleMove, SetClipBlendMode, SetClipOpacity, SetClipText, SetClipTransform,
    SetClipVolume, SetKeyframes, SetTrackFlag, SetTransition, SplitClip, TrackFlag, TrimClip,
};
use crate::error::EngineError;
use crate::keyframes::{
    self, Easing, FadeSide, Keyframe, KeyframeProp, Keyframes, KEYFRAME_MIN_DT,
};
use crate::model::{
    clips_touch, BlendMode, Clip, ClipId, MediaId, MediaKind, MediaRef, Project, ProjectSettings,
    TextSpec, Track, TrackId, TrackKind, Transform, Transition, DEFAULT_STILL_CLIP_DURATION, EPS,
    MAX_TRANSITION_DURATION, MIN_CLIP_DURATION, MIN_TRANSITION_DURATION,
};
use crate::resolve::transition_duration_limit;

/// Matching tolerance when the UI names a keyframe by its time: half the
/// separation minimum, so a reference can never grab a neighbor.
const KEYFRAME_MATCH_EPS: f64 = KEYFRAME_MIN_DT / 2.0;

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

    /// Execute a structural command (anything that moves, resizes, or
    /// removes clips), pruning every transition whose cut it destroys —
    /// as **one** atomic, undoable unit. A transition whose two clips no
    /// longer touch after the command dangles; leaving it stored would
    /// silently rebind it to whatever clip lands on that edge next.
    fn execute_structural(&mut self, primary: Box<dyn Command>) -> Result<(), EngineError> {
        // Preview the primary command to find what it leaves dangling.
        let mut preview = self.project.clone();
        primary.apply(&mut preview)?;
        let prunes = Self::dangling_transitions(&preview);
        if prunes.is_empty() {
            return self.execute(primary);
        }
        let name = primary.name();
        let mut parts: Vec<Box<dyn Command>> = vec![primary];
        for (track_id, clip_id, old) in prunes {
            parts.push(Box::new(SetTransition {
                track_id,
                clip_id,
                old: Some(old),
                new: None,
            }));
        }
        self.execute(Box::new(Compound { name, parts }))
    }

    /// Transitions with no cut left to bind to: the owning clip has no
    /// touching next clip on its (video) track.
    fn dangling_transitions(project: &Project) -> Vec<(TrackId, ClipId, Transition)> {
        let mut dangling = Vec::new();
        for track in &project.tracks {
            for (i, clip) in track.clips.iter().enumerate() {
                let Some(transition) = &clip.transition_out else {
                    continue;
                };
                let bound = track.kind == TrackKind::Video
                    && track
                        .clips
                        .get(i + 1)
                        .is_some_and(|next| clips_touch(clip, next));
                if !bound {
                    dangling.push((track.id, clip.id, transition.clone()));
                }
            }
        }
        dangling
    }

    // ------------------------------------------------------------------
    // Media pool
    // ------------------------------------------------------------------

    /// Register a bounded media file (video or audio) in the project's
    /// media pool, deriving [`MediaKind`] from stream presence. Stills
    /// and GIFs go through [`Engine::add_media_with_kind`].
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
        let kind = if has_video {
            MediaKind::Video
        } else {
            MediaKind::Audio
        };
        self.add_media_with_kind(path, duration, has_video, has_audio, false, kind)
    }

    /// Register a media file of an explicit [`MediaKind`]. Duration must
    /// be finite and positive, except stills which store `0` (they have
    /// no intrinsic time; pass whatever the probe reported ≥ 0).
    pub fn add_media_with_kind(
        &mut self,
        path: impl Into<String>,
        duration: f64,
        has_video: bool,
        has_audio: bool,
        has_alpha: bool,
        kind: MediaKind,
    ) -> Result<MediaId, EngineError> {
        let duration = if kind == MediaKind::Image {
            0.0
        } else {
            duration
        };
        let media = MediaRef {
            id: MediaId(self.next_id()),
            path: path.into(),
            duration,
            has_video,
            has_audio,
            has_alpha,
            kind,
        };
        Project::validate_media_entry(&media)?;
        let id = media.id;
        self.project.media.push(media);
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
        // Removal deletes clips, and deleting from a locked track is an
        // edit like any other — unlock first.
        for track in &self.project.tracks {
            if track.clips.iter().any(|c| c.media_id == Some(media_id)) {
                Self::require_track_unlocked(track)?;
            }
        }
        let removed: Vec<_> = self
            .project
            .tracks
            .iter()
            .flat_map(|t| {
                t.clips
                    .iter()
                    .filter(|c| c.media_id == Some(media_id))
                    .map(|c| (t.id, c.clone()))
            })
            .collect();
        self.execute_structural(Box::new(RemoveMedia { media, removed }))
    }

    // ------------------------------------------------------------------
    // Track management (all routed through the command system)
    // ------------------------------------------------------------------

    /// The track holding `clip_id` must not be locked. Every public clip
    /// mutation calls this first; `Command::apply` itself never checks, so
    /// undo/redo can restore state on locked tracks.
    fn require_clip_unlocked(&self, clip_id: ClipId) -> Result<(), EngineError> {
        let (track, _) = self
            .project
            .find_clip(clip_id)
            .ok_or(EngineError::UnknownClip(clip_id))?;
        Self::require_track_unlocked(track)
    }

    fn require_track_unlocked(track: &Track) -> Result<(), EngineError> {
        if track.locked {
            return Err(EngineError::TrackLocked {
                track: track.id,
                name: track.name.clone(),
            });
        }
        Ok(())
    }

    /// Auto-name for a new track: `V<n>`/`A<n>`/`T<n>`, one past the
    /// highest existing number of that kind (so removals never cause
    /// collisions).
    fn next_track_name(&self, kind: TrackKind) -> String {
        let prefix = match kind {
            TrackKind::Video => 'V',
            TrackKind::Audio => 'A',
            TrackKind::Text => 'T',
        };
        let max_n = self
            .project
            .tracks
            .iter()
            .filter(|t| t.kind == kind)
            .filter_map(|t| {
                t.name
                    .strip_prefix(prefix)
                    .and_then(|n| n.parse::<u64>().ok())
            })
            .max()
            .unwrap_or(0);
        format!("{prefix}{}", max_n + 1)
    }

    /// Insert a new empty track at panel `index` (0 = top; clamped to the
    /// track list). Returns the new track's id. Undoable.
    pub fn add_track(&mut self, kind: TrackKind, index: usize) -> Result<TrackId, EngineError> {
        let id = TrackId(self.next_id());
        let track = Track::new(id, kind, self.next_track_name(kind));
        let index = index.min(self.project.tracks.len());
        self.execute(Box::new(AddTrack { index, track }))?;
        Ok(id)
    }

    /// Remove a track with all its clips (undo restores everything).
    /// Rejected on a locked track and for the last *video*/*audio* track
    /// — the editor always keeps at least one of each. Text tracks are
    /// exempt: they exist on demand and a project may have none.
    pub fn remove_track(&mut self, track_id: TrackId) -> Result<(), EngineError> {
        let index = self
            .project
            .tracks
            .iter()
            .position(|t| t.id == track_id)
            .ok_or(EngineError::UnknownTrack(track_id))?;
        let track = &self.project.tracks[index];
        Self::require_track_unlocked(track)?;
        let siblings = self
            .project
            .tracks
            .iter()
            .filter(|t| t.kind == track.kind)
            .count();
        if siblings <= 1 && track.kind != TrackKind::Text {
            return Err(EngineError::LastTrackOfKind {
                track: track_id,
                kind: match track.kind {
                    TrackKind::Video => "video",
                    TrackKind::Audio => "audio",
                    TrackKind::Text => "text", // unreachable: exempt above
                },
            });
        }
        self.execute(Box::new(RemoveTrack {
            index,
            track: track.clone(),
        }))
    }

    /// Move a track to panel position `to` (clamped). Render order follows
    /// panel order, so this restacks the video composite. Locked tracks
    /// may be reordered — lock protects content, not placement.
    pub fn move_track(&mut self, track_id: TrackId, to: usize) -> Result<(), EngineError> {
        let from = self
            .project
            .tracks
            .iter()
            .position(|t| t.id == track_id)
            .ok_or(EngineError::UnknownTrack(track_id))?;
        let to = to.min(self.project.tracks.len().saturating_sub(1));
        if from == to {
            return Ok(());
        }
        self.execute(Box::new(MoveTrack { track_id, from, to }))
    }

    /// Flip a per-track flag (lock / mute / hide). Always allowed — this
    /// is how a locked track gets unlocked.
    pub fn set_track_flag(
        &mut self,
        track_id: TrackId,
        flag: TrackFlag,
        value: bool,
    ) -> Result<(), EngineError> {
        let track = self
            .project
            .track(track_id)
            .ok_or(EngineError::UnknownTrack(track_id))?;
        let old = match flag {
            TrackFlag::Locked => track.locked,
            TrackFlag::Muted => track.muted,
            TrackFlag::Hidden => track.hidden,
        };
        if old == value {
            return Ok(());
        }
        self.execute(Box::new(SetTrackFlag {
            track_id,
            flag,
            old,
            new: value,
        }))
    }

    // ------------------------------------------------------------------
    // Timeline operations (all routed through the command system)
    // ------------------------------------------------------------------

    /// Place a new clip on a track. `timeline_out` is derived from the
    /// source range (speed is fixed at 1.0 in Phase 1). Fails if the clip
    /// would overlap an existing clip or exceed media bounds.
    ///
    /// Still images have no intrinsic duration, so a degenerate source
    /// range (what a caller naturally produces from `media.duration ==
    /// 0`) becomes the [`DEFAULT_STILL_CLIP_DURATION`] window.
    pub fn add_clip(
        &mut self,
        track_id: TrackId,
        media_id: MediaId,
        timeline_in: f64,
        source_in: f64,
        source_out: f64,
    ) -> Result<ClipId, EngineError> {
        let media = self
            .project
            .media(media_id)
            .ok_or(EngineError::UnknownMedia(media_id))?;
        let (source_in, source_out) =
            if media.kind == MediaKind::Image && source_out - source_in < MIN_CLIP_DURATION {
                (0.0, DEFAULT_STILL_CLIP_DURATION)
            } else {
                (source_in, source_out)
            };
        let track = self
            .project
            .track(track_id)
            .ok_or(EngineError::UnknownTrack(track_id))?;
        Self::require_track_unlocked(track)?;
        let speed = 1.0;
        let id = ClipId(self.next_id());
        let clip = Clip {
            id,
            media_id: Some(media_id),
            timeline_in,
            timeline_out: timeline_in + (source_out - source_in) / speed,
            source_in,
            source_out,
            transform: Transform::default(),
            opacity: 1.0,
            blend_mode: BlendMode::default(),
            speed,
            volume: 1.0,
            transition_out: None,
            text: None,
            keyframes: Keyframes::default(),
        };
        self.execute(Box::new(AddClip { track_id, clip }))?;
        Ok(id)
    }

    /// Place a new text clip at `timeline_in` for `duration` seconds.
    ///
    /// `track`: a specific text track, or `None` for CapCut-style
    /// placement — the topmost unlocked text track with room takes the
    /// clip; when none has room (or none exists) a new text lane is
    /// created at the top of the panel, and track + clip land as **one**
    /// undo entry. Returns the new clip's id.
    pub fn add_text_clip(
        &mut self,
        timeline_in: f64,
        duration: f64,
        text: TextSpec,
        transform: Transform,
        track: Option<TrackId>,
    ) -> Result<ClipId, EngineError> {
        if !timeline_in.is_finite() || !duration.is_finite() || duration < MIN_CLIP_DURATION {
            return Err(EngineError::InvalidTimeRange {
                clip: ClipId(0),
                timeline_in,
                timeline_out: timeline_in + duration,
            });
        }
        let timeline_in = timeline_in.max(0.0);
        let timeline_out = timeline_in + duration;

        let span_free = |t: &Track| {
            t.clips
                .iter()
                .all(|c| c.timeline_out <= timeline_in + EPS || c.timeline_in >= timeline_out - EPS)
        };
        let target = match track {
            Some(id) => {
                let t = self
                    .project
                    .track(id)
                    .ok_or(EngineError::UnknownTrack(id))?;
                if t.kind != TrackKind::Text {
                    return Err(EngineError::InvalidText {
                        clip: ClipId(0),
                        reason: "target is not a text track",
                    });
                }
                Self::require_track_unlocked(t)?;
                Some(id)
            }
            None => self
                .project
                .tracks
                .iter()
                .find(|t| t.kind == TrackKind::Text && !t.locked && span_free(t))
                .map(|t| t.id),
        };

        let clip_id = ClipId(self.next_id());
        let clip = Clip {
            id: clip_id,
            media_id: None,
            timeline_in,
            timeline_out,
            source_in: 0.0,
            source_out: duration,
            transform,
            opacity: 1.0,
            blend_mode: BlendMode::default(),
            speed: 1.0,
            volume: 1.0,
            transition_out: None,
            text: Some(text),
            keyframes: Keyframes::default(),
        };

        match target {
            Some(track_id) => self.execute(Box::new(AddClip { track_id, clip }))?,
            None => {
                // No text lane has room: create one at the top, as one
                // atomic, undoable unit with the clip placement.
                let track_id = TrackId(self.next_id());
                let track = Track::new(
                    track_id,
                    TrackKind::Text,
                    self.next_track_name(TrackKind::Text),
                );
                self.execute(Box::new(Compound {
                    name: "AddTextClip",
                    parts: vec![
                        Box::new(AddTrack { index: 0, track }),
                        Box::new(AddClip { track_id, clip }),
                    ],
                }))?;
            }
        }
        Ok(clip_id)
    }

    /// Replace a text clip's payload (content and/or style). Equal
    /// payloads are a no-op (no undo entry) so UI echoes don't pollute
    /// the stack.
    pub fn set_clip_text(&mut self, clip_id: ClipId, text: TextSpec) -> Result<(), EngineError> {
        self.require_clip_unlocked(clip_id)?;
        let (track, clip) = self
            .project
            .find_clip(clip_id)
            .ok_or(EngineError::UnknownClip(clip_id))?;
        let old = clip.text.clone().ok_or(EngineError::InvalidText {
            clip: clip_id,
            reason: "not a text clip",
        })?;
        if old == text {
            return Ok(());
        }
        self.execute(Box::new(SetClipText {
            track_id: track.id,
            clip_id,
            old,
            new: text,
        }))
    }

    /// Move a clip to a new timeline position (clamped to `>= 0`); duration
    /// and source range are unchanged. Fails on overlap.
    pub fn move_clip(&mut self, clip_id: ClipId, timeline_in: f64) -> Result<(), EngineError> {
        self.require_clip_unlocked(clip_id)?;
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
        self.execute_structural(Box::new(MoveClip {
            track_id: track.id,
            clip_id,
            old_timeline_in: clip.timeline_in,
            old_timeline_out: clip.timeline_out,
            new_timeline_in: new_in,
            new_timeline_out: new_in + clip.duration(),
        }))
    }

    /// Move a clip onto another track (and to a new timeline position in
    /// the same step — a vertical drag moves on both axes). Duration and
    /// source range are unchanged. Fails on overlap, on a kind-incompatible
    /// target (video clip on an audio track), and when either track is
    /// locked. Same-track calls degrade to a plain move.
    pub fn move_clip_to_track(
        &mut self,
        clip_id: ClipId,
        track_id: TrackId,
        timeline_in: f64,
    ) -> Result<(), EngineError> {
        let (source, clip) = self
            .project
            .find_clip(clip_id)
            .ok_or(EngineError::UnknownClip(clip_id))?;
        if source.id == track_id {
            return self.move_clip(clip_id, timeline_in);
        }
        Self::require_track_unlocked(source)?;
        let target = self
            .project
            .track(track_id)
            .ok_or(EngineError::UnknownTrack(track_id))?;
        Self::require_track_unlocked(target)?;
        if !timeline_in.is_finite() {
            return Err(EngineError::InvalidTimeRange {
                clip: clip_id,
                timeline_in,
                timeline_out: timeline_in,
            });
        }
        let new_in = timeline_in.max(0.0);
        self.execute_structural(Box::new(MoveClipToTrack {
            clip_id,
            old_track: source.id,
            new_track: track_id,
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
    /// overlap a neighboring clip. Clips without a bounding medium — text
    /// clips, stills, and looping GIFs — trim freely in both directions
    /// (their source range just re-normalizes to the new duration).
    pub fn trim_clip(
        &mut self,
        clip_id: ClipId,
        edge: TrimEdge,
        to: f64,
    ) -> Result<f64, EngineError> {
        self.require_clip_unlocked(clip_id)?;
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
        // How the medium bounds the drag. `Bounded` clamps to source
        // handles; loops and stills extend freely but differ in what the
        // source range means afterwards (loop phase vs nothing).
        enum SourceBound {
            /// Video/audio: the media duration bounds the range.
            Bounded(f64),
            /// GIF: unbounded, but source values carry loop phase —
            /// folded into `[0, period)` rather than renormalized.
            Loop(f64),
            /// Text and stills: unbounded, source range is derived.
            Free,
        }
        let bound = match clip.media_id {
            Some(id) => {
                let media = self
                    .project
                    .media(id)
                    .ok_or(EngineError::UnknownMedia(id))?;
                match media.kind {
                    MediaKind::Video | MediaKind::Audio => SourceBound::Bounded(media.duration),
                    MediaKind::Gif => SourceBound::Loop(media.duration),
                    MediaKind::Image => SourceBound::Free,
                }
            }
            None => SourceBound::Free,
        };
        let old = ClipSpan::of(clip);
        let mut new = old;

        let applied = match edge {
            TrimEdge::Start => {
                // Media headroom to the left, and timeline 0, bound the drag.
                let lo = match bound {
                    SourceBound::Bounded(_) => {
                        (clip.timeline_in - clip.source_in / clip.speed).max(0.0)
                    }
                    _ => 0.0,
                };
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
                match bound {
                    SourceBound::Bounded(_) => {
                        new.source_in =
                            (clip.source_in + (t - clip.timeline_in) * clip.speed).max(0.0);
                    }
                    // A loop's start edge shifts its phase with the drag.
                    SourceBound::Loop(_) => {
                        new.source_in = clip.source_in + (t - clip.timeline_in) * clip.speed;
                    }
                    SourceBound::Free => {}
                }
                t
            }
            TrimEdge::End => {
                let lo = clip.timeline_in + MIN_CLIP_DURATION;
                let hi = match bound {
                    SourceBound::Bounded(d) => {
                        clip.timeline_out + (d - clip.source_out) / clip.speed
                    }
                    _ => f64::INFINITY,
                };
                if lo > hi {
                    return Err(EngineError::InvalidTimeRange {
                        clip: clip_id,
                        timeline_in: lo,
                        timeline_out: hi,
                    });
                }
                let t = to.clamp(lo, hi);
                new.timeline_out = t;
                match bound {
                    SourceBound::Bounded(d) => {
                        new.source_out =
                            (clip.source_out + (t - clip.timeline_out) * clip.speed).min(d);
                    }
                    // A loop's end edge just extends/shortens the window;
                    // phase (source_in) is untouched.
                    SourceBound::Loop(_) => {
                        new.source_out = clip.source_out + (t - clip.timeline_out) * clip.speed;
                    }
                    SourceBound::Free => {}
                }
                t
            }
        };
        match bound {
            SourceBound::Free => {
                // No medium (text) or no intrinsic time (still): keep the
                // source range normalized to [0, duration).
                new.source_in = 0.0;
                new.source_out = new.timeline_out - new.timeline_in;
            }
            SourceBound::Loop(period) => {
                // Fold the pair into source_in ∈ [0, period) — renderers
                // fold source time the same way, so playback is
                // identical, and the model stays non-negative. Shifting
                // both ends together preserves the span linkage.
                let shift = new.source_in.rem_euclid(period.max(MIN_CLIP_DURATION)) - new.source_in;
                new.source_in += shift;
                new.source_out += shift;
            }
            SourceBound::Bounded(_) => {}
        }

        let trim = Box::new(TrimClip {
            track_id: track.id,
            clip_id,
            old,
            new,
        });
        // Keyframes keep their absolute timeline positions through a
        // trim: the lane re-anchors to the new window (with boundary
        // keyframes where the window cuts a moving envelope), riding the
        // same undo entry as the trim itself.
        let windowed = keyframes::window_lanes(
            &clip.keyframes,
            new.timeline_in - old.timeline_in,
            new.timeline_out - old.timeline_in,
        );
        if windowed == clip.keyframes {
            self.execute_structural(trim)?;
        } else {
            let mut parts: Vec<Box<dyn Command>> = vec![trim];
            for (prop, old_lane) in clip.keyframes.clone() {
                let new_lane = windowed.get(&prop).cloned().unwrap_or_default();
                if new_lane != old_lane {
                    parts.push(Box::new(SetKeyframes {
                        track_id: track.id,
                        clip_id,
                        prop,
                        old: old_lane,
                        new: new_lane,
                    }));
                }
            }
            self.execute_structural(Box::new(Compound {
                name: "TrimClip",
                parts,
            }))?;
        }
        Ok(applied)
    }

    /// Split a clip at timeline time `at` into two clips sharing the same
    /// media (or, for text clips, duplicating the full text payload —
    /// each half is an independent complete text). The left half keeps
    /// the original id; the new right half's id is returned. `at` must be
    /// strictly inside the clip (at least [`MIN_CLIP_DURATION`] from each
    /// edge) — splitting at an exact clip edge is rejected.
    pub fn split_clip(&mut self, clip_id: ClipId, at: f64) -> Result<ClipId, EngineError> {
        self.require_clip_unlocked(clip_id)?;
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

        let offset = at - original.timeline_in;

        let mut left = original.clone();
        left.timeline_out = at;
        left.source_out = source_at;
        // The out cut (and any transition bound to it) belongs to the
        // right half; the left half's new cut at the split point has none.
        left.transition_out = None;
        // Each half keeps the window of the automation it covers, with
        // boundary keyframes holding the envelope value at the cut — the
        // split is inaudible (see `keyframes::window_lane`).
        left.keyframes = keyframes::window_lanes(&original.keyframes, 0.0, offset);

        let mut right = original.clone();
        right.id = ClipId(self.next_id());
        right.timeline_in = at;
        right.source_in = source_at;
        right.keyframes = keyframes::window_lanes(&original.keyframes, offset, original.duration());
        if original.text.is_some() {
            // No source medium: each half re-normalizes to [0, duration)
            // and keeps the whole (duplicated) text payload.
            left.source_in = 0.0;
            left.source_out = left.timeline_out - left.timeline_in;
            right.source_in = 0.0;
            right.source_out = right.timeline_out - right.timeline_in;
        }
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
        self.require_clip_unlocked(clip_id)?;
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

    // ------------------------------------------------------------------
    // Keyframes (volume automation now; Phase 3 reuses the machinery)
    // ------------------------------------------------------------------

    /// The keyframe in `prop`'s lane at (clip-relative) time `t`, within
    /// the UI-reference tolerance.
    fn keyframe_at(clip: &Clip, prop: KeyframeProp, t: f64) -> Result<Keyframe, EngineError> {
        clip.keyframes
            .get(&prop)
            .and_then(|lane| {
                lane.iter()
                    .find(|k| (k.t - t).abs() <= KEYFRAME_MATCH_EPS)
                    .copied()
            })
            .ok_or(EngineError::UnknownKeyframe { clip: clip.id, t })
    }

    /// Add a keyframe at clip-relative time `t` (clamped to the clip).
    /// Landing on an existing keyframe (within [`KEYFRAME_MIN_DT`])
    /// **replaces** it in place — same-time duplicates cannot exist.
    /// Returns the time the keyframe actually landed on. Undoable.
    pub fn add_keyframe(
        &mut self,
        clip_id: ClipId,
        prop: KeyframeProp,
        t: f64,
        value: f64,
        easing: Easing,
    ) -> Result<f64, EngineError> {
        self.require_clip_unlocked(clip_id)?;
        let (track, clip) = self
            .project
            .find_clip(clip_id)
            .ok_or(EngineError::UnknownClip(clip_id))?;
        if !t.is_finite() {
            return Err(EngineError::InvalidProperty {
                clip: clip_id,
                property: "keyframe.t",
                value: t,
            });
        }
        if !prop.valid_value(value) {
            return Err(EngineError::InvalidProperty {
                clip: clip_id,
                property: prop.name(),
                value,
            });
        }
        let t = t.clamp(0.0, clip.duration());
        let track_id = track.id;
        // Dedup at the same time: replace the existing keyframe.
        if let Some(existing) = clip
            .keyframes
            .get(&prop)
            .and_then(|lane| lane.iter().find(|k| (k.t - t).abs() < KEYFRAME_MIN_DT))
        {
            let applied = existing.t;
            let new = Keyframe {
                t: applied,
                value,
                easing,
            };
            if *existing != new {
                let old = *existing;
                self.execute(Box::new(MoveKeyframe {
                    track_id,
                    clip_id,
                    prop,
                    old,
                    new,
                }))?;
            }
            return Ok(applied);
        }
        self.execute(Box::new(AddKeyframe {
            track_id,
            clip_id,
            prop,
            keyframe: Keyframe { t, value, easing },
        }))?;
        Ok(t)
    }

    /// Move the keyframe at `from_t` to `to_t` with a new value (a dot
    /// drag moves on both axes; easing is kept). `to_t` is clamped
    /// between the neighboring keyframes and the clip bounds — keyframes
    /// cannot cross. Returns the applied time. Undoable.
    pub fn move_keyframe(
        &mut self,
        clip_id: ClipId,
        prop: KeyframeProp,
        from_t: f64,
        to_t: f64,
        value: f64,
    ) -> Result<f64, EngineError> {
        self.require_clip_unlocked(clip_id)?;
        let (track, clip) = self
            .project
            .find_clip(clip_id)
            .ok_or(EngineError::UnknownClip(clip_id))?;
        if !from_t.is_finite() || !to_t.is_finite() {
            return Err(EngineError::InvalidProperty {
                clip: clip_id,
                property: "keyframe.t",
                value: to_t,
            });
        }
        if !prop.valid_value(value) {
            return Err(EngineError::InvalidProperty {
                clip: clip_id,
                property: prop.name(),
                value,
            });
        }
        let old = Self::keyframe_at(clip, prop, from_t)?;
        let lane = &clip.keyframes[&prop];
        let idx = lane
            .iter()
            .position(|k| k.t == old.t)
            .expect("keyframe_at found it");
        let lo = if idx > 0 {
            lane[idx - 1].t + KEYFRAME_MIN_DT
        } else {
            0.0
        };
        let hi = if idx + 1 < lane.len() {
            lane[idx + 1].t - KEYFRAME_MIN_DT
        } else {
            clip.duration()
        };
        let t = to_t.clamp(lo, hi.max(lo));
        let new = Keyframe {
            t,
            value,
            easing: old.easing,
        };
        if new == old {
            return Ok(t);
        }
        self.execute(Box::new(MoveKeyframe {
            track_id: track.id,
            clip_id,
            prop,
            old,
            new,
        }))?;
        Ok(t)
    }

    /// Remove the keyframe at clip-relative time `t`. Undoable.
    pub fn remove_keyframe(
        &mut self,
        clip_id: ClipId,
        prop: KeyframeProp,
        t: f64,
    ) -> Result<(), EngineError> {
        self.require_clip_unlocked(clip_id)?;
        let (track, clip) = self
            .project
            .find_clip(clip_id)
            .ok_or(EngineError::UnknownClip(clip_id))?;
        let keyframe = Self::keyframe_at(clip, prop, t)?;
        self.execute(Box::new(RemoveKeyframe {
            track_id: track.id,
            clip_id,
            prop,
            keyframe,
        }))
    }

    /// Set a clip's fade-in or fade-out to `duration` seconds (0 removes
    /// it) — pure sugar over the volume keyframe lane: a fade is a
    /// silent keyframe at the clip edge ramping to the envelope's value
    /// (see [`keyframes::fade_in_duration`]). The duration is clamped to
    /// the clip and against the opposite fade; the applied duration is
    /// returned. One undoable command.
    pub fn set_clip_fade(
        &mut self,
        clip_id: ClipId,
        side: FadeSide,
        duration: f64,
    ) -> Result<f64, EngineError> {
        self.require_clip_unlocked(clip_id)?;
        let (track, clip) = self
            .project
            .find_clip(clip_id)
            .ok_or(EngineError::UnknownClip(clip_id))?;
        if !duration.is_finite() || duration < 0.0 {
            return Err(EngineError::InvalidProperty {
                clip: clip_id,
                property: "fade.duration",
                value: duration,
            });
        }
        if clip.text.is_some() {
            return Err(EngineError::InvalidKeyframes {
                clip: clip_id,
                reason: "volume keyframes on a text clip",
            });
        }
        let old = clip
            .keyframes
            .get(&KeyframeProp::Volume)
            .cloned()
            .unwrap_or_default();
        let (new, applied) = keyframes::with_fade(&old, clip.duration(), side, duration);
        if new != old {
            self.execute(Box::new(SetKeyframes {
                track_id: track.id,
                clip_id,
                prop: KeyframeProp::Volume,
                old,
                new,
            }))?;
        }
        Ok(applied)
    }

    /// Extract a video clip's audio onto an audio track: a new audio
    /// clip referencing the **same media and source range** (volume and
    /// volume automation carried over) is placed at the same timeline
    /// position, and the video clip's own volume drops to 0. The
    /// topmost unlocked audio track with room takes the clip; when none
    /// has room a new audio lane is created at the bottom of the panel.
    /// One undoable command; returns the new audio clip's id.
    ///
    /// The clips stay independent afterwards (no linked editing yet —
    /// moving or trimming one does not follow the other).
    pub fn extract_audio(&mut self, clip_id: ClipId) -> Result<ClipId, EngineError> {
        self.require_clip_unlocked(clip_id)?;
        let (track, clip) = self
            .project
            .find_clip(clip_id)
            .ok_or(EngineError::UnknownClip(clip_id))?;
        if track.kind != TrackKind::Video {
            return Err(EngineError::ExtractAudio {
                clip: clip_id,
                reason: "only video clips carry embedded audio",
            });
        }
        let media_id = clip.media_id.ok_or(EngineError::ExtractAudio {
            clip: clip_id,
            reason: "clip has no media",
        })?;
        let media = self
            .project
            .media(media_id)
            .ok_or(EngineError::UnknownMedia(media_id))?;
        if !media.has_audio {
            return Err(EngineError::ExtractAudio {
                clip: clip_id,
                reason: "the source file has no audio stream",
            });
        }
        let video_track_id = track.id;
        let clip = clip.clone();

        let span_free = |t: &Track| {
            t.clips.iter().all(|c| {
                c.timeline_out <= clip.timeline_in + EPS || c.timeline_in >= clip.timeline_out - EPS
            })
        };
        let target = self
            .project
            .tracks
            .iter()
            .find(|t| t.kind == TrackKind::Audio && !t.locked && span_free(t))
            .map(|t| t.id);

        // The audio clip takes over the sound: same window, same gain,
        // same automation. Only the volume lane carries over — other
        // (Phase 3) lanes animate the picture and stay with the video.
        let mut audio_keyframes = Keyframes::default();
        if let Some(lane) = clip.keyframes.get(&KeyframeProp::Volume) {
            audio_keyframes.insert(KeyframeProp::Volume, lane.clone());
        }
        let audio_clip = Clip {
            id: ClipId(self.next_id()),
            media_id: Some(media_id),
            timeline_in: clip.timeline_in,
            timeline_out: clip.timeline_out,
            source_in: clip.source_in,
            source_out: clip.source_out,
            transform: Transform::default(),
            opacity: 1.0,
            blend_mode: BlendMode::default(),
            speed: clip.speed,
            volume: clip.volume,
            transition_out: None,
            text: None,
            keyframes: audio_keyframes,
        };
        let audio_clip_id = audio_clip.id;

        let mut parts: Vec<Box<dyn Command>> = Vec::new();
        let target = match target {
            Some(id) => id,
            None => {
                let id = TrackId(self.next_id());
                parts.push(Box::new(AddTrack {
                    index: self.project.tracks.len(),
                    track: Track::new(id, TrackKind::Audio, self.next_track_name(TrackKind::Audio)),
                }));
                id
            }
        };
        parts.push(Box::new(AddClip {
            track_id: target,
            clip: audio_clip,
        }));
        parts.push(Box::new(SetClipVolume {
            track_id: video_track_id,
            clip_id,
            old: clip.volume,
            new: 0.0,
        }));
        self.execute(Box::new(Compound {
            name: "ExtractAudio",
            parts,
        }))?;
        Ok(audio_clip_id)
    }

    /// Set a clip's 2D placement (position/scale/rotation, static values —
    /// keyframes are Phase 3). The compositor consumes this identically in
    /// preview and export.
    pub fn set_clip_transform(
        &mut self,
        clip_id: ClipId,
        transform: Transform,
    ) -> Result<(), EngineError> {
        self.require_clip_unlocked(clip_id)?;
        let (track, clip) = self
            .project
            .find_clip(clip_id)
            .ok_or(EngineError::UnknownClip(clip_id))?;
        for (property, value) in [
            ("transform.x", transform.x),
            ("transform.y", transform.y),
            ("transform.rotation", transform.rotation),
        ] {
            if !value.is_finite() {
                return Err(EngineError::InvalidProperty {
                    clip: clip_id,
                    property,
                    value,
                });
            }
        }
        if !transform.scale.is_finite() || transform.scale <= 0.0 {
            return Err(EngineError::InvalidProperty {
                clip: clip_id,
                property: "transform.scale",
                value: transform.scale,
            });
        }
        self.execute(Box::new(SetClipTransform {
            track_id: track.id,
            clip_id,
            old: clip.transform.clone(),
            new: transform,
        }))
    }

    /// Set a clip's opacity (0.0 = transparent, 1.0 = opaque).
    pub fn set_clip_opacity(&mut self, clip_id: ClipId, opacity: f64) -> Result<(), EngineError> {
        self.require_clip_unlocked(clip_id)?;
        let (track, clip) = self
            .project
            .find_clip(clip_id)
            .ok_or(EngineError::UnknownClip(clip_id))?;
        if !opacity.is_finite() || !(0.0..=1.0).contains(&opacity) {
            return Err(EngineError::InvalidProperty {
                clip: clip_id,
                property: "opacity",
                value: opacity,
            });
        }
        self.execute(Box::new(SetClipOpacity {
            track_id: track.id,
            clip_id,
            old: clip.opacity,
            new: opacity,
        }))
    }

    /// Set how a clip blends with the layers below it.
    pub fn set_clip_blend_mode(
        &mut self,
        clip_id: ClipId,
        mode: BlendMode,
    ) -> Result<(), EngineError> {
        self.require_clip_unlocked(clip_id)?;
        let (track, clip) = self
            .project
            .find_clip(clip_id)
            .ok_or(EngineError::UnknownClip(clip_id))?;
        self.execute(Box::new(SetClipBlendMode {
            track_id: track.id,
            clip_id,
            old: clip.blend_mode,
            new: mode,
        }))
    }

    /// Set, replace, or remove the transition at a clip's out cut.
    ///
    /// Requires a video-track clip with a touching next clip (the cut the
    /// transition binds to). The requested duration is clamped to
    /// [`MIN_TRANSITION_DURATION`]..[`MAX_TRANSITION_DURATION`] and to what
    /// the cut currently supports ([`transition_duration_limit`]); the
    /// stored (clamped) duration is returned. `None` removes.
    pub fn set_transition(
        &mut self,
        clip_id: ClipId,
        transition: Option<Transition>,
    ) -> Result<Option<f64>, EngineError> {
        self.require_clip_unlocked(clip_id)?;
        let (track, clip) = self
            .project
            .find_clip(clip_id)
            .ok_or(EngineError::UnknownClip(clip_id))?;

        let new = match transition {
            None => None,
            Some(t) => {
                if track.kind != TrackKind::Video {
                    return Err(EngineError::InvalidTransition {
                        clip: clip_id,
                        reason: "transitions only apply to video clips",
                    });
                }
                if t.kind.is_empty() {
                    return Err(EngineError::InvalidTransition {
                        clip: clip_id,
                        reason: "empty transition kind",
                    });
                }
                if !t.duration.is_finite() || t.duration <= 0.0 {
                    return Err(EngineError::InvalidProperty {
                        clip: clip_id,
                        property: "transition.duration",
                        value: t.duration,
                    });
                }
                let index = track
                    .clips
                    .iter()
                    .position(|c| c.id == clip_id)
                    .expect("clip found on track");
                let next = track
                    .clips
                    .get(index + 1)
                    .filter(|next| clips_touch(clip, next))
                    .ok_or(EngineError::InvalidTransition {
                        clip: clip_id,
                        reason: "no adjacent clip after the cut",
                    })?;
                let limit = transition_duration_limit(&self.project, clip, next)
                    .min(MAX_TRANSITION_DURATION);
                // Floor at the standard minimum unless the cut itself
                // supports less (two very short clips).
                let duration = t
                    .duration
                    .min(limit)
                    .max(MIN_TRANSITION_DURATION.min(limit));
                Some(Transition {
                    kind: t.kind,
                    duration,
                })
            }
        };

        let applied = new.as_ref().map(|t| t.duration);
        let old = clip.transition_out.clone();
        if old == new {
            return Ok(applied);
        }
        self.execute(Box::new(SetTransition {
            track_id: track.id,
            clip_id,
            old,
            new,
        }))?;
        Ok(applied)
    }

    /// Remove a clip from its track, leaving a gap.
    pub fn delete_clip(&mut self, clip_id: ClipId) -> Result<(), EngineError> {
        self.require_clip_unlocked(clip_id)?;
        let (track, clip) = self
            .project
            .find_clip(clip_id)
            .ok_or(EngineError::UnknownClip(clip_id))?;
        self.execute_structural(Box::new(DeleteClip {
            track_id: track.id,
            clip: clip.clone(),
        }))
    }

    /// Remove a clip and shift every later clip on the same track left by
    /// the removed clip's duration (gaps between later clips are
    /// preserved).
    pub fn ripple_delete(&mut self, clip_id: ClipId) -> Result<(), EngineError> {
        self.require_clip_unlocked(clip_id)?;
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
        self.execute_structural(Box::new(RippleDelete {
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
