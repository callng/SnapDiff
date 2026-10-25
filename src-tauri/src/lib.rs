mod commands;
#[cfg(test)]
mod diff;
mod entry_codec;
mod ignore;
mod models;
mod scanner;
mod sql_diff;
mod startup_window;
mod storage;
mod storage_location;
#[cfg(test)]
mod validation;

use commands::*;
use tauri::Manager;
use tauri_plugin_dialog::DialogExt;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(startup_window::Readiness::default())
        .plugin(tauri_plugin_dialog::init())
        // Keep VISIBLE excluded: only the startup gate may reveal the window.
        .plugin(
            tauri_plugin_window_state::Builder::new()
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::SIZE
                        | tauri_plugin_window_state::StateFlags::POSITION
                        | tauri_plugin_window_state::StateFlags::MAXIMIZED,
                )
                .with_filter(|label| label == "main")
                .build(),
        )
        // Plugin callbacks run in registration order, after state restoration.
        .plugin(startup_window::plugin())
        .on_page_load(|webview, payload| {
            // Wait for local HTML/CSS to load. Never expose WebView2's empty
            // document; the matching native background covers its first paint.
            if webview.label() == "main"
                && payload.event() == tauri::webview::PageLoadEvent::Finished
            {
                startup_window::page_loaded(&webview.window());
            }
        })
        .setup(|app| {
            let path = app.path().app_data_dir()?;
            let config = app.path().app_config_dir()?;
            let executable = std::env::current_exe()?;
            let location = storage_location::StorageLocation::new(
                &config,
                path,
                executable.parent().ok_or("安装目录无效")?,
            );
            let result =
                tauri::async_runtime::block_on(tauri::async_runtime::spawn_blocking(move || {
                    storage::Store::open_configured(location)
                }))?;
            let store = result.map_err(|error| {
                app.dialog()
                    .message(&error)
                    .title("SnapDiff 数据库无法打开")
                    .kind(tauri_plugin_dialog::MessageDialogKind::Error)
                    .blocking_show();
                std::io::Error::other(error)
            })?;
            app.manage(Backend::new(store));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            scan_directory,
            cancel_scan,
            list_directories,
            get_directory,
            list_history,
            select_baseline,
            compare_states,
            query_diff,
            get_activity,
            preview_retention,
            update_settings,
            set_focus_patterns,
            delete_nodes,
            set_remark,
            delete_directory,
            get_report,
            export_report,
            get_diff_paths,
            export_snapshots,
            preview_import,
            import_snapshots,
            get_storage_info,
            migrate_database,
            validate_directory_path
        ])
        .run(tauri::generate_context!())
        .expect("error while running SnapDiff");
}
