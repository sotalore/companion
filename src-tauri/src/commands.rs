//! The commands the window's JavaScript can call, with `invoke("name", { args })`. Each is a plain
//! function marked `#[tauri::command]`; Tauri turns the JS arguments into Rust values and the return
//! value (or the `Err` message) back into JSON.

use crate::api::{Client, SiteScene};
use crate::exports;
use crate::history;
use crate::notes::{self, Note, Talk};
use crate::scenes::SceneEntry;
use crate::screenshots::{self, Shot};
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
    /// The two clients' folders, for the quick-pick buttons.
    lua_folders: Vec<LuaFolder>,
    screenshot_folders: Vec<LuaFolder>,
    /// A warning when the Lua and screenshot folders look like different clients.
    client_mismatch: Option<String>,
    has_token: bool,
    connection: Connection,
    watching_lua: Option<PathBuf>,
    watching_shots: Option<PathBuf>,
    /// Whether new exports are uploaded without asking right now.
    auto_uploading: bool,
    /// Where the player is now, from the current-scene file.
    current_scene: Option<SceneEntry>,
    files: Vec<FileView>,
    activity: Vec<Activity>,
    screenshots: Vec<Shot>,
    /// The last comparison of the game's skills with the site's.
    skills: state::SkillsCheck,
    /// Spoken notes, newest first, and how push-to-talk stands.
    notes: Vec<Note>,
    talk: Talk,
    /// Whether this build can transcribe (it was built with the `whisper` feature).
    has_whisper: bool,
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
    let presets = |list: Vec<(&'static str, PathBuf)>| {
        list.into_iter().map(|(label, path)| LuaFolder { label, exists: path.is_dir(), path }).collect()
    };
    let (watching_lua, watching_shots) = state.watching();
    let auto_uploading = watching_lua.is_some() && state.settings().auto_upload && {
        let c = lock(&state.connection);
        c.scopes.iter().any(|s| s == "upload") || (c.state == "offline" && has_token)
    };
    StateView {
        version: env!("CARGO_PKG_VERSION"),
        lua_folders: presets(settings::lua_folders()),
        screenshot_folders: presets(settings::screenshot_folders()),
        client_mismatch: settings::client_mismatch(&settings),
        has_token,
        connection: lock(&state.connection).clone(),
        watching_lua,
        watching_shots,
        auto_uploading,
        current_scene: lock(&state.timeline).current().cloned(),
        files: list_files(&state, &settings),
        activity: lock(&state.activity).iter().cloned().collect(),
        screenshots: lock(&state.shots).list.clone(),
        skills: lock(&state.skills).clone(),
        notes: lock(&state.notes).list.clone(),
        talk: lock(&state.talk).clone(),
        has_whisper: cfg!(feature = "whisper"),
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
    screenshot_folder: Option<String>,
    talk_key: String,
    whisper_model: Option<String>,
) -> Result<(), String> {
    let site_url = settings::check_site_url(&site_url)?;
    let lua_folder = folder_setting(lua_folder)?;
    let screenshot_folder = folder_setting(screenshot_folder)?;
    let whisper_model = file_setting(whisper_model)?;
    let talk_key = talk_key.trim().to_string();
    let site_changed = {
        let state = app.state::<AppState>();
        let mut settings = lock(&state.settings);
        let site_changed = settings.site() != site_url;
        *settings = Settings { lua_folder, site_url, auto_upload, screenshot_folder, talk_key, whisper_model };
        settings.save(&state.settings_path).map_err(|e| format!("Couldn't save the settings: {e}"))?;
        site_changed
    };
    notes::register_talk_key(&app);
    if site_changed {
        // A different site has its own token (or none), so check again.
        state::check_connection(app).await;
    } else {
        state::refresh_watchers(&app);
    }
    Ok(())
}

/// A folder typed in the settings: blank for none, or a folder that exists.
fn folder_setting(text: Option<String>) -> Result<Option<PathBuf>, String> {
    match text.map(|f| f.trim().to_string()).filter(|f| !f.is_empty()) {
        Some(folder) => {
            let path = PathBuf::from(folder);
            if path.is_dir() {
                Ok(Some(path))
            } else {
                Err(format!("There's no folder at {}", path.display()))
            }
        }
        None => Ok(None),
    }
}

/// A file typed in the settings: blank for none, or a file that exists.
fn file_setting(text: Option<String>) -> Result<Option<PathBuf>, String> {
    match text.map(|f| f.trim().trim_matches('"').to_string()).filter(|f| !f.is_empty()) {
        Some(file) => {
            let path = PathBuf::from(file);
            if path.is_file() {
                Ok(Some(path))
            } else {
                Err(format!("There's no file at {}", path.display()))
            }
        }
        None => Ok(None),
    }
}

/// Choose the site scene a spoken note belongs to (one from a search, or the one shown with it).
#[tauri::command]
pub fn set_note_scene(app: AppHandle, id: String, scene: SiteScene) -> Result<(), String> {
    notes::set_scene(&app, &id, scene)?;
    state::notify_ui(&app);
    Ok(())
}

/// Save Darren's corrections to a note's transcript.
#[tauri::command]
pub fn set_note_transcript(app: AppHandle, id: String, text: String) -> Result<(), String> {
    notes::set_transcript(&app, &id, text)?;
    state::notify_ui(&app);
    Ok(())
}

/// Delete a spoken note and its recording.
#[tauri::command]
pub fn delete_note(app: AppHandle, id: String) -> Result<(), String> {
    let result = notes::delete(&app, &id);
    state::notify_ui(&app);
    result
}

/// Transcribe every note not transcribed yet, in one batch in the background. Returns how many.
#[tauri::command]
pub fn transcribe_notes(app: AppHandle) -> Result<usize, String> {
    notes::transcribe_all(app)
}

/// Send: add a waiting screenshot to a scene's gallery. `scene` is the one shown with it, or one
/// Darren chose from a search. This is the only way a screenshot leaves the machine.
#[tauri::command]
pub fn send_screenshot(app: AppHandle, path: PathBuf, scene: SiteScene) -> Result<(), String> {
    screenshots::send(app, path, scene)
}

/// Skip: drop a screenshot from the waiting list without sending it. The file isn't touched.
#[tauri::command]
pub fn skip_screenshot(app: AppHandle, path: PathBuf) -> Result<(), String> {
    screenshots::skip(&app, &path)?;
    state::notify_ui(&app);
    Ok(())
}

/// Check skills: compare the game-skills file in the Lua folder with the site's skills. Only reads.
#[tauri::command]
pub async fn check_skills(app: AppHandle) {
    state::check_skills(app, None).await;
}

/// Searches the site's scenes by name, to choose one for a screenshot (`GET /scenes?q=`).
#[tauri::command]
pub async fn search_scenes(app: AppHandle, query: String) -> Result<Vec<SiteScene>, String> {
    let query = query.trim().to_string();
    if query.len() < 2 {
        return Err("Type at least two letters of the scene's name.".into());
    }
    let state = app.state::<AppState>();
    let site = state.settings().site().to_string();
    let token = secrets::get_token(&site)?.ok_or("Add an API token first.")?;
    Client::new(&site, &token).scenes("q", &query).await.map_err(|e| match e {
        crate::api::ApiError::Status { status: 403, message } => {
            format!("The site refused the search ({message}): the token needs the `read` scope.")
        }
        e => e.to_string(),
    })
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

/// Opens a link (an import to review, a scene page, an image) in the browser. Only web links:
/// nothing else goes to the shell.
#[tauri::command]
pub fn open_url(app: AppHandle, url: String) -> Result<(), String> {
    if settings::check_site_url(&url).is_err() {
        return Err(format!("Not a link to open: {url}"));
    }
    app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
}
