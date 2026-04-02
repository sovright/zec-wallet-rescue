#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;

use commands::AppState;
use tauri::Manager;
use zeck_core::RecoveryService;

fn main() {
    tauri::Builder::default()
        .manage(AppState {
            service: RecoveryService::new(),
        })
        .invoke_handler(tauri::generate_handler![
            commands::validate_seed,
            commands::validate_address,
            commands::start_scan,
            commands::get_scan_progress,
            commands::cancel_scan,
            commands::propose_sweep,
            commands::execute_sweep,
            commands::estimate_birthday_from_date
        ])
        .setup(|app| {
            let _window = app.get_webview_window("main");
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running ZECK GUI");
}
