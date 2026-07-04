//! Thin IPC wrappers around the workspace crates. No logic lives here:
//! commands validate nothing beyond types, translate errors to strings, and
//! forward events/frames to the webview.

use std::sync::{Arc, Mutex};

use cutty_media::{MediaInfo, Player, PlayerEvent, PlayerInfo};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{Emitter, State};

/// Managed state: the single active player (Phase 0 plays one file).
/// The `Arc` lets blocking sections run on `spawn_blocking` without
/// borrowing from the command's lifetime.
#[derive(Default)]
pub struct AppState {
    player: Arc<Mutex<Option<Player>>>,
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
/// path when done.
#[tauri::command]
pub async fn generate_proxy(
    app: tauri::AppHandle,
    path: String,
    duration_hint: Option<f64>,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let result = cutty_media::generate_proxy(path.as_ref(), duration_hint, |p| {
            let _ = app.emit(
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
    .map_err(|e| e.to_string())
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

/// Which of the given source paths currently exist (missing-media checks).
#[tauri::command]
pub async fn paths_exist(paths: Vec<String>) -> Result<Vec<bool>, String> {
    tauri::async_runtime::spawn_blocking(move || cutty_media::paths_exist(&paths))
        .await
        .map_err(|e| e.to_string())
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

/// Open the playback engine on a proxy file. Frames stream over `on_frame`
/// (binary channel); position/EOF/errors arrive as JSON events. Replaces
/// any previously open player.
#[tauri::command]
pub async fn open_player(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    path: String,
    on_frame: Channel<InvokeResponseBody>,
) -> Result<PlayerInfo, String> {
    let state_player = state.player.clone();
    tauri::async_runtime::spawn_blocking(move || {
        // One guard across replace-and-create: concurrent open_player
        // calls serialize here instead of both installing a live player.
        let mut guard = state_player.lock().expect("player state poisoned");
        // Drop any existing player first (joins its threads).
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

        let player = Player::open(path.as_ref(), sink).map_err(|e| e.to_string())?;
        let info = player.info().clone();
        *guard = Some(player);
        Ok(info)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Tear down the active player, if any.
#[tauri::command]
pub async fn close_player(state: State<'_, AppState>) -> Result<(), String> {
    let state_player = state.player.clone();
    tauri::async_runtime::spawn_blocking(move || {
        state_player.lock().expect("player state poisoned").take();
    })
    .await
    .map_err(|e| e.to_string())
}

/// Transport: toggle play/pause (Space).
#[tauri::command]
pub async fn player_toggle(state: State<'_, AppState>) -> Result<(), String> {
    with_player(&state, |p| p.toggle_play())
}

/// Transport: play.
#[tauri::command]
pub async fn player_play(state: State<'_, AppState>) -> Result<(), String> {
    with_player(&state, |p| p.play())
}

/// Transport: pause.
#[tauri::command]
pub async fn player_pause(state: State<'_, AppState>) -> Result<(), String> {
    with_player(&state, |p| p.pause())
}

/// Transport: seek to an absolute position in seconds.
#[tauri::command]
pub async fn player_seek(state: State<'_, AppState>, position_sec: f64) -> Result<(), String> {
    with_player(&state, |p| p.seek(position_sec))
}

/// Transport: step by `delta` frames (negative = backwards).
#[tauri::command]
pub async fn player_step(state: State<'_, AppState>, delta: i64) -> Result<(), String> {
    with_player(&state, |p| p.step(delta))
}

fn with_player(state: &State<'_, AppState>, f: impl FnOnce(&Player)) -> Result<(), String> {
    match state.player.lock().expect("player state poisoned").as_ref() {
        Some(p) => {
            f(p);
            Ok(())
        }
        None => Err("no player is open".into()),
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
/// the result reports the actual bounds).
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
