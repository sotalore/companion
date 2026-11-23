//! The upload queue's worker. It takes one file at a time off the queue (from the watcher, or the
//! Upload button), reads it, checks its `type`, and uploads it if the site takes that type.
//!
//! One worker, one file at a time, waiting (not spinning) for the next: the game comes first.

use crate::api::{Client, RETRY_DELAYS};
use crate::exports::{self, Header, Kind};
use crate::history::{self, Record};
use crate::secrets;
use crate::state::{lock, notify_ui, now, AppState};
use crate::watcher::Job;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tauri::{AppHandle, Manager};
use tokio::sync::mpsc::UnboundedReceiver;

/// How many times to read a file that isn't valid JSON yet (we may have caught it half-written), and
/// how long to wait between reads.
const READ_TRIES: u32 = 5;
const READ_WAIT: Duration = Duration::from_secs(2);

/// Starts the worker. It runs until the app quits.
pub fn start(app: AppHandle, mut queue: UnboundedReceiver<Job>) {
    tauri::async_runtime::spawn(async move {
        // The content hash each path had when we last looked at it, so an unchanged file the watcher
        // reports again (or one we skipped or that failed) isn't handled twice.
        let mut last_seen: HashMap<PathBuf, String> = HashMap::new();
        // `recv` waits without using CPU until a job arrives, and returns `None` when the app quits.
        while let Some(job) = queue.recv().await {
            process(&app, job, &mut last_seen).await;
            notify_ui(&app);
        }
    });
}

async fn process(app: &AppHandle, job: Job, last_seen: &mut HashMap<PathBuf, String>) {
    let state = app.state::<AppState>();
    let file = file_name(&job.path);

    let (bytes, header) = match read_complete(&job.path).await {
        Ok(Some(read)) => read,
        Ok(None) => {
            // The file went away before we read it. Only worth saying if the user asked for it.
            if job.manual {
                state.add_activity(&file, "", "failed", "The file isn't there any more.");
            }
            return;
        }
        Err(e) => {
            state.add_activity(&file, "", "skipped", e);
            return;
        }
    };

    let hash = history::sha256(&bytes);
    if !job.manual && last_seen.get(&job.path) == Some(&hash) {
        return; // the same bytes as last time
    }
    last_seen.insert(job.path.clone(), hash.clone());

    let label = exports::describe(&header);
    let (file_type, endpoint) = match exports::classify(&header) {
        Kind::Upload { file_type, endpoint } => (file_type, endpoint),
        Kind::NotUploaded { file_type } => {
            // gameSkills, skills, the current-scene file...: normal, so only mention it when asked.
            if job.manual {
                state.add_activity(&file, &label, "skipped", format!("The site doesn't take `{file_type}` files."));
            }
            return;
        }
        Kind::NoType => {
            state.add_activity(
                &file,
                &label,
                "skipped",
                "No `type` in the file: it's from a plugin older than 0.7.1, which the site refuses.",
            );
            return;
        }
    };

    let settings = state.settings();
    let site = settings.site().to_string();
    if !job.manual && lock(&state.history).find(&hash, &site).is_some() {
        // Uploaded before, byte for byte (maybe before a restart): don't send it again.
        return;
    }
    let token = match secrets::get_token(&site) {
        Ok(Some(token)) => token,
        Ok(None) => {
            state.add_activity(&file, &label, "failed", "No API token: add one in the settings.");
            return;
        }
        Err(e) => {
            state.add_activity(&file, &label, "failed", e);
            return;
        }
    };

    let id = state.add_activity(&file, &label, "uploading", format!("Uploading to {site}..."));
    notify_ui(app);

    let client = Client::new(&site, &token);
    let result = client
        .upload(endpoint, &file, &bytes, RETRY_DELAYS, |err, wait| {
            state.update_activity(id, |line| {
                line.status = "retrying";
                line.message = format!("{err}. Trying again in {} s.", wait.as_secs());
            });
            notify_ui(app);
        })
        .await;

    match result {
        Ok(uploaded) => {
            let what = if uploaded.summary.is_empty() { String::new() } else { format!(" ({})", uploaded.summary) };
            state.update_activity(id, |line| {
                line.at = now();
                if uploaded.created {
                    line.status = "uploaded";
                    line.message = format!("Uploaded{what}: an admin reviews it on the site.");
                } else {
                    line.status = "already";
                    line.message = format!("The site already had this exact file{what}; nothing new was made.");
                }
                line.review_url = uploaded.review_url.clone();
            });
            lock(&state.history).add(
                hash,
                Record {
                    file: file.clone(),
                    file_type,
                    site,
                    at: now(),
                    created: uploaded.created,
                    import_id: uploaded.import_id,
                    review_url: uploaded.review_url,
                },
            );
        }
        Err(err) => {
            let message = if err.is_retryable() {
                format!("{err}. Gave up after {} retries; press Upload to try again.", RETRY_DELAYS.len())
            } else {
                // A 4xx: the file or the token is the problem, so it isn't retried.
                format!("{err}")
            };
            state.update_activity(id, |line| {
                line.at = now();
                line.status = "failed";
                line.message = message;
            });
        }
    }
}

/// Reads a whole file and its header. Tries again a few times if it isn't valid JSON yet or is locked
/// by the game. `Ok(None)`: the file isn't there.
async fn read_complete(path: &Path) -> Result<Option<(Vec<u8>, Header)>, String> {
    let mut problem = String::new();
    for attempt in 1..=READ_TRIES {
        match std::fs::read(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => problem = format!("Couldn't read the file: {e}"),
            Ok(bytes) => match exports::read_header(&bytes) {
                Ok(header) => return Ok(Some((bytes, header))),
                Err(e) => problem = format!("The file isn't valid JSON ({e}), so it wasn't uploaded."),
            },
        }
        if attempt < READ_TRIES {
            tokio::time::sleep(READ_WAIT).await;
        }
    }
    Err(problem)
}

pub fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}
