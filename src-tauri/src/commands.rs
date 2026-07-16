//! Thin IPC wrappers around the workspace crates. No logic lives here:
//! commands validate nothing beyond types, translate errors to strings, and
//! forward events/frames to the webview.

use std::sync::{Arc, Mutex};

use cutty_media::{MediaInfo, PlayerEvent, TimelinePlayer};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::engine_ipc::EngineHandle;

/// Managed state: the timeline playback engine. Created by
/// [`playback_attach`] (the Player component connecting its frame
/// channel); every engine mutation pushes the new project snapshot in via
/// [`sync_playback`].
#[derive(Default)]
pub struct AppState {
    playback: Arc<Mutex<Option<TimelinePlayer>>>,
}

/// Push the current project snapshot into the playback engine (called by
/// the engine IPC layer after every committed/transient change).
pub fn sync_playback(app: &AppHandle, project: cutty_engine::Project) {
    if let Some(state) = app.try_state::<AppState>() {
        let guard = state.playback.lock().expect("playback state poisoned");
        if let Some(player) = guard.as_ref() {
            player.set_project(project);
        }
    }
}

/// Probe a media file and return its properties.
#[tauri::command]
pub async fn probe_media(path: String) -> Result<MediaInfo, String> {
    tauri::async_runtime::spawn_blocking(move || cutty_media::probe(path.as_ref()))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

/// Progress payload for the `proxy://progress` event.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyProgressEvent {
    pub src_path: String,
    pub percent: f32,
    pub out_time_sec: f64,
    pub speed: f32,
}

/// Generate (or fetch from cache) the 720p proxy for a media file.
///
/// Emits `proxy://progress` events while encoding; resolves with the proxy
/// path when done. On success the playback engine re-resolves its sources,
/// so clips waiting on this proxy start rendering.
#[tauri::command]
pub async fn generate_proxy(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    path: String,
    duration_hint: Option<f64>,
) -> Result<String, String> {
    let emit_app = app.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let result = cutty_media::generate_proxy(path.as_ref(), duration_hint, |p| {
            let _ = emit_app.emit(
                "proxy://progress",
                ProxyProgressEvent {
                    src_path: path.clone(),
                    percent: p.percent,
                    out_time_sec: p.out_time_sec,
                    speed: p.speed,
                },
            );
        });
        result.map(|p| p.display().to_string())
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;

    if let Some(player) = state.playback.lock().expect("playback poisoned").as_ref() {
        player.refresh_sources();
    }
    Ok(result)
}

/// Extract (or fetch from cache) the thumbnail for a media file and return
/// the JPEG bytes directly (binary IPC, like frames — pixels never travel
/// as JSON).
#[tauri::command]
pub async fn media_thumbnail(
    path: String,
    duration_hint: Option<f64>,
) -> Result<tauri::ipc::Response, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let thumb = cutty_media::generate_thumbnail(path.as_ref(), duration_hint)?;
        std::fs::read(thumb).map_err(cutty_media::MediaError::from)
    })
    .await
    .map_err(|e| e.to_string())?
    .map(tauri::ipc::Response::new)
    .map_err(|e| e.to_string())
}

/// Generate (or fetch from cache) the filmstrip sprite for a media file
/// and return the packed bytes directly (binary IPC — see
/// `cutty_media::filmstrip` for the format). Decode-heavy on a miss;
/// runs on the blocking pool like proxies.
#[tauri::command]
pub async fn media_filmstrip(
    path: String,
    duration_hint: Option<f64>,
) -> Result<tauri::ipc::Response, String> {
    tauri::async_runtime::spawn_blocking(move || {
        cutty_media::generate_filmstrip(path.as_ref(), duration_hint)
    })
    .await
    .map_err(|e| e.to_string())?
    .map(tauri::ipc::Response::new)
    .map_err(|e| e.to_string())
}

/// Generate (or fetch from cache) the audio peak data for a media file
/// and return the packed bytes directly (binary IPC — see
/// `cutty_media::peaks` for the format). The timeline draws waveforms
/// from this; generation decodes through the symphonia→libav chain.
#[tauri::command]
pub async fn media_peaks(path: String) -> Result<tauri::ipc::Response, String> {
    tauri::async_runtime::spawn_blocking(move || cutty_media::generate_peaks(path.as_ref()))
        .await
        .map_err(|e| e.to_string())?
        .map(tauri::ipc::Response::new)
        .map_err(|e| e.to_string())
}

/// Dev bench mode (timeline perf acceptance): when `CUTTY_BENCH=1`, the
/// frontend runs the full-visuals timeline benchmark autonomously at
/// startup — importing the `:`-separated `CUTTY_BENCH_MEDIA` files,
/// seeding the 3-video + 2-audio × 62-clip layout, panning for a few
/// seconds, and reporting draw stats via [`bench_report`]. `None` in
/// normal runs (the frontend does nothing).
#[tauri::command]
pub fn bench_config() -> Option<Vec<String>> {
    if std::env::var("CUTTY_BENCH").ok()?.trim() != "1" {
        return None;
    }
    Some(
        std::env::var("CUTTY_BENCH_MEDIA")
            .ok()?
            .split(':')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
    )
}

/// Dev bench sink: save a PNG snapshot of the timeline canvas next to
/// the report (visual verification without any screen capture).
#[tauri::command]
pub fn bench_snapshot(png: Vec<u8>) -> Result<(), String> {
    let out = std::env::var("CUTTY_BENCH_OUT")
        .unwrap_or_else(|_| "/tmp/cutty-bench.json".to_string());
    std::fs::write(format!("{out}.png"), png).map_err(|e| e.to_string())
}

/// Dev bench sink: write the frontend's JSON report to
/// `CUTTY_BENCH_OUT` (default `/tmp/cutty-bench.json`) and exit.
#[tauri::command]
pub fn bench_report(report: String) -> Result<(), String> {
    let out = std::env::var("CUTTY_BENCH_OUT")
        .unwrap_or_else(|_| "/tmp/cutty-bench.json".to_string());
    std::fs::write(&out, report).map_err(|e| e.to_string())?;
    eprintln!("cutty-bench: report written to {out}");
    std::process::exit(0);
}

/// Which of the given source paths currently exist (missing-media checks).
#[tauri::command]
pub async fn paths_exist(paths: Vec<String>) -> Result<Vec<bool>, String> {
    tauri::async_runtime::spawn_blocking(move || cutty_media::paths_exist(&paths))
        .await
        .map_err(|e| e.to_string())
}

/// One transition shader available to drop onto a cut (from the GPU
/// registry, via cutty-media's re-export).
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransitionDefWire {
    pub id: &'static str,
    pub label: &'static str,
    pub default_duration: f64,
}

/// Distinct system font families (fontconfig), sorted — the Inspector's
/// font dropdown. First call loads the font database; run it off the IPC
/// thread.
#[tauri::command]
pub async fn text_font_families() -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(cutty_media::text_font_families)
        .await
        .map_err(|e| e.to_string())
}

/// Measure a text block in project pixels at transform scale 1 (the
/// player gizmo's box), using the renderer's own layout.
#[tauri::command]
pub async fn text_measure(text: cutty_engine::TextSpec) -> Result<(f64, f64), String> {
    tauri::async_runtime::spawn_blocking(move || cutty_media::measure_text_block(&text))
        .await
        .map_err(|e| e.to_string())
}

/// The transition catalog for the left-panel Transitions tab.
#[tauri::command]
pub fn transition_list() -> Vec<TransitionDefWire> {
    cutty_media::transitions()
        .iter()
        .map(|t| TransitionDefWire {
            id: t.id,
            label: t.label,
            default_duration: t.default_duration,
        })
        .collect()
}

/// Payload for `player://position` events.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionEvent {
    pub position_sec: f64,
    pub playing: bool,
}

/// Binary frame message: little-endian header + JPEG payload.
/// `[pts: f64][width: u32][height: u32][jpeg bytes…]`
fn frame_message(pts_sec: f64, width: u32, height: u32, jpeg: &[u8]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(16 + jpeg.len());
    msg.extend_from_slice(&pts_sec.to_le_bytes());
    msg.extend_from_slice(&width.to_le_bytes());
    msg.extend_from_slice(&height.to_le_bytes());
    msg.extend_from_slice(jpeg);
    msg
}

/// Start (or restart) the timeline playback engine on the current
/// project. Frames stream over `on_frame` (binary channel);
/// position/EOF/errors arrive as JSON events. Called once by the Player
/// component when it mounts.
#[tauri::command]
pub async fn playback_attach(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    engine: State<'_, EngineHandle>,
    on_frame: Channel<InvokeResponseBody>,
) -> Result<(), String> {
    let project = engine.0.lock().expect("engine poisoned").project().clone();
    let playback = state.playback.clone();

    tauri::async_runtime::spawn_blocking(move || {
        // One guard across replace-and-create: concurrent attach calls
        // serialize here. Drop any previous player first (joins threads).
        let mut guard = playback.lock().expect("playback state poisoned");
        guard.take();

        let sink = Box::new(move |event: PlayerEvent| match event {
            PlayerEvent::Frame {
                pts_sec,
                clock_sec: _,
                width,
                height,
                jpeg,
            } => {
                let _ = on_frame.send(InvokeResponseBody::Raw(frame_message(
                    pts_sec, width, height, &jpeg,
                )));
            }
            PlayerEvent::Position {
                position_sec,
                playing,
            } => {
                let _ = app.emit(
                    "player://position",
                    PositionEvent {
                        position_sec,
                        playing,
                    },
                );
            }
            PlayerEvent::Eof => {
                let _ = app.emit("player://eof", ());
            }
            PlayerEvent::Error(message) => {
                let _ = app.emit("player://error", message);
            }
        });

        let player = TimelinePlayer::open(project, sink).map_err(|e| e.to_string())?;
        *guard = Some(player);
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Transport: toggle play/pause (Space).
#[tauri::command]
pub async fn playback_toggle(state: State<'_, AppState>) -> Result<(), String> {
    with_playback(&state, |p| p.toggle_play())
}

/// Transport: play.
#[tauri::command]
pub async fn playback_play(state: State<'_, AppState>) -> Result<(), String> {
    with_playback(&state, |p| p.play())
}

/// Transport: pause.
#[tauri::command]
pub async fn playback_pause(state: State<'_, AppState>) -> Result<(), String> {
    with_playback(&state, |p| p.pause())
}

/// Transport: seek/scrub to an absolute timeline position in seconds.
#[tauri::command]
pub async fn playback_seek(state: State<'_, AppState>, position_sec: f64) -> Result<(), String> {
    with_playback(&state, |p| p.seek(position_sec))
}

/// Transport: step by `delta` project frames (negative = backwards).
#[tauri::command]
pub async fn playback_step(state: State<'_, AppState>, delta: i64) -> Result<(), String> {
    with_playback(&state, |p| p.step(delta))
}

fn with_playback(
    state: &State<'_, AppState>,
    f: impl FnOnce(&TimelinePlayer),
) -> Result<(), String> {
    match state.playback.lock().expect("playback poisoned").as_ref() {
        Some(p) => {
            f(p);
            Ok(())
        }
        None => Err("playback engine is not attached".into()),
    }
}

/// Result of a trim export, echoed back to the UI.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportResult {
    pub path: String,
    pub actual_start_sec: f64,
    pub duration_sec: f64,
}

/// Losslessly trim `[in_sec, out_sec]` of `src_path` into `dst_path`
/// (stream copy — the cut starts on the keyframe at or before `in_sec`;
/// the result reports the actual bounds). Used by the export prompt later
/// in Phase 1.
#[tauri::command]
pub async fn export_trim(
    src_path: String,
    dst_path: String,
    in_sec: f64,
    out_sec: f64,
) -> Result<ExportResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        cutty_media::export_trim(src_path.as_ref(), dst_path.as_ref(), in_sec, out_sec).map(|r| {
            ExportResult {
                path: dst_path,
                actual_start_sec: r.actual_start_sec,
                duration_sec: r.duration_sec,
            }
        })
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}
