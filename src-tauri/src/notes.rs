//! Spoken notes: Darren holds the talk key while playing and says something about the scene he's in.
//!
//! Each recording is saved as a WAV file in the app's data folder (`notes\`), labelled with the scene
//! from the current-scene file (as screenshots are), and listed in the window. **The audio never
//! leaves peregrin.** Later, on Darren's command and never during play, the notes are transcribed in
//! one batch (`transcribe.rs`), and he reviews each transcript before it's uploaded (that waits for
//! the site's endpoint, sotalore/sotalore#311).

use crate::api::SiteScene;
use crate::recorder::{self, Clip, Recorder};
use crate::scenes::{SceneAt, SceneEntry};
use crate::screenshots;
use crate::state::{lock, notify_ui, AppState};
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
    /// `recorded` (not transcribed yet), `queued`, `transcribing`, `transcribed` or `failed`.
    pub status: String,
    pub message: String,
    /// What Whisper heard, then as Darren corrected it.
    pub transcript: Option<String>,
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
                    lock(&state.talk).recording = true;
                    show_recording(&started, true);
                }
                Err(e) => {
                    state.add_activity("", "note", "failed", e);
                }
            }
            notify_ui(&started);
        },
        move |result| {
            let state = done.state::<AppState>();
            lock(&state.talk).recording = false;
            show_recording(&done, false);
            match result {
                Ok(clip) => saved(&done, clip),
                Err(e) => {
                    state.add_activity("", "note", "failed", e);
                }
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
        state.add_activity("", "note", "failed", format!("Couldn't save the note: {e}"));
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

/// While recording, the tray icon gets a red dot and says so (the window is usually behind the game).
fn show_recording(app: &AppHandle, recording: bool) {
    let Some(tray) = app.tray_by_id("main") else { return };
    let Some(icon) = app.default_window_icon() else { return };
    if recording {
        let _ = tray.set_icon(Some(with_red_dot(icon)));
        let _ = tray.set_tooltip(Some("SotA Lore Companion: recording a note"));
    } else {
        let _ = tray.set_icon(Some(icon.clone()));
        let waiting = lock(&app.state::<AppState>().shots).waiting();
        screenshots::update_tray(app, waiting);
    }
}

/// The app icon with a red dot in its lower right quarter.
fn with_red_dot(icon: &tauri::image::Image<'_>) -> tauri::image::Image<'static> {
    let (w, h) = (icon.width() as i64, icon.height() as i64);
    let mut rgba = icon.rgba().to_vec();
    let (cx, cy, r) = (w * 3 / 4, h * 3 / 4, w.min(h) / 4);
    for y in 0..h {
        for x in 0..w {
            if (x - cx).pow(2) + (y - cy).pow(2) <= r * r {
                let i = ((y * w + x) * 4) as usize;
                rgba[i..i + 4].copy_from_slice(&[220, 30, 30, 255]);
            }
        }
    }
    tauri::image::Image::new_owned(rgba, w as u32, h as u32)
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
