//! Thin IPC wrappers around `cutty-engine`. No editing logic lives here:
//! commands translate ids/enums, forward to the engine, map errors to
//! strings, and emit a full-state snapshot event after every mutation.

use std::sync::{Arc, Mutex};

use cutty_engine::{
    transition_spans, BlendMode, ClipId, Easing, Engine, FadeSide, KeyframeProp, MediaId, Project,
    ProjectSettings, SnappedMove, TextSpec, TrackFlag, TrackId, TrackKind, Transform, Transition,
    TrimEdge,
};
use tauri::{AppHandle, Emitter, State};

/// Event carrying the full engine state after every committed or transient
/// change. Projects are a few KB of JSON, so full snapshots keep the
/// frontend trivially consistent (matching `EngineEvent::ProjectChanged`).
pub const ENGINE_STATE_EVENT: &str = "engine://project";

/// Managed state: the single engine instance behind a mutex. Engine
/// operations are microseconds on Phase 1 projects, so commands run
/// synchronously on the IPC thread. `Arc` so the autosave worker can hold
/// a reference too.
pub struct EngineHandle(pub Arc<Mutex<Engine>>);

impl Default for EngineHandle {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(
            Engine::new(ProjectSettings::default()),
        )))
    }
}

/// A transition resolved to its effective on-timeline span (mirrors
/// `cutty_engine::TransitionSpan`). The UI renders chips exactly where
/// these say — clamping and centering stay engine-side.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransitionSpanWire {
    pub track_id: u64,
    pub from_clip_id: u64,
    pub to_clip_id: u64,
    pub kind: String,
    pub cut: f64,
    pub start: f64,
    pub end: f64,
    /// Stored (requested) duration, seconds.
    pub requested: f64,
    /// The longest duration this cut currently supports.
    pub max_duration: f64,
}

/// Full engine state as sent to the frontend (also the return value of
/// [`engine_get_state`] for the initial fetch).
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineSnapshot {
    pub project: Project,
    /// Resolved transition spans for the project above (derived state —
    /// recomputed with every snapshot).
    pub transitions: Vec<TransitionSpanWire>,
    pub undo_depth: usize,
    pub redo_depth: usize,
    pub transaction_active: bool,
}

/// Wire form of [`TrimEdge`].
#[derive(Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TrimEdgeArg {
    Start,
    End,
}

impl From<TrimEdgeArg> for TrimEdge {
    fn from(edge: TrimEdgeArg) -> Self {
        match edge {
            TrimEdgeArg::Start => TrimEdge::Start,
            TrimEdgeArg::End => TrimEdge::End,
        }
    }
}

fn snapshot(engine: &mut Engine) -> EngineSnapshot {
    // The snapshot below supersedes any pending per-command events.
    engine.drain_events();
    let transitions = transition_spans(engine.project())
        .into_iter()
        .map(|s| TransitionSpanWire {
            track_id: s.track_id.0,
            from_clip_id: s.from_clip.0,
            to_clip_id: s.to_clip.0,
            kind: s.kind,
            cut: s.cut,
            start: s.start,
            end: s.end,
            requested: s.requested,
            max_duration: s.max_duration,
        })
        .collect();
    EngineSnapshot {
        project: engine.project().clone(),
        transitions,
        undo_depth: engine.undo_depth(),
        redo_depth: engine.redo_depth(),
        transaction_active: engine.transaction_active(),
    }
}

/// Emit the full snapshot and fan the change out to every mirror: the
/// playback engine and the persistence layer (dirty meta + autosave).
/// Also used by project load/new/restore after swapping the engine.
pub(crate) fn emit_state(app: &AppHandle, engine: &mut Engine) {
    let _ = app.emit(ENGINE_STATE_EVENT, snapshot(engine));
    // The playback engine mirrors every project change (its scrub/pump
    // paths resolve against the newest snapshot).
    crate::commands::sync_playback(app, engine.project().clone());
    crate::project_ipc::notify_mutation(app, engine.project());
}

/// Run a mutating engine operation; on success emit the new state.
/// Failed commands leave the engine untouched, so nothing is emitted.
fn mutate<T>(
    app: &AppHandle,
    state: &State<'_, EngineHandle>,
    op: impl FnOnce(&mut Engine) -> Result<T, cutty_engine::EngineError>,
) -> Result<T, String> {
    let mut engine = state.0.lock().expect("engine state poisoned");
    let result = op(&mut engine).map_err(|e| e.to_string())?;
    emit_state(app, &mut engine);
    Ok(result)
}

/// Fetch the current state (initial load; afterwards the frontend follows
/// `engine://project` events).
#[tauri::command]
pub fn engine_get_state(state: State<'_, EngineHandle>) -> EngineSnapshot {
    snapshot(&mut state.0.lock().expect("engine state poisoned"))
}

/// Register a media file in the project's media pool. `kind` and
/// `has_alpha` come from the probe (`probe_media`); older callers'
/// video/audio semantics are the `Video`/`Audio` kinds.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn engine_add_media(
    app: AppHandle,
    state: State<'_, EngineHandle>,
    path: String,
    duration: f64,
    has_video: bool,
    has_audio: bool,
    has_alpha: bool,
    kind: cutty_engine::MediaKind,
) -> Result<u64, String> {
    mutate(&app, &state, |e| {
        e.add_media_with_kind(path, duration, has_video, has_audio, has_alpha, kind)
            .map(|m| m.0)
    })
}

/// Remove a media file from the pool and every clip referencing it, as a
/// single undoable command.
#[tauri::command]
pub fn engine_remove_media(
    app: AppHandle,
    state: State<'_, EngineHandle>,
    media_id: u64,
) -> Result<(), String> {
    mutate(&app, &state, |e| e.remove_media(MediaId(media_id)))
}

/// Place a new clip on a track; returns the new clip's id.
#[tauri::command]
pub fn engine_add_clip(
    app: AppHandle,
    state: State<'_, EngineHandle>,
    track_id: u64,
    media_id: u64,
    timeline_in: f64,
    source_in: f64,
    source_out: f64,
) -> Result<u64, String> {
    mutate(&app, &state, |e| {
        e.add_clip(
            TrackId(track_id),
            MediaId(media_id),
            timeline_in,
            source_in,
            source_out,
        )
        .map(|c| c.0)
    })
}

/// Place a new text clip. `track_id: null` = CapCut placement (topmost
/// text lane with room, else a new lane on top — one undo step). The
/// preset transform places lower-thirds etc. Returns the new clip's id.
#[tauri::command]
pub fn engine_add_text_clip(
    app: AppHandle,
    state: State<'_, EngineHandle>,
    timeline_in: f64,
    duration: f64,
    text: TextSpec,
    transform: Option<Transform>,
    track_id: Option<u64>,
) -> Result<u64, String> {
    mutate(&app, &state, |e| {
        e.add_text_clip(
            timeline_in,
            duration,
            text,
            transform.unwrap_or_default(),
            track_id.map(TrackId),
        )
        .map(|c| c.0)
    })
}

/// Replace a text clip's payload (content and/or style). Equal payloads
/// are a no-op (no undo entry).
#[tauri::command]
pub fn engine_set_clip_text(
    app: AppHandle,
    state: State<'_, EngineHandle>,
    clip_id: u64,
    text: TextSpec,
) -> Result<(), String> {
    mutate(&app, &state, |e| e.set_clip_text(ClipId(clip_id), text))
}

/// Move a clip to a new timeline position.
#[tauri::command]
pub fn engine_move_clip(
    app: AppHandle,
    state: State<'_, EngineHandle>,
    clip_id: u64,
    timeline_in: f64,
) -> Result<(), String> {
    mutate(&app, &state, |e| e.move_clip(ClipId(clip_id), timeline_in))
}

/// Drag one edge of a clip to (approximately) `to`; returns the clamped
/// edge time actually applied.
#[tauri::command]
pub fn engine_trim_clip(
    app: AppHandle,
    state: State<'_, EngineHandle>,
    clip_id: u64,
    edge: TrimEdgeArg,
    to: f64,
) -> Result<f64, String> {
    mutate(&app, &state, |e| {
        e.trim_clip(ClipId(clip_id), edge.into(), to)
    })
}

/// Split a clip at a timeline time; returns the new right half's id.
#[tauri::command]
pub fn engine_split_clip(
    app: AppHandle,
    state: State<'_, EngineHandle>,
    clip_id: u64,
    at: f64,
) -> Result<u64, String> {
    mutate(&app, &state, |e| {
        e.split_clip(ClipId(clip_id), at).map(|c| c.0)
    })
}

/// Remove a clip, leaving a gap.
#[tauri::command]
pub fn engine_delete_clip(
    app: AppHandle,
    state: State<'_, EngineHandle>,
    clip_id: u64,
) -> Result<(), String> {
    mutate(&app, &state, |e| e.delete_clip(ClipId(clip_id)))
}

/// Remove a clip and shift later clips on the track left.
#[tauri::command]
pub fn engine_ripple_delete(
    app: AppHandle,
    state: State<'_, EngineHandle>,
    clip_id: u64,
) -> Result<(), String> {
    mutate(&app, &state, |e| e.ripple_delete(ClipId(clip_id)))
}

/// Set a clip's audio gain (linear; 1.0 = unity, 0.0 = silent).
#[tauri::command]
pub fn engine_set_clip_volume(
    app: AppHandle,
    state: State<'_, EngineHandle>,
    clip_id: u64,
    volume: f64,
) -> Result<(), String> {
    mutate(&app, &state, |e| e.set_clip_volume(ClipId(clip_id), volume))
}

/// Add a keyframe (or replace the one already at that time) on a clip
/// property lane. `t` is clip-relative seconds; returns the time the
/// keyframe landed on after clamping/dedup.
#[tauri::command]
pub fn engine_add_keyframe(
    app: AppHandle,
    state: State<'_, EngineHandle>,
    clip_id: u64,
    prop: KeyframeProp,
    t: f64,
    value: f64,
    easing: Option<Easing>,
) -> Result<f64, String> {
    mutate(&app, &state, |e| {
        e.add_keyframe(ClipId(clip_id), prop, t, value, easing.unwrap_or_default())
    })
}

/// Move the keyframe at `from_t` to `to_t` with a new value (both axes
/// of a dot drag). Returns the applied time after neighbor clamping.
#[tauri::command]
pub fn engine_move_keyframe(
    app: AppHandle,
    state: State<'_, EngineHandle>,
    clip_id: u64,
    prop: KeyframeProp,
    from_t: f64,
    to_t: f64,
    value: f64,
) -> Result<f64, String> {
    mutate(&app, &state, |e| {
        e.move_keyframe(ClipId(clip_id), prop, from_t, to_t, value)
    })
}

/// Remove the keyframe at clip-relative time `t`.
#[tauri::command]
pub fn engine_remove_keyframe(
    app: AppHandle,
    state: State<'_, EngineHandle>,
    clip_id: u64,
    prop: KeyframeProp,
    t: f64,
) -> Result<(), String> {
    mutate(&app, &state, |e| {
        e.remove_keyframe(ClipId(clip_id), prop, t)
    })
}

/// Set a clip's fade-in or fade-out duration in seconds (0 removes it)
/// — sugar over the volume keyframe lane. Returns the applied duration.
#[tauri::command]
pub fn engine_set_clip_fade(
    app: AppHandle,
    state: State<'_, EngineHandle>,
    clip_id: u64,
    side: FadeSide,
    duration: f64,
) -> Result<f64, String> {
    mutate(&app, &state, |e| {
        e.set_clip_fade(ClipId(clip_id), side, duration)
    })
}

/// Extract a video clip's audio onto an audio track (one undo step);
/// the video clip's own volume drops to 0. Returns the new clip's id.
#[tauri::command]
pub fn engine_extract_audio(
    app: AppHandle,
    state: State<'_, EngineHandle>,
    clip_id: u64,
) -> Result<u64, String> {
    mutate(&app, &state, |e| {
        e.extract_audio(ClipId(clip_id)).map(|c| c.0)
    })
}

/// Set a clip's 2D placement (position/scale/rotation).
#[tauri::command]
pub fn engine_set_clip_transform(
    app: AppHandle,
    state: State<'_, EngineHandle>,
    clip_id: u64,
    transform: Transform,
) -> Result<(), String> {
    mutate(&app, &state, |e| {
        e.set_clip_transform(ClipId(clip_id), transform)
    })
}

/// Set a clip's opacity (0.0 transparent .. 1.0 opaque).
#[tauri::command]
pub fn engine_set_clip_opacity(
    app: AppHandle,
    state: State<'_, EngineHandle>,
    clip_id: u64,
    opacity: f64,
) -> Result<(), String> {
    mutate(&app, &state, |e| {
        e.set_clip_opacity(ClipId(clip_id), opacity)
    })
}

/// Set how a clip blends with the layers below it.
#[tauri::command]
pub fn engine_set_clip_blend_mode(
    app: AppHandle,
    state: State<'_, EngineHandle>,
    clip_id: u64,
    mode: BlendMode,
) -> Result<(), String> {
    mutate(&app, &state, |e| {
        e.set_clip_blend_mode(ClipId(clip_id), mode)
    })
}

/// Move a clip onto another track (and position) in one step.
#[tauri::command]
pub fn engine_move_clip_to_track(
    app: AppHandle,
    state: State<'_, EngineHandle>,
    clip_id: u64,
    track_id: u64,
    timeline_in: f64,
) -> Result<(), String> {
    mutate(&app, &state, |e| {
        e.move_clip_to_track(ClipId(clip_id), TrackId(track_id), timeline_in)
    })
}

/// Insert a new empty track at panel `index` (0 = top; clamped). Returns
/// the new track's id.
#[tauri::command]
pub fn engine_add_track(
    app: AppHandle,
    state: State<'_, EngineHandle>,
    kind: TrackKind,
    index: usize,
) -> Result<u64, String> {
    mutate(&app, &state, |e| e.add_track(kind, index).map(|t| t.0))
}

/// Remove a track with all its clips (one undo step). Rejected for the
/// last track of a kind and for locked tracks.
#[tauri::command]
pub fn engine_remove_track(
    app: AppHandle,
    state: State<'_, EngineHandle>,
    track_id: u64,
) -> Result<(), String> {
    mutate(&app, &state, |e| e.remove_track(TrackId(track_id)))
}

/// Move a track to another panel position (restacks the composite).
#[tauri::command]
pub fn engine_move_track(
    app: AppHandle,
    state: State<'_, EngineHandle>,
    track_id: u64,
    to: usize,
) -> Result<(), String> {
    mutate(&app, &state, |e| e.move_track(TrackId(track_id), to))
}

/// Flip a per-track flag: "locked" | "muted" | "hidden".
#[tauri::command]
pub fn engine_set_track_flag(
    app: AppHandle,
    state: State<'_, EngineHandle>,
    track_id: u64,
    flag: TrackFlag,
    value: bool,
) -> Result<(), String> {
    mutate(&app, &state, |e| {
        e.set_track_flag(TrackId(track_id), flag, value)
    })
}

/// Wire form of a transition assignment.
#[derive(Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransitionArg {
    pub kind: String,
    pub duration: f64,
}

/// Set, replace, or remove (`transition: null`) the transition at a
/// clip's out cut. Returns the stored duration after clamping (`null`
/// when removing).
#[tauri::command]
pub fn engine_set_transition(
    app: AppHandle,
    state: State<'_, EngineHandle>,
    clip_id: u64,
    transition: Option<TransitionArg>,
) -> Result<Option<f64>, String> {
    mutate(&app, &state, |e| {
        e.set_transition(
            ClipId(clip_id),
            transition.map(|t| Transition {
                kind: t.kind,
                duration: t.duration,
            }),
        )
    })
}

/// Undo the most recent command; `false` when there was nothing to undo.
#[tauri::command]
pub fn engine_undo(app: AppHandle, state: State<'_, EngineHandle>) -> Result<bool, String> {
    mutate(&app, &state, |e| e.undo())
}

/// Redo the most recently undone command; `false` when there was nothing
/// to redo.
#[tauri::command]
pub fn engine_redo(app: AppHandle, state: State<'_, EngineHandle>) -> Result<bool, String> {
    mutate(&app, &state, |e| e.redo())
}

/// Open a gesture transaction (mousedown of a drag).
#[tauri::command]
pub fn engine_begin_transaction(
    app: AppHandle,
    state: State<'_, EngineHandle>,
) -> Result<(), String> {
    mutate(&app, &state, |e| e.begin_transaction())
}

/// Commit the open transaction into a single undo entry (mouseup).
#[tauri::command]
pub fn engine_commit_transaction(
    app: AppHandle,
    state: State<'_, EngineHandle>,
) -> Result<(), String> {
    mutate(&app, &state, |e| e.commit_transaction())
}

/// Abort the open transaction, restoring pre-gesture state (Escape).
#[tauri::command]
pub fn engine_rollback_transaction(
    app: AppHandle,
    state: State<'_, EngineHandle>,
) -> Result<(), String> {
    mutate(&app, &state, |e| e.rollback_transaction())
}

/// Snap a time against clip edges and the playhead. Read-only. The UI
/// converts its pixel snap radius to seconds before calling.
#[tauri::command]
pub fn engine_snap_time(
    state: State<'_, EngineHandle>,
    t: f64,
    threshold: f64,
    playhead: Option<f64>,
    exclude: Vec<u64>,
) -> Option<f64> {
    let engine = state.0.lock().expect("engine state poisoned");
    let exclude: Vec<ClipId> = exclude.into_iter().map(ClipId).collect();
    cutty_engine::snap_time(engine.project(), t, threshold, playhead, &exclude)
}

/// Snap a clip-move gesture (both edges compete). Read-only.
#[tauri::command]
pub fn engine_snap_clip_move(
    state: State<'_, EngineHandle>,
    clip_id: u64,
    desired_in: f64,
    threshold: f64,
    playhead: Option<f64>,
) -> Result<SnappedMove, String> {
    let engine = state.0.lock().expect("engine state poisoned");
    cutty_engine::snap_clip_move(
        engine.project(),
        ClipId(clip_id),
        desired_in,
        threshold,
        playhead,
    )
    .map_err(|e| e.to_string())
}
