//! The SotA Lore companion. `run` builds the app: its state, the workers and the window, then hands
//! control to Tauri's event loop until the window is closed.
//!
//! The modules, roughly in the order a file goes through them:
//! - `settings`, `secrets`: where the folder, the site and the token are kept.
//! - `watcher`: notices export files written to the game's `Lua/` folder.
//! - `exports`: reads a file's header and decides, by its `type`, whether and where to upload it.
//! - `uploader`: the queue's worker, which reads, checks and uploads each file.
//! - `api`: talks to the site (`GET /me`, the uploads), with the retry rules.
//! - `history`: remembers what was uploaded.
//! - `scenes`: the scene changes read from the plugin's current-scene file.
//! - `screenshots`: new screenshots, matched to a scene, waiting for Darren to send or skip.
//! - `recorder`, `notes`, `transcribe`: push-to-talk spoken notes, labelled by scene, and Whisper.
//! - `skills`: compares the game's skills file with the site's skills, and reports what differs.
//! - `state`, `commands`: what the window shows and the actions it can take.

mod api;
mod commands;
mod exports;
mod history;
mod notes;
mod recorder;
mod scenes;
mod screenshots;
mod secrets;
mod settings;
mod skills;
mod state;
mod transcribe;
mod uploader;
mod watcher;

use state::AppState;
use tauri::{AppHandle, Manager};
use tauri_plugin_window_state::{StateFlags, WindowExt};

/// What the window remembers between runs: where it was and how big, so it reopens on the same
/// monitor. Not whether it was shown, since it always opens.
const REMEMBER: StateFlags = StateFlags::POSITION.union(StateFlags::SIZE).union(StateFlags::MAXIMIZED);

pub fn run() {
    tauri::Builder::default()
        // A second start of the app shows the running one instead (must be the first plugin).
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_window(app)))
        .plugin(tauri_plugin_opener::init())
        // Push-to-talk: a key that works while the game has focus (`notes::register_talk_key`).
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        // Saves the window's position and size when it closes (in the app's config folder).
        .plugin(tauri_plugin_window_state::Builder::new().with_state_flags(REMEMBER).build())
        .setup(|app| {
            let config_dir = app.path().app_config_dir()?;
            let data_dir = app.path().app_data_dir()?;
            // The queues from the watchers (and the Upload button) to the two workers. A channel is
            // a thread-safe queue: one end sends, the other waits for the next item.
            let (queue, jobs) = tokio::sync::mpsc::unbounded_channel();
            let (shots_queue, new_shots) = tokio::sync::mpsc::unbounded_channel();
            let recorder = notes::start_recorder(app.handle());
            app.manage(AppState::new(config_dir.join("settings.json"), &data_dir, queue, shots_queue, recorder));
            uploader::start(app.handle().clone(), jobs);
            screenshots::start(app.handle().clone(), new_shots);

            // Screenshots and notes from last time: let the window show them.
            let state = app.state::<AppState>();
            for shot in &state::lock(&state.shots).list {
                let _ = app.asset_protocol_scope().allow_file(&shot.path);
            }
            for note in &state::lock(&state.notes).list {
                let _ = app.asset_protocol_scope().allow_file(&note.path);
            }
            notes::register_talk_key(app.handle());

            // Start watching now (scene changes need no token), then check the token with the site.
            state::refresh_watchers(app.handle());
            tauri::async_runtime::spawn(state::check_connection(app.handle().clone()));

            // The window starts hidden (tauri.conf.json) so it can be put back where it was before
            // it appears, rather than flash up in the default place first. Closing it quits the app;
            // minimizing it keeps the app running.
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.restore_state(REMEMBER);
                window.show()?;
            }
            state::notify_ui(app.handle()); // sets the title
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_state,
            commands::save_settings,
            commands::save_token,
            commands::clear_token,
            commands::check_connection,
            commands::upload_file,
            commands::open_url,
            commands::send_screenshot,
            commands::skip_screenshot,
            commands::search_scenes,
            commands::check_skills,
            commands::set_note_scene,
            commands::set_note_transcript,
            commands::delete_note,
            commands::transcribe_notes,
            commands::upload_note,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the companion");
}

/// A second start of the app brings the running one's window forward (Darren's own action).
fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}
