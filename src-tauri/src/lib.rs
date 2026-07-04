//! Tauri IPC surface. Commands and events only — all logic lives in the
//! workspace crates (`cutty-media`, `cutty-audio`, …).

mod commands;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(commands::AppState::default())
        .invoke_handler(tauri::generate_handler![
            commands::probe_media,
            commands::generate_proxy,
            commands::open_player,
            commands::close_player,
            commands::player_toggle,
            commands::player_play,
            commands::player_pause,
            commands::player_seek,
            commands::player_step,
            commands::export_trim
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
