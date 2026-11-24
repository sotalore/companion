//! The app's shared state: settings, the connection to the site, the scene timeline, the activity and
//! screenshot lists, and the two folder watchers.
//!
//! Tauri keeps one `AppState` for the whole app (`app.manage(...)`), and any command or task can reach
//! it with `app.state::<AppState>()`. Each part sits behind a `Mutex`, Rust's lock for data shared
//! between threads: `.lock()` gives access, and the lock is let go at the end of the statement or block.
//! We never hold a lock across an `.await`, so a slow upload never blocks the UI.

use crate::api::{ApiError, Client, SiteScene};
use crate::exports;
use crate::history::History;
use crate::notes::{Notes, Talk};
use crate::recorder::Recorder;
use crate::scenes::{self, Timeline};
use crate::screenshots::{self, Shots};
use crate::secrets;
use crate::settings::Settings;
use crate::skills;
use crate::watcher::{self, FolderWatcher, Job};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc::UnboundedSender;

/// How many lines the activity list keeps.
const ACTIVITY_LINES: usize = 100;

/// What `GET /me` last said, for the UI.
#[derive(Debug, Clone, Serialize)]
pub struct Connection {
    /// `unchecked`, `checking`, `no_token`, `ok`, `bad_token` or `offline`.
    pub state: &'static str,
    pub message: String,
    pub account: Option<String>,
    pub scopes: Vec<String>,
}

impl Connection {
    fn simple(state: &'static str, message: impl Into<String>) -> Connection {
        Connection { state, message: message.into(), account: None, scopes: vec![] }
    }
}

/// One line in the activity list: an export upload and how it went.
#[derive(Debug, Clone, Serialize)]
pub struct Activity {
    pub id: u64,
    pub at: String,
    pub file: String,
    /// The file's type (and scope), e.g. `recipes (all)`.
    pub label: String,
    /// `uploading`, `retrying`, `uploaded`, `already`, `failed` or `skipped`.
    pub status: &'static str,
    pub message: String,
    pub review_url: Option<String>,
}

/// A running watcher and the folder it watches.
type Watching = Option<(PathBuf, FolderWatcher)>;

pub struct AppState {
    pub settings_path: PathBuf,
    pub settings: Mutex<Settings>,
    pub history: Mutex<History>,
    pub connection: Mutex<Connection>,
    pub activity: Mutex<VecDeque<Activity>>,
    /// The scene changes seen while the app runs.
    pub timeline: Mutex<Timeline>,
    pub shots: Mutex<Shots>,
    /// Site scenes found by game name (`None`: the site has none with that name). Like a Ruby Hash
    /// used as a cache.
    pub site_scenes: Mutex<HashMap<String, Option<SiteScene>>>,
    /// The last comparison of the game's skills with the site's.
    pub skills: Mutex<SkillsCheck>,
    lua_watcher: Mutex<Watching>,
    shots_watcher: Mutex<Watching>,
    /// Export files for the upload worker.
    pub queue: UnboundedSender<Job>,
    /// New screenshots for the screenshot worker.
    pub shots_queue: UnboundedSender<PathBuf>,
    /// The spoken notes, and push-to-talk: the key, whether it's recording, and the recording thread.
    pub notes: Mutex<Notes>,
    pub talk: Mutex<Talk>,
    pub recorder: Recorder,
    next_id: AtomicU64,
}

/// A short way to lock a mutex. A lock is only "poisoned" if a thread panicked while holding it;
/// the data is still usable, so take it anyway rather than crash the app.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn now() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

impl AppState {
    pub fn new(
        settings_path: PathBuf,
        data_dir: &Path,
        queue: UnboundedSender<Job>,
        shots_queue: UnboundedSender<PathBuf>,
        recorder: Recorder,
    ) -> AppState {
        AppState {
            settings: Mutex::new(Settings::load(&settings_path)),
            settings_path,
            history: Mutex::new(History::load(&data_dir.join("uploads.json"))),
            connection: Mutex::new(Connection::simple("unchecked", "")),
            activity: Mutex::new(VecDeque::new()),
            timeline: Mutex::new(Timeline::default()),
            shots: Mutex::new(Shots::load(&data_dir.join("screenshots.json"))),
            site_scenes: Mutex::new(HashMap::new()),
            skills: Mutex::new(SkillsCheck { status: "unchecked", message: String::new(), report: None }),
            lua_watcher: Mutex::new(None),
            shots_watcher: Mutex::new(None),
            queue,
            shots_queue,
            notes: Mutex::new(Notes::load(&data_dir.join("notes"))),
            talk: Mutex::new(Talk { key: None, message: String::new(), recording: false, problem: None }),
            recorder,
            next_id: AtomicU64::new(1),
        }
    }

    pub fn settings(&self) -> Settings {
        lock(&self.settings).clone()
    }

    /// Whether the token had `scope` when last checked.
    pub fn has_scope(&self, scope: &str) -> bool {
        lock(&self.connection).scopes.iter().any(|s| s == scope)
    }

    /// Whether a new export should be uploaded without asking: auto upload is on, and the token has
    /// `upload` (or the site couldn't be reached to check, and uploads will retry).
    fn may_auto_upload(&self) -> bool {
        let settings = self.settings();
        let connection = lock(&self.connection);
        let token_ok = connection.scopes.iter().any(|s| s == "upload")
            || (connection.state == "offline" && matches!(secrets::get_token(settings.site()), Ok(Some(_))));
        settings.auto_upload && token_ok
    }

    /// The folders being watched, for the UI: (Lua folder, screenshot folder).
    pub fn watching(&self) -> (Option<PathBuf>, Option<PathBuf>) {
        let folder = |w: &Mutex<Watching>| lock(w).as_ref().map(|(folder, _)| folder.clone());
        (folder(&self.lua_watcher), folder(&self.shots_watcher))
    }

    /// Adds a line to the top of the activity list and returns its id, for `update_activity`.
    pub fn add_activity(&self, file: &str, label: &str, status: &'static str, message: impl Into<String>) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut list = lock(&self.activity);
        list.push_front(Activity {
            id,
            at: now(),
            file: file.to_string(),
            label: label.to_string(),
            status,
            message: message.into(),
            review_url: None,
        });
        list.truncate(ACTIVITY_LINES);
        id
    }

    /// Changes a line in the activity list. `change` is like a Ruby block given the line to edit.
    pub fn update_activity(&self, id: u64, change: impl FnOnce(&mut Activity)) {
        if let Some(line) = lock(&self.activity).iter_mut().find(|a| a.id == id) {
            change(line);
        }
    }
}

/// Tells the window something changed; it asks for the state again. Also brings the window's title
/// up to date. (Callers must not be holding the screenshots, notes, skills or talk lock.)
pub fn notify_ui(app: &AppHandle) {
    update_title(app);
    let _ = app.emit("changed", ());
}

/// The window's title says what waits on Darren and whether a note is being recorded. It shows on
/// the taskbar button, so he sees it with the window minimized or behind the game.
fn update_title(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else { return };
    let Some(window) = app.get_webview_window("main") else { return };
    let waiting = Waiting {
        screenshots: lock(&state.shots).waiting(),
        notes: lock(&state.notes).list.iter().filter(|n| matches!(n.status.as_str(), "recorded" | "failed")).count(),
        skills_out_of_step: lock(&state.skills).report.as_ref().is_some_and(|r| !r.in_step),
        recording: lock(&state.talk).recording,
    };
    let title = window_title(&waiting);
    if window.title().ok().as_deref() != Some(title.as_str()) {
        let _ = window.set_title(&title);
    }
}

/// What the title counts.
struct Waiting {
    screenshots: usize,
    notes: usize,
    skills_out_of_step: bool,
    recording: bool,
}

/// "SotA Lore Companion" when nothing waits; otherwise a short list that fits a taskbar button:
/// "Companion: 2 screenshots, 3 notes, skills". Recording comes first, so it's never cut off.
fn window_title(w: &Waiting) -> String {
    let plural = |n: usize, one: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {one}s") };
    let mut parts = vec![];
    if w.screenshots > 0 {
        parts.push(plural(w.screenshots, "screenshot"));
    }
    if w.notes > 0 {
        parts.push(plural(w.notes, "note"));
    }
    if w.skills_out_of_step {
        parts.push("skills".to_string());
    }
    let title = if parts.is_empty() { "SotA Lore Companion".to_string() } else { format!("Companion: {}", parts.join(", ")) };
    if w.recording {
        format!("● Recording · {title}")
    } else {
        title
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn waiting(screenshots: usize, notes: usize, skills_out_of_step: bool, recording: bool) -> Waiting {
        Waiting { screenshots, notes, skills_out_of_step, recording }
    }

    #[test]
    fn the_title_lists_what_waits() {
        assert_eq!(window_title(&waiting(0, 0, false, false)), "SotA Lore Companion");
        assert_eq!(window_title(&waiting(2, 3, true, false)), "Companion: 2 screenshots, 3 notes, skills");
        assert_eq!(window_title(&waiting(1, 1, false, false)), "Companion: 1 screenshot, 1 note");
        assert_eq!(window_title(&waiting(0, 0, false, true)), "● Recording · SotA Lore Companion");
    }
}

/// Starts or stops the two watchers to match the settings:
/// - the `Lua/` folder whenever it's set: scene changes are always recorded (they cost nothing to
///   keep), and exports go to the upload queue when `may_auto_upload` says so;
/// - the screenshot folder whenever it's set: new screenshots only wait in the list, nothing is sent.
pub fn refresh_watchers(app: &AppHandle) {
    let state = app.state::<AppState>();
    let settings = state.settings();

    let lua = settings.lua_folder.clone().filter(|f| f.is_dir());
    let handle = app.clone();
    restart(&state, &state.lua_watcher, lua, exports::is_candidate, move |files| {
        lua_files_changed(&handle, files)
    });
    if let Some(folder) = &settings.lua_folder {
        seed_scene(app, folder);
    }

    let shots = settings.screenshot_folder.clone().filter(|f| f.is_dir());
    let queue = state.shots_queue.clone();
    restart(&state, &state.shots_watcher, shots, screenshots::is_image, move |files| {
        for f in files {
            let _ = queue.send(f);
        }
    });
    notify_ui(app);
}

/// Replaces a watcher if its folder changed.
fn restart(
    state: &AppState,
    slot: &Mutex<Watching>,
    wanted: Option<PathBuf>,
    filter: fn(&Path) -> bool,
    on_files: impl FnMut(Vec<PathBuf>) + Send + 'static,
) {
    let mut current = lock(slot);
    if current.as_ref().map(|(folder, _)| folder) == wanted.as_ref() {
        return; // already as it should be
    }
    *current = None; // drops (stops) the old watcher, if any
    if let Some(folder) = wanted {
        match watcher::watch(&folder, filter, on_files) {
            Ok(w) => *current = Some((folder, w)),
            Err(e) => {
                state.add_activity("", "", "failed", e);
            }
        }
    }
}

/// Files changed in the `Lua/` folder (on the watcher's thread), sent on by their `type`:
/// - `currentScene`: recorded at once, here, so a screenshot is never matched before its scene
///   change is known;
/// - `gameSkills`: compared with the site's skills (a read, in the background);
/// - anything else: to the upload queue, whose worker can be busy retrying for a minute. It checks
///   the type again and uploads only the types the site takes.
fn lua_files_changed(app: &AppHandle, files: Vec<PathBuf>) {
    let state = app.state::<AppState>();
    for path in files {
        match read_typed(&path) {
            Some((t, bytes)) if t == "currentScene" => {
                if let Some(entry) = scenes::parse(&bytes) {
                    if lock(&state.timeline).record(entry) {
                        notify_ui(app);
                    }
                }
            }
            Some((t, _)) if t == "gameSkills" => {
                tauri::async_runtime::spawn(check_skills(app.clone(), Some(path)));
            }
            _ => {
                if state.may_auto_upload() {
                    let _ = state.queue.send(Job { path, manual: false });
                }
            }
        }
    }
}

/// Reads a file and its `type`, trying again briefly if it's caught half-written. `None` if it has
/// no type, or still isn't valid JSON (the upload worker will say so if it's an export).
fn read_typed(path: &Path) -> Option<(String, Vec<u8>)> {
    for _ in 0..3 {
        let bytes = std::fs::read(path).ok()?;
        match exports::read_header(&bytes) {
            Ok(header) => return header.file_type.map(|t| (t, bytes)),
            Err(_) => std::thread::sleep(Duration::from_millis(300)), // maybe half-written
        }
    }
    None
}

/// The file of a given `type` in the Lua folder, if there is one (found by type, never by name).
fn find_typed(folder: &Path, file_type: &str) -> Option<(PathBuf, Vec<u8>)> {
    let entries = std::fs::read_dir(folder).ok()?;
    entries.flatten().map(|e| e.path()).filter(|p| exports::is_candidate(p)).find_map(|path| {
        let (t, bytes) = read_typed(&path)?;
        (t == file_type).then_some((path, bytes))
    })
}

/// On start (or a new Lua folder), records the scene the player is in now, from the file as it is.
fn seed_scene(app: &AppHandle, folder: &Path) {
    if let Some(entry) = find_typed(folder, "currentScene").and_then(|(_, bytes)| scenes::parse(&bytes)) {
        lock(&app.state::<AppState>().timeline).record(entry);
    }
}

/// The skills check as the window shows it.
#[derive(Debug, Clone, Serialize)]
pub struct SkillsCheck {
    /// `unchecked`, `checking`, `done` or `failed`.
    pub status: &'static str,
    pub message: String,
    pub report: Option<skills::Report>,
}

/// Compares the game-skills file with the site's skills (`GET /skills`) and keeps the report for the
/// window. `path`: the file that just changed; `None` looks for it in the Lua folder. Only reads.
pub async fn check_skills(app: AppHandle, path: Option<PathBuf>) {
    let state = app.state::<AppState>();
    let set = |status, message: String, report| {
        *lock(&state.skills) = SkillsCheck { status, message, report };
        notify_ui(&app);
    };
    let settings = state.settings();
    let found = match path {
        Some(path) => std::fs::read(&path).ok().map(|bytes| (path, bytes)),
        None => settings.lua_folder.as_deref().and_then(|folder| find_typed(folder, "gameSkills")),
    };
    let Some((path, bytes)) = found else {
        return set("unchecked", "No game-skills file in the Lua folder yet: run /sladmin gameskills in game.".into(), None);
    };
    let Some(file) = skills::parse_file(&bytes) else {
        return set("failed", format!("{} isn't a complete game-skills file.", path.display()), None);
    };
    let site = settings.site().to_string();
    let Ok(Some(token)) = secrets::get_token(&site) else {
        return set("unchecked", "Add an API token (with the read scope) to compare with the site.".into(), None);
    };
    // Keep the last report on screen while checking again.
    let previous = lock(&state.skills).report.clone();
    set("checking", "Comparing with the site...".into(), previous);
    match Client::new(&site, &token).skills().await {
        Ok(site_skills) => {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let report = skills::compare(&file, &name, &site_skills, &now());
            let message = if report.in_step {
                format!("In step: all {} skills in the game match the site.", report.game_count)
            } else {
                "The site's skills differ from the game's: see below.".to_string()
            };
            set("done", message, Some(report));
        }
        Err(ApiError::Status { status: 403, message }) => {
            set("failed", format!("The site refused ({message}): the token needs the `read` scope."), None)
        }
        Err(e) => set("failed", format!("Couldn't get the site's skills: {e}"), None),
    }
}

/// Asks the site who the token is (`GET /me`) and which scopes it has.
pub async fn check_connection(app: AppHandle) {
    let state = app.state::<AppState>();
    let site = state.settings().site().to_string();
    let result = match secrets::get_token(&site) {
        Err(e) => Connection::simple("bad_token", e),
        Ok(None) => Connection::simple("no_token", "No API token yet: paste one below."),
        Ok(Some(token)) => {
            *lock(&state.connection) = Connection::simple("checking", "Checking the token...");
            notify_ui(&app);
            match Client::new(&site, &token).me().await {
                Ok(me) => Connection {
                    state: "ok",
                    message: format!("Signed in as {}", me.account.name),
                    account: Some(me.account.name),
                    scopes: me.token.scopes,
                },
                Err(err @ ApiError::Network(_)) => Connection::simple("offline", err.to_string()),
                Err(ApiError::Status { status: 401, message }) => {
                    Connection::simple("bad_token", format!("The site refused the token: {message}"))
                }
                Err(err) => Connection::simple("offline", format!("The site answered {err}")),
            }
        }
    };
    *lock(&state.connection) = result;
    // A new token or site: forget scene lookups, and try again for screenshots still without one.
    lock(&state.site_scenes).clear();
    retry_scene_lookups(&app).await;
    refresh_watchers(&app);
    notify_ui(&app);
    // With a working token, see whether the site's skills are in step with the game's file.
    if lock(&state.connection).state == "ok" {
        check_skills(app.clone(), None).await;
    }
}

/// Looks up the site scene again for waiting screenshots that don't have one.
async fn retry_scene_lookups(app: &AppHandle) {
    let state = app.state::<AppState>();
    let todo: Vec<screenshots::Shot> = lock(&state.shots)
        .list
        .iter()
        .filter(|s| s.status == "waiting" && s.scene.is_some() && s.site_scene.is_none())
        .cloned()
        .collect();
    for mut shot in todo {
        screenshots::look_up_site_scene(app, &mut shot).await;
        let mut shots = lock(&state.shots);
        if let Some(listed) = shots.find_mut(&shot.path) {
            listed.site_scene = shot.site_scene;
            listed.site_note = shot.site_note;
        }
        shots.save();
    }
    // The same for spoken notes.
    let todo: Vec<(String, String)> = lock(&state.notes)
        .list
        .iter()
        .filter(|n| n.site_scene.is_none())
        .filter_map(|n| Some((n.id.clone(), n.scene.as_ref()?.raw_name.clone()?)))
        .collect();
    for (id, raw) in todo {
        let found = screenshots::find_site_scene(app, &raw).await;
        let mut notes = lock(&state.notes);
        if let Some(note) = notes.find_mut(&id).filter(|n| n.site_scene.is_none()) {
            match found {
                Ok(scene) => {
                    note.site_scene = Some(scene);
                    note.site_note.clear();
                }
                Err(why) => note.site_note = why,
            }
        }
        notes.save();
    }
}
