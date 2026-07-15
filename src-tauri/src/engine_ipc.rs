//! Thin IPC wrappers around `cutty-engine`. No editing logic lives here:
//! commands translate ids/enums, forward to the engine, map errors to
//! strings, and emit a full-state snapshot event after every mutation.

use std::sync::{Arc, Mutex};

use cutty_engine::{
    ClipId, Engine, MediaId, Project, ProjectSettings, SnappedMove, TrackId, TrimEdge,
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

/// Full engine state as sent to the frontend (also the return value of
/// [`engine_get_state`] for the initial fetch).
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineSnapshot {
    pub project: Project,
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
    EngineSnapshot {
        project: engine.project().clone(),
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

/// Register a media file in the project's media pool.
#[tauri::command]
pub fn engine_add_media(
    app: AppHandle,
    state: State<'_, EngineHandle>,
    path: String,
    duration: f64,
    has_video: bool,
    has_audio: bool,
) -> Result<u64, String> {
    mutate(&app, &state, |e| {
        e.add_media(path, duration, has_video, has_audio)
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
