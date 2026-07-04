//! Tauri IPC surface. Commands and events only — all logic lives in the
//! workspace crates (`cutty-media`, `cutty-audio`, …).

mod commands;
mod engine_ipc;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(commands::AppState::default())
        .manage(engine_ipc::EngineHandle::default())
        .invoke_handler(tauri::generate_handler![
            commands::probe_media,
            commands::generate_proxy,
            commands::media_thumbnail,
            commands::paths_exist,
            commands::open_player,
            commands::close_player,
            commands::player_toggle,
            commands::player_play,
            commands::player_pause,
            commands::player_seek,
            commands::player_step,
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
            engine_ipc::engine_undo,
            engine_ipc::engine_redo,
            engine_ipc::engine_begin_transaction,
            engine_ipc::engine_commit_transaction,
            engine_ipc::engine_rollback_transaction,
            engine_ipc::engine_snap_time,
            engine_ipc::engine_snap_clip_move
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
