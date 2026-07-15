//! Export IPC: start/cancel the render job and stream its progress as
//! events. All rendering logic lives in `cutty_media::render` — this file
//! only owns the one-job-at-a-time policy and the event plumbing. The job
//! runs on a worker thread, so the editor stays fully usable while an
//! export is running.

use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use cutty_media::{CancelToken, ExportQuality, ExportSpec, ExportStage};
use tauri::{AppHandle, Emitter, State};

use crate::engine_ipc::EngineHandle;

/// Progress events (`ExportProgressEvent`).
pub const EXPORT_PROGRESS_EVENT: &str = "export://progress";
/// Successful completion (`ExportDoneEvent`).
pub const EXPORT_DONE_EVENT: &str = "export://done";
/// Failure (`ExportErrorEvent`).
pub const EXPORT_ERROR_EVENT: &str = "export://error";
/// User-requested cancellation completed (cleanup finished).
pub const EXPORT_CANCELLED_EVENT: &str = "export://cancelled";

struct RunningExport {
    cancel: Arc<CancelToken>,
    thread: JoinHandle<()>,
}

/// Managed state: at most one export at a time.
#[derive(Default)]
pub struct ExportState {
    job: Mutex<Option<RunningExport>>,
}

/// Wire form of the export request (mirrors the dialog).
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportRequest {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub quality: ExportQuality,
    pub dst_path: String,
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportProgressEvent {
    pub stage: ExportStage,
    pub percent: f32,
    pub eta_sec: Option<f64>,
    pub speed: f32,
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportDoneEvent {
    pub path: String,
    pub duration_sec: f64,
    pub encoder: String,
    pub hardware_encode: bool,
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportErrorEvent {
    pub message: String,
}

/// Which encoder exports will use (for display in the dialog).
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EncoderInfo {
    /// ffmpeg encoder name, e.g. `h264_vaapi`.
    pub encoder: String,
    /// Human-readable label, e.g. `VAAPI hardware (h264_vaapi on /dev/dri/renderD128)`.
    pub label: String,
    pub hardware: bool,
}

/// The H.264 encoder detection result (cached; detection is warmed at
/// startup, so this normally returns instantly).
#[tauri::command]
pub async fn export_detect_encoder() -> Result<EncoderInfo, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let encoder = cutty_media::detected_h264_encoder();
        EncoderInfo {
            encoder: encoder.ffmpeg_name().to_string(),
            label: encoder.label(),
            hardware: encoder.is_hardware(),
        }
    })
    .await
    .map_err(|e| e.to_string())
}

/// Start exporting the current project. Fails if an export is already
/// running. Progress/completion stream as `export://*` events.
#[tauri::command]
pub fn export_start(
    app: AppHandle,
    engine: State<'_, EngineHandle>,
    state: State<'_, ExportState>,
    request: ExportRequest,
) -> Result<(), String> {
    let mut job = state.job.lock().expect("export state poisoned");
    // Reap a finished job slot; refuse if one is genuinely running.
    if let Some(running) = job.take() {
        if running.thread.is_finished() {
            let _ = running.thread.join();
        } else {
            *job = Some(running);
            return Err("an export is already running".into());
        }
    }

    let project = engine.0.lock().expect("engine state poisoned").project().clone();
    let spec = ExportSpec {
        width: request.width,
        height: request.height,
        fps: request.fps,
        quality: request.quality,
        dst: request.dst_path.into(),
    };
    let cancel = Arc::new(CancelToken::new());

    let thread = {
        let cancel = cancel.clone();
        std::thread::Builder::new()
            .name("cutty-export".into())
            .spawn(move || {
                let mut on_progress = |p: cutty_media::ExportProgress| {
                    let _ = app.emit(
                        EXPORT_PROGRESS_EVENT,
                        ExportProgressEvent {
                            stage: p.stage,
                            percent: p.percent,
                            eta_sec: p.eta_sec,
                            speed: p.speed,
                        },
                    );
                };
                match cutty_media::run_export(&project, &spec, &cancel, &mut on_progress) {
                    Ok(summary) => {
                        eprintln!(
                            "cutty: export finished: {} ({:.2}s, {})",
                            summary.path.display(),
                            summary.duration_sec,
                            summary.encoder
                        );
                        let _ = app.emit(
                            EXPORT_DONE_EVENT,
                            ExportDoneEvent {
                                path: summary.path.display().to_string(),
                                duration_sec: summary.duration_sec,
                                encoder: summary.encoder.to_string(),
                                hardware_encode: summary.hardware_encode,
                            },
                        );
                    }
                    Err(cutty_media::MediaError::ExportCancelled) => {
                        eprintln!("cutty: export cancelled");
                        let _ = app.emit(EXPORT_CANCELLED_EVENT, ());
                    }
                    Err(e) => {
                        eprintln!("cutty: export failed: {e}");
                        let _ = app.emit(
                            EXPORT_ERROR_EVENT,
                            ExportErrorEvent {
                                message: e.to_string(),
                            },
                        );
                    }
                }
            })
            .map_err(|e| e.to_string())?
    };

    *job = Some(RunningExport { cancel, thread });
    Ok(())
}

/// Cancel the running export (kills the encoder process; cleanup runs on
/// the export thread, which then emits `export://cancelled`). No-op when
/// nothing is running.
#[tauri::command]
pub fn export_cancel(state: State<'_, ExportState>) {
    let job = state.job.lock().expect("export state poisoned");
    if let Some(running) = job.as_ref() {
        running.cancel.cancel();
    }
}
