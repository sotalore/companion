//! The commands the window's JavaScript can call, with `invoke("name", { args })`. Each is a plain
//! function marked `#[tauri::command]`; Tauri turns the JS arguments into Rust values and the return
//! value (or the `Err` message) back into JSON.

use crate::exports;
use crate::history;
use crate::secrets;
use crate::settings::{self, Settings};
use crate::state::{self, lock, Activity, AppState, Connection};
use crate::watcher::Job;
use serde::Serialize;
use std::path::PathBuf;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_opener::OpenerExt;

/// Everything the window shows, in one answer.
#[derive(Serialize)]
pub struct StateView {
    version: &'static str,
    settings: Settings,
    /// The two clients' `Lua/` folders, for the quick-pick buttons.
    lua_folders: Vec<LuaFolder>,
    has_token: bool,
    connection: Connection,
    watching: Option<PathBuf>,
    files: Vec<FileView>,
    activity: Vec<Activity>,
}

#[derive(Serialize)]
pub struct LuaFolder {
    label: &'static str,
    path: PathBuf,
    exists: bool,
}

/// One export file in the folder.
#[derive(Serialize)]
pub struct FileView {
    name: String,
    /// `recipes (all)`, or why it can't be read.
    label: String,
    uploadable: bool,
    modified: String,
    /// When these exact bytes were uploaded, if they were.
    uploaded_at: Option<String>,
    review_url: Option<String>,
    /// When an older version of the file was uploaded, if the current one wasn't.
    previous_upload_at: Option<String>,
}

#[tauri::command]
pub fn get_state(state: State<'_, AppState>) -> StateView {
    let settings = state.settings();
    let has_token = matches!(secrets::get_token(settings.site()), Ok(Some(_)));
    StateView {
        version: env!("CARGO_PKG_VERSION"),
        lua_folders: settings::lua_folders()
            .into_iter()
            .map(|(label, path)| LuaFolder { label, exists: path.is_dir(), path })
            .collect(),
        has_token,
        connection: lock(&state.connection).clone(),
        watching: state.watching(),
        files: list_files(&state, &settings),
        activity: lock(&state.activity).iter().cloned().collect(),
        settings,
    }
}

/// The export files in the `Lua/` folder, newest first. Reads each one to find its type (they're small,
/// and this runs only when the window asks).
fn list_files(state: &AppState, settings: &Settings) -> Vec<FileView> {
    let Some(folder) = &settings.lua_folder else { return vec![] };
    let Ok(entries) = std::fs::read_dir(folder) else { return vec![] };
    let history = lock(&state.history);
    let mut files: Vec<(std::time::SystemTime, FileView)> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && exports::is_candidate(path))
        .filter_map(|path| {
            let name = path.file_name()?.to_string_lossy().into_owned();
            let modified_time = std::fs::metadata(&path).and_then(|m| m.modified()).ok()?;
            let modified = chrono::DateTime::<chrono::Local>::from(modified_time).format("%Y-%m-%d %H:%M").to_string();
            let bytes = std::fs::read(&path).ok()?;
            let (label, uploadable) = match exports::read_header(&bytes) {
                Ok(header) => (exports::describe(&header), matches!(exports::classify(&header), exports::Kind::Upload { .. })),
                Err(_) => ("not valid JSON".to_string(), false),
            };
            let this_upload = history.find(&history::sha256(&bytes), settings.site());
            let previous = history.latest_for_file(&name, settings.site());
            Some((
                modified_time,
                FileView {
                    name,
                    label,
                    uploadable,
                    modified,
                    uploaded_at: this_upload.map(|r| r.at.clone()),
                    review_url: this_upload.and_then(|r| r.review_url.clone()),
                    previous_upload_at: if this_upload.is_none() { previous.map(|r| r.at.clone()) } else { None },
                },
            ))
        })
        .collect();
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified)); // newest first
    files.into_iter().map(|(_, f)| f).collect()
}

#[tauri::command]
pub async fn save_settings(
    app: AppHandle,
    lua_folder: Option<String>,
    site_url: String,
    auto_upload: bool,
) -> Result<(), String> {
    let site_url = settings::check_site_url(&site_url)?;
    let lua_folder = match lua_folder.map(|f| f.trim().to_string()).filter(|f| !f.is_empty()) {
        Some(folder) => {
            let path = PathBuf::from(folder);
            if !path.is_dir() {
                return Err(format!("There's no folder at {}", path.display()));
            }
            Some(path)
        }
        None => None,
    };
    let site_changed = {
        let state = app.state::<AppState>();
        let mut settings = lock(&state.settings);
        let site_changed = settings.site() != site_url;
        *settings = Settings { lua_folder, site_url, auto_upload };
        settings.save(&state.settings_path).map_err(|e| format!("Couldn't save the settings: {e}"))?;
        site_changed
    };
    if site_changed {
        // A different site has its own token (or none), so check again.
        state::check_connection(app).await;
    } else {
        state::refresh_watcher(&app);
    }
    Ok(())
}

#[tauri::command]
pub async fn save_token(app: AppHandle, token: String) -> Result<(), String> {
    let token = token.trim();
    if !token.starts_with("slt_") {
        return Err("That isn't an API token: they start with slt_.".into());
    }
    let site = app.state::<AppState>().settings().site().to_string();
    secrets::set_token(&site, token)?;
    state::check_connection(app).await;
    Ok(())
}

#[tauri::command]
pub async fn clear_token(app: AppHandle) -> Result<(), String> {
    let site = app.state::<AppState>().settings().site().to_string();
    secrets::clear_token(&site)?;
    state::check_connection(app).await;
    Ok(())
}

#[tauri::command]
pub async fn check_connection(app: AppHandle) {
    state::check_connection(app).await;
}

/// The Upload button: queue one file from the `Lua/` folder, by name.
#[tauri::command]
pub fn upload_file(state: State<'_, AppState>, name: String) -> Result<(), String> {
    let folder = state.settings().lua_folder.ok_or("Choose the game's Lua folder first.")?;
    // Only a bare file name from the list: no `..` or folders.
    let path = folder.join(&name);
    if path.parent() != Some(folder.as_path()) || !exports::is_candidate(&path) {
        return Err(format!("Not an export file: {name}"));
    }
    state.queue.send(Job { path, manual: true }).map_err(|_| "The upload queue has stopped.".to_string())
}

/// Opens a review link in the browser. Only web links: nothing else goes to the shell.
#[tauri::command]
pub fn open_url(app: AppHandle, url: String) -> Result<(), String> {
    if settings::check_site_url(&url).is_err() {
        return Err(format!("Not a link to open: {url}"));
    }
    app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
}
