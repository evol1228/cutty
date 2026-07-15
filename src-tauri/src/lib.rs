//! Tauri IPC surface. Commands and events only — all logic lives in the
//! workspace crates (`cutty-media`, `cutty-audio`, `cutty-engine`, …).

mod commands;
mod engine_ipc;
mod export_ipc;
mod project_ipc;

use tauri::{Emitter, Manager};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let engine = engine_ipc::EngineHandle::default();
    let session = {
        let guard = engine.0.lock().expect("engine state poisoned");
        project_ipc::SessionState::new(guard.project())
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .manage(commands::AppState::default())
        .manage(engine)
        .manage(session)
        .manage(export_ipc::ExportState::default())
        .setup(|app| {
            project_ipc::start_autosaver(app.handle());
            // Warm the encoder-detection cache so the export dialog opens
            // with the answer in hand (functional ffmpeg probes take a
            // few hundred ms — never on the startup path).
            cutty_media::start_encoder_detection();
            Ok(())
        })
        .on_window_event(|window, event| {
            // Unsaved-changes guard: hand the close over to the frontend,
            // which shows the save/discard/cancel dialog and destroys the
            // window itself.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let app = window.app_handle();
                if project_ipc::has_unsaved_changes(app) {
                    api.prevent_close();
                    let _ = app.emit(project_ipc::CLOSE_REQUESTED_EVENT, ());
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::probe_media,
            commands::generate_proxy,
            commands::media_thumbnail,
            commands::paths_exist,
            commands::playback_attach,
            commands::playback_toggle,
            commands::playback_play,
            commands::playback_pause,
            commands::playback_seek,
            commands::playback_step,
            commands::export_trim,
            engine_ipc::engine_get_state,
            engine_ipc::engine_add_media,
            engine_ipc::engine_remove_media,
            engine_ipc::engine_add_clip,
            engine_ipc::engine_move_clip,
            engine_ipc::engine_trim_clip,
            engine_ipc::engine_split_clip,
            engine_ipc::engine_delete_clip,
            engine_ipc::engine_ripple_delete,
            engine_ipc::engine_set_clip_volume,
            engine_ipc::engine_undo,
            engine_ipc::engine_redo,
            engine_ipc::engine_begin_transaction,
            engine_ipc::engine_commit_transaction,
            engine_ipc::engine_rollback_transaction,
            engine_ipc::engine_snap_time,
            engine_ipc::engine_snap_clip_move,
            export_ipc::export_detect_encoder,
            export_ipc::export_start,
            export_ipc::export_cancel,
            project_ipc::project_meta,
            project_ipc::project_save,
            project_ipc::project_load,
            project_ipc::project_new,
            project_ipc::project_recents,
            project_ipc::project_remove_recent,
            project_ipc::project_recovery_scan,
            project_ipc::project_restore_autosave,
            project_ipc::project_discard_autosave,
            project_ipc::project_discard_current_autosave
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
