//! The app's shared state: settings, the connection to the site, the activity list and the watcher.
//!
//! Tauri keeps one `AppState` for the whole app (`app.manage(...)`), and any command or task can reach
//! it with `app.state::<AppState>()`. Each part sits behind a `Mutex`, Rust's lock for data shared
//! between threads: `.lock()` gives access, and the lock is let go at the end of the statement or block.
//! We never hold a lock across an `.await`, so a slow upload never blocks the UI.

use crate::api::{ApiError, Client};
use crate::history::History;
use crate::settings::Settings;
use crate::watcher::{self, FolderWatcher, Job};
use crate::secrets;
use serde::Serialize;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc::UnboundedSender;

/// How many lines the activity list keeps.
const ACTIVITY_LINES: usize = 100;

/// What `GET /me` last said, for the UI.
#[derive(Debug, Clone, Serialize)]
pub struct Connection {
    /// `unchecked`, `checking`, `no_token`, `ok`, `no_upload_scope`, `bad_token` or `offline`.
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

/// One line in the activity list: an upload and how it went.
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

pub struct AppState {
    pub settings_path: PathBuf,
    pub settings: Mutex<Settings>,
    pub history: Mutex<History>,
    pub connection: Mutex<Connection>,
    pub activity: Mutex<VecDeque<Activity>>,
    /// The running watcher, and the folder it watches. `None` when not watching.
    watcher: Mutex<Option<(PathBuf, FolderWatcher)>>,
    pub queue: UnboundedSender<Job>,
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
    pub fn new(settings_path: PathBuf, history_path: PathBuf, queue: UnboundedSender<Job>) -> AppState {
        AppState {
            settings: Mutex::new(Settings::load(&settings_path)),
            settings_path,
            history: Mutex::new(History::load(&history_path)),
            connection: Mutex::new(Connection::simple("unchecked", "")),
            activity: Mutex::new(VecDeque::new()),
            watcher: Mutex::new(None),
            queue,
            next_id: AtomicU64::new(1),
        }
    }

    pub fn settings(&self) -> Settings {
        lock(&self.settings).clone()
    }

    /// The folder being watched, if any.
    pub fn watching(&self) -> Option<PathBuf> {
        lock(&self.watcher).as_ref().map(|(folder, _)| folder.clone())
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

/// Tells the window something changed; it asks for the state again.
pub fn notify_ui(app: &AppHandle) {
    let _ = app.emit("changed", ());
}

/// Starts or stops the watcher to match the settings and the connection. We watch when there's a
/// folder, auto upload is on, there's a token, and the site hasn't said the token is bad or lacks
/// `upload`. (If the site can't be reached we still watch: uploads retry on their own.)
pub fn refresh_watcher(app: &AppHandle) {
    let state = app.state::<AppState>();
    let settings = state.settings();
    let connection = lock(&state.connection).state;
    let has_token = matches!(secrets::get_token(settings.site()), Ok(Some(_)));
    let wanted = settings
        .lua_folder
        .filter(|folder| folder.is_dir())
        .filter(|_| settings.auto_upload && has_token && !matches!(connection, "bad_token" | "no_upload_scope"));

    let mut current = lock(&state.watcher);
    if current.as_ref().map(|(folder, _)| folder) == wanted.as_ref() {
        return; // already as it should be
    }
    *current = None; // drops (stops) the old watcher, if any
    if let Some(folder) = wanted {
        match watcher::watch(&folder, state.queue.clone()) {
            Ok(w) => *current = Some((folder, w)),
            Err(e) => {
                state.add_activity("", "", "failed", e);
            }
        }
    }
    drop(current);
    notify_ui(app);
}

/// Asks the site who the token is (`GET /me`) and whether it may upload, then updates the watcher.
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
                Ok(me) if me.can_upload() => Connection {
                    state: "ok",
                    message: format!("Signed in as {}", me.account.name),
                    account: Some(me.account.name),
                    scopes: me.token.scopes,
                },
                Ok(me) => Connection {
                    state: "no_upload_scope",
                    message: format!(
                        "The token for {} doesn't have the upload scope. Make one that does at ADM → AI Access.",
                        me.account.name
                    ),
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
    refresh_watcher(&app);
    notify_ui(&app);
}
