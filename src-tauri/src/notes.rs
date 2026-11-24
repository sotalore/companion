//! Spoken notes: Darren holds the talk key while playing and says something about the scene he's in.
//!
//! Each recording is saved as a WAV file in the app's data folder (`notes\`), labelled with the scene
//! from the current-scene file (as screenshots are), and listed in the window. **The audio never
//! leaves peregrin.** Later, on Darren's command and never during play, the notes are transcribed in
//! one batch (`transcribe.rs`). He reads and corrects each transcript, then presses Upload, which
//! adds the text (never the audio) to the scene's private notes on the site (`POST /scenes/:id/notes`).
//! **Transcripts are private**: they never go into the activity list, a log line or an issue.

use crate::api::{ApiError, Client, NewNote, SiteScene, RETRY_DELAYS};
use crate::secrets;
use crate::recorder::{self, Clip, Recorder};
use crate::scenes::{SceneAt, SceneEntry};
use crate::screenshots;
use crate::state::{lock, notify_ui, now, AppState};
use crate::transcribe;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

/// Shorter than this is a tap of the key by mistake, not a note: it's dropped.
const MIN_SECONDS: f32 = 0.5;
/// How long after a note starts we wait before matching its scene, so a scene change written at
/// about the same moment is recorded first (as for screenshots).
const SETTLE: Duration = Duration::from_secs(3);

/// One spoken note.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    /// Its id in the list, from when it was recorded: `2026-11-23_16-40-12`.
    pub id: String,
    /// The WAV file, in the notes folder.
    pub path: PathBuf,
    /// When the key was pressed, local, ISO 8601 with the offset.
    pub recorded_at: String,
    pub seconds: f32,
    pub device: String,
    /// The scene the player was in, from the current-scene file, or `None` (see `scene_note`).
    pub scene: Option<SceneEntry>,
    pub scene_note: String,
    /// The site's scene it belongs to: found by the game name, or chosen by Darren.
    pub site_scene: Option<SiteScene>,
    pub site_note: String,
    /// `recorded` (not transcribed yet), `queued`, `transcribing`, `transcribed`, `failed` (to
    /// transcribe), then `uploading`, `retrying`, `uploaded` or `upload_failed`.
    pub status: String,
    pub message: String,
    /// What Whisper heard, then as Darren corrected it.
    pub transcript: Option<String>,
    /// The note's id for the site, made when it was recorded and sent with every try, so a retry
    /// never adds it twice. (`default`: notes recorded before uploads existed get one on loading.)
    #[serde(default)]
    pub client_id: String,
    /// What made the text, e.g. `whisper-small.en`, from the model file's name.
    #[serde(default)]
    pub transcriber: Option<String>,
    /// Once uploaded: the scene's page on the site, where root sees its notes.
    #[serde(default)]
    pub uploaded_url: Option<String>,
}

impl Note {
    /// Whether Darren has something to do with it: transcribe it, or read it through and upload it.
    pub fn waits_on_darren(&self) -> bool {
        matches!(self.status.as_str(), "recorded" | "failed" | "transcribed" | "upload_failed")
    }

    /// Once a note is on the site (or on its way), its text and scene stay as sent: the site keeps
    /// the first version of a `client_id` and refuses another.
    pub fn is_sent(&self) -> bool {
        matches!(self.status.as_str(), "uploading" | "retrying" | "uploaded")
    }
}

/// The site takes at most this much text in a note (`docs/api.md`).
const MAX_TEXT: usize = 20_000;

/// A new `client_id`: a hash of the moment recording started, this process and a counter. Unique
/// for Darren's account without a random-number library (like `SecureRandom.hex(16)` in spirit).
fn new_client_id(started: SystemTime) -> String {
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = started.duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let seed = format!("{nanos}-{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed));
    hex::encode(&Sha256::digest(seed.as_bytes())[..16])
}

/// The notes, newest first, kept in `notes\notes.json` beside the recordings.
pub struct Notes {
    pub folder: PathBuf,
    pub list: Vec<Note>,
}

impl Notes {
    pub fn load(folder: &Path) -> Notes {
        let list: Vec<Note> = std::fs::read_to_string(folder.join("notes.json"))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        // Forget notes whose recording is gone, and a batch cut off by a quit.
        let list = list
            .into_iter()
            .filter(|n| n.path.is_file())
            .map(|mut n| {
                if n.status == "queued" || n.status == "transcribing" {
                    n.status = "recorded".into();
                    n.message = "The app quit before this was transcribed.".into();
                }
                if n.status == "uploading" || n.status == "retrying" {
                    n.status = "upload_failed".into();
                    n.message = "The app quit while uploading; press Upload again (it won't be added twice).".into();
                }
                if n.client_id.is_empty() {
                    n.client_id = new_client_id(SystemTime::now());
                }
                n
            })
            .collect();
        Notes { folder: folder.to_path_buf(), list }
    }

    pub fn save(&self) {
        let result = (|| {
            std::fs::create_dir_all(&self.folder)?;
            std::fs::write(self.folder.join("notes.json"), serde_json::to_string_pretty(&self.list)?)
        })();
        if let Err(e) = result {
            eprintln!("couldn't save the notes list: {e}");
        }
    }

    pub fn find_mut(&mut self, id: &str) -> Option<&mut Note> {
        self.list.iter_mut().find(|n| n.id == id)
    }

    /// A file name not taken yet for a note recorded at `stamp`.
    fn free_id(&self, stamp: &str) -> String {
        let mut id = stamp.to_string();
        let mut n = 2;
        while self.list.iter().any(|note| note.id == id) || self.folder.join(format!("{id}.wav")).exists() {
            id = format!("{stamp}_{n}");
            n += 1;
        }
        id
    }
}

/// The push-to-talk state the window shows.
#[derive(Debug, Clone, Serialize)]
pub struct Talk {
    /// The key registered now, if any.
    pub key: Option<String>,
    /// How push-to-talk stands: "Hold F9 to record", or why it can't.
    pub message: String,
    pub recording: bool,
    /// Why the last recording failed (no microphone...), until the next one works.
    pub problem: Option<String>,
}

/// Starts the recording thread. Recordings arrive in `saved`, on that thread.
pub fn start_recorder(app: &AppHandle) -> Recorder {
    let started = app.clone();
    let done = app.clone();
    Recorder::start_thread(
        move |result| {
            let state = started.state::<AppState>();
            match result {
                Ok(_) => {
                    let mut talk = lock(&state.talk);
                    talk.recording = true; // shown in the window and its title
                    talk.problem = None;
                }
                Err(e) => lock(&state.talk).problem = Some(e),
            }
            notify_ui(&started);
        },
        move |result| {
            let state = done.state::<AppState>();
            lock(&state.talk).recording = false;
            match result {
                Ok(clip) => saved(&done, clip),
                Err(e) => lock(&state.talk).problem = Some(e),
            }
            notify_ui(&done);
        },
    )
}

/// Registers the talk key from the settings (and lets go of the old one). Called at start and when
/// the settings are saved.
pub fn register_talk_key(app: &AppHandle) {
    let state = app.state::<AppState>();
    let wanted = state.settings().talk_key.trim().to_string();
    let shortcuts = app.global_shortcut();
    let mut talk = lock(&state.talk);
    if talk.key.as_deref() == Some(wanted.as_str()) {
        return;
    }
    if let Some(old) = talk.key.take() {
        let _ = shortcuts.unregister(old.as_str());
    }
    if wanted.is_empty() {
        talk.message = "Push-to-talk is off: choose a key in the settings.".into();
        return;
    }
    // A name the library can't read is a typo, not a key taken by another program: say which.
    if wanted.parse::<tauri_plugin_global_shortcut::Shortcut>().is_err() {
        talk.message = format!("\"{wanted}\" isn't a key name. Try F9, Ctrl+F9, Numpad0 or ScrollLock.");
        return;
    }
    // The handler runs on the app's main thread for each press and release. It only sends an order
    // to the recording thread, so it returns at once.
    let result = shortcuts.on_shortcut(wanted.as_str(), |app, _shortcut, event| {
        let state = app.state::<AppState>();
        match event.state() {
            ShortcutState::Pressed => state.recorder.start(),
            ShortcutState::Released => state.recorder.stop(),
        }
    });
    match result {
        Ok(()) => {
            talk.message = format!("Hold {wanted} while you speak, in game or anywhere else.");
            talk.key = Some(wanted);
        }
        Err(e) => {
            talk.message = format!(
                "Couldn't use {wanted} for push-to-talk ({e}). Another program probably has it; choose another key."
            );
        }
    }
}

/// A recording has finished (on the recording thread): save it, then match its scene.
fn saved(app: &AppHandle, clip: Clip) {
    let state = app.state::<AppState>();
    if clip.seconds() < MIN_SECONDS {
        return; // a tap of the key, not a note
    }
    let started = chrono::DateTime::<chrono::Local>::from(clip.started);
    let mut notes = lock(&state.notes);
    let id = notes.free_id(&started.format("%Y-%m-%d_%H-%M-%S").to_string());
    let path = notes.folder.join(format!("{id}.wav"));
    if let Err(e) = std::fs::create_dir_all(&notes.folder).map_err(|e| e.to_string()).and_then(|_| recorder::write_wav(&path, &clip.samples)) {
        drop(notes);
        lock(&state.talk).problem = Some(format!("Couldn't save the note: {e}"));
        return;
    }
    notes.list.insert(
        0,
        Note {
            id: id.clone(),
            path: path.clone(),
            recorded_at: started.to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
            seconds: clip.seconds(),
            device: clip.device.clone(),
            scene: None,
            scene_note: "Finding the scene...".into(),
            site_scene: None,
            site_note: String::new(),
            status: "recorded".into(),
            message: String::new(),
            transcript: None,
            client_id: new_client_id(clip.started),
            transcriber: None,
            uploaded_url: None,
        },
    );
    notes.save();
    drop(notes);
    // Let the window play it back (it may read only files allowed this way).
    let _ = app.asset_protocol_scope().allow_file(&path);

    let app = app.clone();
    let started_unix = clip.started.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let wait = SETTLE.saturating_sub(SystemTime::now().duration_since(clip.started).unwrap_or_default());
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(wait).await;
        match_scene(&app, &id, started_unix).await;
        notify_ui(&app);
    });
}

/// Labels a note with the scene the player was in when it started, and finds that scene on the site.
async fn match_scene(app: &AppHandle, id: &str, started_unix: i64) {
    let state = app.state::<AppState>();
    let at = lock(&state.timeline).at(started_unix);
    let (scene, scene_note) = describe_scene(at);
    let site = match scene.as_ref().and_then(|s| s.raw_name.clone()) {
        Some(raw) => Some(screenshots::find_site_scene(app, &raw).await),
        None => None,
    };
    let mut notes = lock(&state.notes);
    if let Some(note) = notes.find_mut(id) {
        note.scene = scene;
        note.scene_note = scene_note;
        match site {
            Some(Ok(found)) => note.site_scene = Some(found),
            Some(Err(why)) => note.site_note = why,
            None => note.site_note = "Choose its scene by name.".into(),
        }
    }
    notes.save();
}

/// The scene for a moment, and a remark for the window (as for screenshots).
fn describe_scene(at: SceneAt) -> (Option<SceneEntry>, String) {
    match at {
        SceneAt::Scene(entry) => {
            let note = if let Some(owner) = entry.dungeon_owner.as_ref() {
                let name = entry.dungeon_name.as_deref().unwrap_or("an unnamed dungeon");
                format!("In {owner}'s player dungeon or basement ({name}).")
            } else if !entry.exact {
                "The plugin started in this scene, so it may have been entered earlier.".into()
            } else {
                String::new()
            };
            (Some(entry), note)
        }
        SceneAt::NoScene(state_name) => (
            None,
            match state_name.as_str() {
                "loading" => "Recorded while a scene was loading: it belongs to no scene.".into(),
                "loggedOut" => "Recorded at the login screen: it belongs to no scene.".into(),
                other => format!("Recorded in game state `{other}`: no scene."),
            },
        ),
        SceneAt::Unknown => (
            None,
            "No scene known for that time: sl-admin 0.16.0 or later writes it, while the companion runs.".into(),
        ),
    }
}

/// Darren chose the site scene a note belongs to.
pub fn set_scene(app: &AppHandle, id: &str, scene: SiteScene) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut notes = lock(&state.notes);
    let note = notes.find_mut(id).ok_or("That note isn't in the list any more.")?;
    if note.is_sent() {
        return Err("It's uploaded to its scene already.".into());
    }
    note.site_scene = Some(scene);
    note.site_note.clear();
    notes.save();
    Ok(())
}

/// Darren corrected a transcript.
pub fn set_transcript(app: &AppHandle, id: &str, text: String) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut notes = lock(&state.notes);
    let note = notes.find_mut(id).ok_or("That note isn't in the list any more.")?;
    if note.status == "transcribing" {
        return Err("It's being transcribed.".into());
    }
    if note.is_sent() {
        return Err("It's uploaded already, so its text stays as sent.".into());
    }
    note.transcript = Some(text);
    notes.save();
    Ok(())
}

/// Darren deleted a note: its recording goes too (it's the app's own file, in its data folder).
pub fn delete(app: &AppHandle, id: &str) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut notes = lock(&state.notes);
    let at = notes.list.iter().position(|n| n.id == id).ok_or("That note isn't in the list any more.")?;
    if matches!(notes.list[at].status.as_str(), "queued" | "transcribing") {
        return Err("It's being transcribed; delete it when the batch is done.".into());
    }
    if matches!(notes.list[at].status.as_str(), "uploading" | "retrying") {
        return Err("It's being uploaded.".into());
    }
    let note = notes.list.remove(at);
    notes.save();
    std::fs::remove_file(&note.path).or_else(|e| if note.path.exists() { Err(e) } else { Ok(()) }).map_err(|e| {
        format!("Removed from the list, but couldn't delete {}: {e}", note.path.display())
    })
}

/// Transcribe: every note not transcribed yet, one after another, on a thread of its own at low
/// priority. Only ever started by Darren's button.
pub fn transcribe_all(app: AppHandle) -> Result<usize, String> {
    let state = app.state::<AppState>();
    if !cfg!(feature = "whisper") {
        return Err("This build of the companion can't transcribe: it's built without Whisper (sotalore/companion#4).".into());
    }
    let model = state.settings().whisper_model.ok_or("Choose the Whisper model file in the settings first.")?;
    if !model.is_file() {
        return Err(format!("There's no model file at {}", model.display()));
    }
    if lock(&state.talk).recording {
        return Err("Finish recording first.".into());
    }
    let ids: Vec<String> = {
        let mut notes = lock(&state.notes);
        if notes.list.iter().any(|n| matches!(n.status.as_str(), "queued" | "transcribing")) {
            return Err("A batch is running already.".into());
        }
        let mut ids = vec![];
        for note in notes.list.iter_mut().filter(|n| matches!(n.status.as_str(), "recorded" | "failed")) {
            note.status = "queued".into();
            note.message = "Waiting its turn...".into();
            ids.push(note.id.clone());
        }
        notes.save();
        ids
    };
    if ids.is_empty() {
        return Err("Every note is transcribed already.".into());
    }
    let count = ids.len();
    notify_ui(&app);
    std::thread::Builder::new()
        .name("transcribe".into())
        .spawn(move || run_batch(&app, &model, ids))
        .map_err(|e| e.to_string())?;
    Ok(count)
}

fn run_batch(app: &AppHandle, model: &Path, ids: Vec<String>) {
    let state = app.state::<AppState>();
    transcribe::lower_priority();
    // The model is loaded once for the batch (it's hundreds of MB) and dropped at the end, so the
    // memory goes back once the batch is done.
    // `ggml-small.en.bin` → `whisper-small.en`, sent with each note as its `transcriber`.
    let transcriber = model
        .file_stem()
        .map(|s| s.to_string_lossy().trim_start_matches("ggml-").to_string())
        .map(|name| format!("whisper-{name}"));
    let mut whisper = match transcribe::Whisper::load(model) {
        Ok(w) => w,
        Err(e) => {
            mark_all(app, &ids, "failed", &e);
            return;
        }
    };
    for id in ids {
        let (path, scene) = {
            let mut notes = lock(&state.notes);
            let Some(note) = notes.find_mut(&id) else { continue }; // deleted meanwhile
            note.status = "transcribing".into();
            note.message = "Transcribing...".into();
            // The site's name for the scene if it has one, else the game's.
            let scene = note.site_scene.as_ref().map(|s| s.name.clone()).or_else(|| note.scene.as_ref().and_then(|s| s.name.clone()));
            (note.path.clone(), scene)
        };
        notify_ui(app);
        let began = std::time::Instant::now();
        let result = recorder::read_wav(&path).and_then(|samples| whisper.transcribe(&samples, scene.as_deref()));
        let mut notes = lock(&state.notes);
        if let Some(note) = notes.find_mut(&id) {
            match result {
                Ok(text) => {
                    note.status = "transcribed".into();
                    note.message = format!("Transcribed in {:.1} s. Read it through and correct it.", began.elapsed().as_secs_f32());
                    note.transcript = Some(text);
                    note.transcriber = transcriber.clone();
                }
                Err(e) => {
                    note.status = "failed".into();
                    note.message = e;
                }
            }
        }
        notes.save();
        drop(notes);
        notify_ui(app);
    }
}

/// Upload: Darren has read and corrected the transcript and sends it to its scene's private notes.
/// Runs in the background; the note's status shows how it goes. This is the only way a note leaves
/// the machine, and only its text does.
pub fn upload(app: AppHandle, id: String) -> Result<(), String> {
    let state = app.state::<AppState>();
    if lock(&state.connection).state == "ok" && !state.has_scope("notes") {
        return Err(screenshots::scope_help("This token doesn't have the notes scope", "notes"));
    }
    let scene = {
        let mut notes = lock(&state.notes);
        let note = notes.find_mut(&id).ok_or("That note isn't in the list any more.")?;
        if !matches!(note.status.as_str(), "transcribed" | "upload_failed") {
            return Err(match note.status.as_str() {
                "uploaded" => "It's uploaded already.".to_string(),
                "uploading" | "retrying" => "It's being uploaded.".to_string(),
                _ => "Transcribe it first.".to_string(),
            });
        }
        let text = note.transcript.as_deref().unwrap_or("").trim();
        if text.is_empty() {
            return Err("Its transcript is empty: there's nothing to upload.".into());
        }
        if text.chars().count() > MAX_TEXT {
            return Err(format!("It's over {MAX_TEXT} characters, the most the site takes in a note."));
        }
        let scene = note.site_scene.clone().ok_or("Choose its scene first.")?;
        note.status = "uploading".into();
        note.message = format!("Uploading to {}...", scene.name);
        notes.save();
        scene
    };
    notify_ui(&app);
    tauri::async_runtime::spawn(async move {
        let result = upload_now(&app, &id, &scene).await;
        let state = app.state::<AppState>();
        let mut notes = lock(&state.notes);
        if let Some(note) = notes.find_mut(&id) {
            match result {
                Ok((created, url)) => {
                    note.status = "uploaded".into();
                    note.message = if created {
                        format!("Added to {}'s private notes at {}.", scene.name, now())
                    } else {
                        format!("{} had this note already; nothing was added.", scene.name)
                    };
                    note.uploaded_url = url.or_else(|| scene.url.clone());
                }
                Err(message) => {
                    note.status = "upload_failed".into();
                    note.message = message;
                }
            }
        }
        notes.save();
        drop(notes);
        notify_ui(&app);
    });
    Ok(())
}

/// Sends the note with retries. Returns whether it was new, and the scene's page.
async fn upload_now(app: &AppHandle, id: &str, scene: &SiteScene) -> Result<(bool, Option<String>), String> {
    let state = app.state::<AppState>();
    let site = state.settings().site().to_string();
    let token = secrets::get_token(&site)?.ok_or("No API token: add one in the settings.")?;
    // Copy what's sent out of the list, so the lock isn't held while waiting on the site.
    let (text, recorded_at, client_id, seconds, transcriber) = {
        let notes = lock(&state.notes);
        let n = notes.list.iter().find(|n| n.id == id).ok_or("It isn't in the list any more.")?;
        let text = n.transcript.clone().unwrap_or_default().trim().to_string();
        (text, n.recorded_at.clone(), n.client_id.clone(), n.seconds, n.transcriber.clone())
    };
    let note = NewNote {
        text: &text,
        recorded_at: &recorded_at,
        client_id: &client_id,
        duration: Some(seconds.round().max(0.0) as u64),
        transcriber: transcriber.as_deref(),
    };
    Client::new(&site, &token)
        .add_scene_note(scene.id, &note, RETRY_DELAYS, |err, wait| {
            if let Some(n) = lock(&state.notes).find_mut(id) {
                n.status = "retrying".into();
                n.message = format!("{err}. Trying again in {} s.", wait.as_secs());
            }
            notify_ui(app);
        })
        .await
        .map(|made| (made.created, made.scene_url))
        .map_err(|err| match err {
            ApiError::Status { status: 403, message } => screenshots::scope_help(&message, "notes"),
            ApiError::Status { status: 404, .. } => {
                format!("The site has no scene {} (id {}) any more. Choose another scene.", scene.name, scene.id)
            }
            // The site has this client_id with another scene or text and keeps the first. Don't
            // retry; say so.
            ApiError::Status { status: 409, message } => format!(
                "The site refused it ({message}): it keeps the version it has, which a resend can't replace."
            ),
            err if err.is_retryable() => {
                format!("{err}. Gave up after {} retries; press Upload to try again.", RETRY_DELAYS.len())
            }
            err => err.to_string(),
        })
}

fn mark_all(app: &AppHandle, ids: &[String], status: &str, message: &str) {
    let state = app.state::<AppState>();
    let mut notes = lock(&state.notes);
    for id in ids {
        if let Some(note) = notes.find_mut(id) {
            note.status = status.into();
            note.message = message.into();
        }
    }
    notes.save();
    drop(notes);
    notify_ui(app);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(id: &str, status: &str, folder: &Path) -> Note {
        let path = folder.join(format!("{id}.wav"));
        std::fs::write(&path, b"RIFF").unwrap();
        Note {
            id: id.into(),
            path,
            recorded_at: String::new(),
            seconds: 1.0,
            device: String::new(),
            scene: None,
            scene_note: String::new(),
            site_scene: None,
            site_note: String::new(),
            status: status.into(),
            message: String::new(),
            transcript: None,
            client_id: String::new(),
            transcriber: None,
            uploaded_url: None,
        }
    }

    #[test]
    fn a_batch_cut_off_by_a_quit_is_offered_again() {
        let dir = tempfile::tempdir().unwrap();
        let mut notes = Notes { folder: dir.path().to_path_buf(), list: vec![] };
        notes.list.push(note("a", "transcribing", dir.path()));
        notes.list.push(note("b", "transcribed", dir.path()));
        let mut gone = note("c", "recorded", dir.path());
        gone.path = dir.path().join("missing.wav");
        notes.list.push(gone);
        notes.save();
        let loaded = Notes::load(dir.path());
        let statuses: Vec<_> = loaded.list.iter().map(|n| (n.id.as_str(), n.status.as_str())).collect();
        assert_eq!(statuses, vec![("a", "recorded"), ("b", "transcribed")], "the one without its file is dropped");
    }

    #[test]
    fn every_note_gets_its_own_client_id_and_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut notes = Notes { folder: dir.path().to_path_buf(), list: vec![] };
        notes.list.push(note("a", "transcribed", dir.path()));
        notes.list.push(note("b", "uploading", dir.path()));
        notes.save();
        let loaded = Notes::load(dir.path());
        let (a, b) = (&loaded.list[0], &loaded.list[1]);
        assert_eq!(a.client_id.len(), 32);
        assert_ne!(a.client_id, b.client_id);
        assert_eq!(b.status, "upload_failed", "an upload cut off by a quit can be tried again");
        loaded.save();
        assert_eq!(Notes::load(dir.path()).list[0].client_id, a.client_id, "saved, so a retry sends the same one");
    }

    #[test]
    fn two_notes_in_one_second_get_their_own_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut notes = Notes { folder: dir.path().to_path_buf(), list: vec![] };
        assert_eq!(notes.free_id("2026-11-23_16-40-12"), "2026-11-23_16-40-12");
        notes.list.push(note("2026-11-23_16-40-12", "recorded", dir.path()));
        assert_eq!(notes.free_id("2026-11-23_16-40-12"), "2026-11-23_16-40-12_2");
    }

    #[test]
    fn a_note_outside_a_scene_says_why() {
        let (scene, why) = describe_scene(SceneAt::NoScene("loading".into()));
        assert!(scene.is_none());
        assert!(why.contains("loading"));
        assert!(describe_scene(SceneAt::Unknown).1.contains("sl-admin 0.16.0"));
    }
}
