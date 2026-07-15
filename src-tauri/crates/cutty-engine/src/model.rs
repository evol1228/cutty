//! Project data model: media references, tracks, clips, and the invariants
//! that hold across every mutation.
//!
//! All times are **seconds** as `f64`, matching the probe/proxy layer in
//! `cutty-media`. Frame quantization is a presentation concern, not a model
//! concern.

use serde::{Deserialize, Serialize};

use crate::error::EngineError;

/// Comparison tolerance for timeline math. Touching clip edges (`a.out ==
/// b.in`) must not read as an overlap after f64 arithmetic.
pub const EPS: f64 = 1e-9;

/// Minimum clip duration in seconds. Trims and splits clamp/reject against
/// this so no operation can produce a (near-)zero-length clip.
pub const MIN_CLIP_DURATION: f64 = 1e-3;

/// Two clips "touch" (share a cut) when their facing edges are within this
/// tolerance, seconds. Well under one frame at any real rate, comfortably
/// above f64 dust. Transitions bind to touching edges; every adjacency
/// check (validation, pruning, span resolution) uses this one definition.
pub const TOUCH_EPS: f64 = 1e-6;

/// Shortest transition the engine stores, seconds (matching the CapCut
/// floor — anything shorter reads as a hard cut).
pub const MIN_TRANSITION_DURATION: f64 = 0.1;

/// Longest transition the engine stores, seconds.
pub const MAX_TRANSITION_DURATION: f64 = 5.0;

/// Whether `a`'s out edge and `b`'s in edge share a cut (see [`TOUCH_EPS`]).
pub fn clips_touch(a: &Clip, b: &Clip) -> bool {
    (b.timeline_in - a.timeline_out).abs() <= TOUCH_EPS
}

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub u64);
    };
}

id_type!(
    /// Identifier of an imported media file.
    MediaId
);
id_type!(
    /// Identifier of a track.
    TrackId
);
id_type!(
    /// Identifier of a clip.
    ClipId
);

/// Project-level render settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectSettings {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
}

impl Default for ProjectSettings {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 30.0,
        }
    }
}

/// A media file registered in the project's media pool.
///
/// Only what the timeline model needs: duration for source-range clamping
/// and stream presence for track-kind compatibility. Probe details
/// (resolution, codecs) stay in `cutty-media`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaRef {
    pub id: MediaId,
    /// Absolute path of the original file.
    pub path: String,
    /// Duration in seconds.
    pub duration: f64,
    pub has_video: bool,
    pub has_audio: bool,
}

/// Kind of a track. Phase 1 is exactly one `Video` + one `Audio` track;
/// `Text` tracks arrive in Phase 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TrackKind {
    Video,
    Audio,
}

/// 2D placement of a clip in the frame.
///
/// Semantics (shared by preview and export — the compositor consumes
/// these directly):
/// - The clip's base placement is its source frame fit inside the project
///   canvas (aspect-preserving "contain"), centered.
/// - `x`/`y` offset the clip center from the canvas center, in **project
///   pixels** (`ProjectSettings::width`/`height`), +x right, +y down.
/// - `scale` multiplies the fitted base size (1.0 = fit).
/// - `rotation` is degrees, clockwise positive.
///
/// Rendering at a different resolution (720p preview, 4K export) scales
/// the whole coordinate space uniformly, so the composition is identical
/// at every output size.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transform {
    pub x: f64,
    pub y: f64,
    pub scale: f64,
    pub rotation: f64,
}

impl Default for Transform {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            scale: 1.0,
            rotation: 0.0,
        }
    }
}

impl Transform {
    /// Whether this is exactly the identity placement (fit, centered,
    /// unrotated) — the fast-path export check relies on exact equality.
    pub fn is_identity(&self) -> bool {
        *self == Self::default()
    }
}

/// How a video clip blends with the layers below it.
///
/// Formulas follow the W3C compositing spec's separable modes, applied
/// per channel on sRGB-encoded values (see `cutty-gpu`).
///
/// **Schema rule:** additive clip/track *fields* use `#[serde(default)]`
/// and need no project-version bump (older files simply lack the key).
/// Adding new *enum variants* here (or a new `TrackKind`) is different:
/// old builds cannot read files that use them, so that bumps
/// `project_file::CURRENT_VERSION` with a migration arm.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BlendMode {
    #[default]
    Normal,
    Multiply,
    Screen,
    Overlay,
    Add,
}

/// A transition bound to the cut at its owning clip's out edge.
///
/// The span is `duration` seconds **centered on the cut**; the effective
/// span is derived at resolve time ([`crate::resolve::transition_spans`]),
/// clamped to both clips' durations and to available source handles.
/// `kind` is a transition id from the GPU shader registry (`cutty-gpu`);
/// unknown ids render as a crossfade rather than failing, so project
/// files stay forward-compatible.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transition {
    pub kind: String,
    /// Requested duration, seconds. Validated finite and positive.
    pub duration: f64,
}

/// A clip: a window into a media file, placed on the timeline.
///
/// Invariants (enforced by [`Project::validate`]):
/// - `timeline_in < timeline_out`, both finite and `>= 0`
/// - `0 <= source_in < source_out <= media.duration`
/// - `(timeline_out - timeline_in) * speed == source_out - source_in`
///   (within [`EPS`]) — `speed` is modeled but fixed at `1.0` in Phase 1
/// - no overlap with other clips on the same track (touching edges are fine)
/// - `transition_out` only on video tracks, only with a touching next clip
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Clip {
    pub id: ClipId,
    pub media_id: MediaId,
    /// Start position on the timeline, seconds (inclusive).
    pub timeline_in: f64,
    /// End position on the timeline, seconds (exclusive).
    pub timeline_out: f64,
    /// Start of the used range within the media, seconds.
    pub source_in: f64,
    /// End of the used range within the media, seconds.
    pub source_out: f64,
    pub transform: Transform,
    /// 0.0..=1.0
    pub opacity: f64,
    /// Blend mode against the layers below. Additive schema field
    /// (`serde(default)`): projects saved before Phase 2 load as `Normal`.
    #[serde(default)]
    pub blend_mode: BlendMode,
    /// Playback rate. Modeled now, fixed at 1.0 until Phase 3.
    pub speed: f64,
    /// Linear gain, `>= 0.0` (1.0 = unity).
    pub volume: f64,
    /// Transition into the next clip on the same track, spanning the cut
    /// at `timeline_out`. Additive schema field (`serde(default)`):
    /// pre-transition projects load as `None`, and files without
    /// transitions serialize byte-identically to before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transition_out: Option<Transition>,
}

impl Clip {
    /// Timeline duration in seconds.
    pub fn duration(&self) -> f64 {
        self.timeline_out - self.timeline_in
    }
}

/// A horizontal lane of non-overlapping clips, kept sorted by
/// `timeline_in`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub id: TrackId,
    pub kind: TrackKind,
    pub name: String,
    /// Rejects edits: every clip mutation targeting this track fails with
    /// [`EngineError::TrackLocked`]. Enforced at the engine's public
    /// operations, not in `Command::apply` — undo/redo must be able to
    /// restore state on a locked track.
    pub locked: bool,
    /// Audio silenced. Applies to audio tracks and to the embedded audio
    /// of clips on video tracks; it never affects the picture.
    pub muted: bool,
    /// Excluded from the video composite (preview and export take the
    /// same `resolve_video_layers` path, so both respect it). Meaningful
    /// on video tracks only. Additive schema field: pre-Phase 2 files
    /// load as `false`.
    #[serde(default)]
    pub hidden: bool,
    pub clips: Vec<Clip>,
}

impl Track {
    /// An empty, unlocked, audible, visible track.
    pub fn new(id: TrackId, kind: TrackKind, name: impl Into<String>) -> Self {
        Self {
            id,
            kind,
            name: name.into(),
            locked: false,
            muted: false,
            hidden: false,
            clips: Vec::new(),
        }
    }

    /// Look up a clip by id.
    pub fn clip(&self, id: ClipId) -> Option<&Clip> {
        self.clips.iter().find(|c| c.id == id)
    }

    pub(crate) fn clip_mut(&mut self, id: ClipId) -> Option<&mut Clip> {
        self.clips.iter_mut().find(|c| c.id == id)
    }

    /// Restore the sorted-by-`timeline_in` ordering after a mutation.
    pub(crate) fn sort_clips(&mut self) {
        self.clips
            .sort_by(|a, b| a.timeline_in.total_cmp(&b.timeline_in));
    }
}

/// The whole editable state of a Cutty project. Owned exclusively by the
/// Rust engine; the frontend only ever sees serialized snapshots of it.
///
/// `tracks` is stored in **visual order**: index 0 renders at the top of
/// the timeline panel. Video compositing stacks the other way — the
/// *last* video track in the vec is the base layer and earlier video
/// tracks paint over it (see [`crate::resolve::resolve_video_layers`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub settings: ProjectSettings,
    pub media: Vec<MediaRef>,
    pub tracks: Vec<Track>,
}

impl Project {
    /// An empty project with the Phase 1 track layout: one video track and
    /// one audio track. The caller supplies the track ids so that all id
    /// allocation stays in one place (the engine).
    pub fn new(settings: ProjectSettings, video_track: TrackId, audio_track: TrackId) -> Self {
        Self {
            settings,
            media: Vec::new(),
            tracks: vec![
                Track::new(video_track, TrackKind::Video, "V1"),
                Track::new(audio_track, TrackKind::Audio, "A1"),
            ],
        }
    }

    /// Look up a media reference by id.
    pub fn media(&self, id: MediaId) -> Option<&MediaRef> {
        self.media.iter().find(|m| m.id == id)
    }

    /// Look up a track by id.
    pub fn track(&self, id: TrackId) -> Option<&Track> {
        self.tracks.iter().find(|t| t.id == id)
    }

    pub(crate) fn track_mut(&mut self, id: TrackId) -> Option<&mut Track> {
        self.tracks.iter_mut().find(|t| t.id == id)
    }

    /// Find the track holding a clip, together with the clip.
    pub fn find_clip(&self, id: ClipId) -> Option<(&Track, &Clip)> {
        self.tracks.iter().find_map(|t| t.clip(id).map(|c| (t, c)))
    }

    /// The largest id used anywhere in the project. Used to seed the
    /// engine's id counter when loading a saved project.
    pub fn max_id(&self) -> u64 {
        let media = self.media.iter().map(|m| m.id.0);
        let tracks = self.tracks.iter().map(|t| t.id.0);
        let clips = self
            .tracks
            .iter()
            .flat_map(|t| t.clips.iter().map(|c| c.id.0));
        media.chain(tracks).chain(clips).max().unwrap_or(0)
    }

    /// Check every model invariant. Called by the engine after each command
    /// application; a violation aborts the command and leaves the previous
    /// state in place.
    pub fn validate(&self) -> Result<(), EngineError> {
        self.validate_unique_ids()?;
        for track in &self.tracks {
            for clip in &track.clips {
                self.validate_clip(track, clip)?;
            }
            // Clips are sorted, so overlap is a neighbor-only check.
            for pair in track.clips.windows(2) {
                if pair[1].timeline_in < pair[0].timeline_out - EPS {
                    return Err(EngineError::ClipOverlap {
                        track: track.id,
                        a: pair[0].id,
                        b: pair[1].id,
                    });
                }
            }
            // Transitions bind to a live cut: video track, touching next
            // clip, sane duration. Structural commands prune what a
            // mutation would leave dangling, so this only trips on bugs
            // and on hand-edited project files.
            for (i, clip) in track.clips.iter().enumerate() {
                let Some(transition) = &clip.transition_out else {
                    continue;
                };
                if track.kind != TrackKind::Video {
                    return Err(EngineError::InvalidTransition {
                        clip: clip.id,
                        reason: "transitions only apply to video clips",
                    });
                }
                if transition.kind.is_empty() {
                    return Err(EngineError::InvalidTransition {
                        clip: clip.id,
                        reason: "empty transition kind",
                    });
                }
                if !transition.duration.is_finite() || transition.duration <= 0.0 {
                    return Err(EngineError::InvalidProperty {
                        clip: clip.id,
                        property: "transition.duration",
                        value: transition.duration,
                    });
                }
                let touching_next = track
                    .clips
                    .get(i + 1)
                    .is_some_and(|next| clips_touch(clip, next));
                if !touching_next {
                    return Err(EngineError::InvalidTransition {
                        clip: clip.id,
                        reason: "no adjacent clip after the cut",
                    });
                }
            }
        }
        Ok(())
    }

    fn validate_clip(&self, track: &Track, clip: &Clip) -> Result<(), EngineError> {
        let times = [
            clip.timeline_in,
            clip.timeline_out,
            clip.source_in,
            clip.source_out,
        ];
        if times.iter().any(|t| !t.is_finite())
            || clip.timeline_in < 0.0
            || clip.timeline_out <= clip.timeline_in
        {
            return Err(EngineError::InvalidTimeRange {
                clip: clip.id,
                timeline_in: clip.timeline_in,
                timeline_out: clip.timeline_out,
            });
        }

        let media = self
            .media(clip.media_id)
            .ok_or(EngineError::UnknownMedia(clip.media_id))?;
        let compatible = match track.kind {
            TrackKind::Video => media.has_video,
            TrackKind::Audio => media.has_audio,
        };
        if !compatible {
            return Err(EngineError::IncompatibleMedia {
                track: track.id,
                media: media.id,
            });
        }
        if clip.source_in < -EPS
            || clip.source_out <= clip.source_in
            || clip.source_out > media.duration + EPS
        {
            return Err(EngineError::SourceOutOfBounds {
                clip: clip.id,
                source_in: clip.source_in,
                source_out: clip.source_out,
                media_duration: media.duration,
            });
        }

        if !clip.speed.is_finite() || clip.speed <= 0.0 {
            return Err(EngineError::InvalidProperty {
                clip: clip.id,
                property: "speed",
                value: clip.speed,
            });
        }
        let timeline_span = clip.duration();
        let source_span = clip.source_out - clip.source_in;
        // Tolerance scales with span so long clips don't trip on f64 noise.
        let tolerance = EPS * timeline_span.max(1.0);
        if (timeline_span * clip.speed - source_span).abs() > tolerance {
            return Err(EngineError::SpeedMismatch {
                clip: clip.id,
                timeline_span,
                source_span,
                speed: clip.speed,
            });
        }

        if !clip.opacity.is_finite() || !(0.0..=1.0).contains(&clip.opacity) {
            return Err(EngineError::InvalidProperty {
                clip: clip.id,
                property: "opacity",
                value: clip.opacity,
            });
        }
        if !clip.volume.is_finite() || clip.volume < 0.0 {
            return Err(EngineError::InvalidProperty {
                clip: clip.id,
                property: "volume",
                value: clip.volume,
            });
        }
        let t = &clip.transform;
        for (property, value) in [
            ("transform.x", t.x),
            ("transform.y", t.y),
            ("transform.scale", t.scale),
            ("transform.rotation", t.rotation),
        ] {
            if !value.is_finite() {
                return Err(EngineError::InvalidProperty {
                    clip: clip.id,
                    property,
                    value,
                });
            }
        }
        // Zero/negative scale would collapse or mirror the quad — no UI
        // produces it, so it's a model violation rather than a clamp.
        if t.scale <= 0.0 {
            return Err(EngineError::InvalidProperty {
                clip: clip.id,
                property: "transform.scale",
                value: t.scale,
            });
        }
        Ok(())
    }

    fn validate_unique_ids(&self) -> Result<(), EngineError> {
        let mut seen = std::collections::HashSet::new();
        let media = self.media.iter().map(|m| m.id.0);
        let tracks = self.tracks.iter().map(|t| t.id.0);
        let clips = self
            .tracks
            .iter()
            .flat_map(|t| t.clips.iter().map(|c| c.id.0));
        for id in media.chain(tracks).chain(clips) {
            if !seen.insert(id) {
                return Err(EngineError::DuplicateId(id));
            }
        }
        Ok(())
    }
}
