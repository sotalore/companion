//! The SotA Lore companion. `run` builds the app: its state, the upload worker, the tray icon and the
//! window, then hands control to Tauri's event loop until the user picks Quit.
//!
//! The modules, roughly in the order a file goes through them:
//! - `settings`, `secrets`: where the folder, the site and the token are kept.
//! - `watcher`: notices export files written to the game's `Lua/` folder.
//! - `exports`: reads a file's header and decides, by its `type`, whether and where to upload it.
//! - `uploader`: the queue's worker, which reads, checks and uploads each file.
//! - `api`: talks to the site (`GET /me`, the uploads), with the retry rules.
//! - `history`: remembers what was uploaded.
//! - `state`, `commands`: what the window shows and the actions it can take.

mod api;
mod commands;
mod exports;
mod history;
mod secrets;
mod settings;
mod state;
mod uploader;
mod watcher;

use state::AppState;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WindowEvent};

pub fn run() {
    tauri::Builder::default()
        // A second start of the app shows the running one instead (must be the first plugin).
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_window(app)))
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let config_dir = app.path().app_config_dir()?;
            let data_dir = app.path().app_data_dir()?;
            // The queue between the watcher (and the Upload button) and the upload worker.
            let (queue, jobs) = tokio::sync::mpsc::unbounded_channel();
            app.manage(AppState::new(config_dir.join("settings.json"), data_dir.join("uploads.json"), queue));
            uploader::start(app.handle().clone(), jobs);

            build_tray(app.handle())?;

            // On start, check the token with the site; that also starts the watcher.
            tauri::async_runtime::spawn(state::check_connection(app.handle().clone()));
            Ok(())
        })
        // Closing the window hides it; the app keeps watching from the tray until Quit.
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_state,
            commands::save_settings,
            commands::save_token,
            commands::clear_token,
            commands::check_connection,
            commands::upload_file,
            commands::open_url,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the companion");
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;
    TrayIconBuilder::with_id("main")
        .icon(app.default_window_icon().expect("the app has an icon").clone())
        .tooltip("SotA Lore Companion")
        .menu(&menu)
        .show_menu_on_left_click(false) // left click shows the window; right click opens the menu
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                show_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}
