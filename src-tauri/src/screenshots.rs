//! Screenshots for the scenes' galleries.
//!
//! When the game saves a screenshot (F11), the watcher hands it here. We wait until it's written,
//! take its time from the file itself, find the scene the player was in then (`scenes.rs`), look that
//! scene up on the site (`GET /scenes?game_name=`), and add the screenshot to the list in the window.
//! **Nothing is sent until Darren presses Send**: an image goes live on the site at once, and a
//! screenshot can show things he doesn't want published. Skip drops it from the list; the file on disk
//! is never touched.

use crate::api::{ApiError, Client, SiteScene, RETRY_DELAYS};
use crate::scenes::{SceneAt, SceneEntry};
use crate::secrets;
use crate::state::{lock, notify_ui, now, AppState};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager};
use tokio::sync::mpsc::UnboundedReceiver;

/// The site takes images up to 20 MB (`docs/api.md`).
const MAX_BYTES: usize = 20 * 1024 * 1024;
/// How long after a screenshot's time we wait before matching it, so a scene change written at
/// about the same moment has been recorded first.
const SETTLE: Duration = Duration::from_secs(3);
/// How many screenshots the list keeps (waiting ones are never dropped for this).
const KEEP: usize = 100;

/// One screenshot in the list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Shot {
    /// The file's full path; also its id in the list.
    pub path: PathBuf,
    pub file: String,
    /// When it was taken (the file's time), local, ISO 8601 with the offset: sent as `taken_at`.
    pub taken_at: String,
    /// The scene the player was in, from the current-scene file, or `None` (see `scene_note`).
    pub scene: Option<SceneEntry>,
    /// Why there's no scene, or a remark about it (a player dungeon...).
    pub scene_note: String,
    /// The site's scene it would go to. Darren can choose another before sending.
    pub site_scene: Option<SiteScene>,
    /// Why there's no site scene (not on the site yet, the token can't read...).
    pub site_note: String,
    /// `waiting` (for Darren), `sending`, `retrying`, `sent`, `already`, `failed` or `skipped`.
    pub status: String,
    pub message: String,
    pub image_url: Option<String>,
}

/// The list, newest first, kept in the app's data folder (`screenshots.json`) so waiting screenshots
/// survive a restart.
pub struct Shots {
    path: PathBuf,
    pub list: Vec<Shot>,
}

impl Shots {
    pub fn load(path: &Path) -> Shots {
        let list: Vec<Shot> = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        // Forget screenshots whose files are gone, and any send that was cut off by a quit.
        let list = list
            .into_iter()
            .filter(|s| s.path.is_file())
            .map(|mut s| {
                if s.status == "sending" || s.status == "retrying" {
                    s.status = "waiting".into();
                    s.message = "The app quit while sending; press Send again.".into();
                }
                s
            })
            .collect();
        Shots { path: path.to_path_buf(), list }
    }

    pub fn save(&self) {
        let result = (|| {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&self.path, serde_json::to_string_pretty(&self.list)?)
        })();
        if let Err(e) = result {
            eprintln!("couldn't save the screenshot list: {e}");
        }
    }

    pub fn find_mut(&mut self, path: &Path) -> Option<&mut Shot> {
        self.list.iter_mut().find(|s| s.path == path)
    }

    pub fn waiting(&self) -> usize {
        self.list.iter().filter(|s| s.status == "waiting").count()
    }

    fn add(&mut self, shot: Shot) {
        self.list.insert(0, shot);
        // Drop the oldest finished ones past the limit; never one still waiting for Darren.
        while self.list.len() > KEEP {
            match self.list.iter().rposition(|s| s.status != "waiting") {
                Some(i) => self.list.remove(i),
                None => break,
            };
        }
        self.save();
    }
}

/// Image files the game (or a person) might save in the folder.
pub fn is_image(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    !name.starts_with('.') && [".png", ".jpg", ".jpeg", ".webp"].iter().any(|ext| name.ends_with(ext))
}

/// Whether an image file is completely written, from how each format ends.
pub fn is_complete(bytes: &[u8]) -> bool {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        // A PNG ends with an IEND chunk: its type, then a 4-byte checksum.
        bytes.len() >= 12 && &bytes[bytes.len() - 8..bytes.len() - 4] == b"IEND"
    } else if bytes.starts_with(&[0xFF, 0xD8]) {
        bytes.ends_with(&[0xFF, 0xD9]) // JPEG's end-of-image marker
    } else if bytes.starts_with(b"RIFF") && bytes.len() >= 12 && &bytes[8..12] == b"WEBP" {
        // WebP gives its own length after "RIFF" (little-endian), not counting the first 8 bytes.
        let size = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
        bytes.len() >= size + 8
    } else {
        false
    }
}

/// Starts the worker that takes new screenshots from the watcher, one at a time.
pub fn start(app: AppHandle, mut queue: UnboundedReceiver<PathBuf>) {
    tauri::async_runtime::spawn(async move {
        while let Some(path) = queue.recv().await {
            detect(&app, path).await;
            notify_ui(&app);
        }
    });
}

/// A new screenshot: match it to a scene and add it to the list to wait for Darren.
async fn detect(app: &AppHandle, path: PathBuf) {
    let state = app.state::<AppState>();
    if lock(&state.shots).list.iter().any(|s| s.path == path) {
        return; // already listed (the game or Windows touched the file again)
    }
    let Some(modified) = std::fs::metadata(&path).and_then(|m| m.modified()).ok() else {
        return; // gone already
    };
    // Give a scene change at about the same moment time to be recorded.
    if let Ok(age) = SystemTime::now().duration_since(modified) {
        if age < SETTLE {
            tokio::time::sleep(SETTLE - age).await;
        }
    }
    let file = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let taken = chrono::DateTime::<chrono::Local>::from(modified);
    let taken_unix = modified.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);

    let mut shot = Shot {
        path: path.clone(),
        file,
        taken_at: taken.to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
        scene: None,
        scene_note: String::new(),
        site_scene: None,
        site_note: String::new(),
        status: "waiting".into(),
        message: String::new(),
        image_url: None,
    };

    if read_complete(&path).await.is_none() {
        shot.status = "failed".into();
        shot.message = "It isn't a complete PNG, JPEG or WebP image.".into();
    }

    let at = lock(&state.timeline).at(taken_unix);
    match at {
        SceneAt::Scene(entry) => {
            if let Some(dungeon) = entry.dungeon_owner.as_ref() {
                let name = entry.dungeon_name.as_deref().unwrap_or("an unnamed dungeon");
                shot.scene_note = format!("In {dungeon}'s player dungeon or basement ({name}).");
            } else if !entry.exact {
                shot.scene_note = "The plugin started in this scene, so it may have been entered earlier.".into();
            }
            shot.scene = Some(entry);
        }
        SceneAt::NoScene(state_name) => {
            shot.scene_note = match state_name.as_str() {
                "loading" => "Taken while a scene was loading: it belongs to no scene.".into(),
                "loggedOut" => "Taken at the login screen: it belongs to no scene.".into(),
                other => format!("Taken in game state `{other}`: no scene."),
            };
        }
        SceneAt::Unknown => {
            shot.scene_note =
                "No scene known for that time: sl-admin 0.16.0 or later writes it, while the companion runs.".into();
        }
    }
    if shot.scene.is_some() {
        look_up_site_scene(app, &mut shot).await;
    }

    // Let the window show this file as a thumbnail (it may read only files allowed this way).
    let _ = app.asset_protocol_scope().allow_file(&shot.path);
    let mut shots = lock(&state.shots);
    shots.add(shot);
    let waiting = shots.waiting();
    drop(shots);
    update_tray(app, waiting);
}

/// Reads the image, waiting a little if it's still being written. `None` if it never completes.
async fn read_complete(path: &Path) -> Option<Vec<u8>> {
    for _ in 0..5 {
        if let Ok(bytes) = std::fs::read(path) {
            if is_complete(&bytes) {
                return Some(bytes);
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    None
}

/// Finds the site's scene for a screenshot's game scene (`GET /scenes?game_name=`), using the
/// answers already had where possible. Fills `site_scene` or says in `site_note` why not.
pub async fn look_up_site_scene(app: &AppHandle, shot: &mut Shot) {
    let Some(raw) = shot.scene.as_ref().and_then(|s| s.raw_name.clone()) else {
        return;
    };
    let state = app.state::<AppState>();
    let cached = lock(&state.site_scenes).get(&raw).cloned();
    let found = match cached {
        Some(found) => found,
        None => {
            let site = state.settings().site().to_string();
            let token = match secrets::get_token(&site) {
                Ok(Some(token)) => token,
                _ => {
                    shot.site_note = "Add an API token to find this scene on the site.".into();
                    return;
                }
            };
            if !state.has_scope("read") {
                shot.site_note = "The token needs the read scope to find scenes on the site.".into();
                return;
            }
            match Client::new(&site, &token).scenes("game_name", &raw).await {
                Ok(list) => {
                    let found = list.into_iter().next();
                    lock(&state.site_scenes).insert(raw.clone(), found.clone());
                    found
                }
                Err(ApiError::Status { status: 403, message }) => {
                    shot.site_note = format!(
                        "The site refused the scene lookup ({message}): the token needs the `read` scope."
                    );
                    return;
                }
                Err(e) => {
                    shot.site_note = format!("Couldn't look the scene up: {e}");
                    return;
                }
            }
        }
    };
    match found {
        Some(scene) => {
            shot.site_scene = Some(scene);
            shot.site_note.clear();
        }
        None => {
            shot.site_note = format!(
                "No scene on the site has the game name {raw} yet (a finds import links it). Choose one by name, or skip."
            );
        }
    }
}

/// Darren pressed Send: add the screenshot to `scene`'s gallery. Runs in the background; the list
/// shows how it goes.
pub fn send(app: AppHandle, path: PathBuf, scene: SiteScene) -> Result<(), String> {
    let state = app.state::<AppState>();
    {
        let mut shots = lock(&state.shots);
        let shot = shots.find_mut(&path).ok_or("That screenshot isn't in the list any more.")?;
        if !matches!(shot.status.as_str(), "waiting" | "failed") {
            return Err(format!("That screenshot is {} already.", shot.status));
        }
        shot.status = "sending".into();
        shot.message = format!("Sending to {}...", scene.name);
        shot.site_scene = Some(scene.clone());
        shot.site_note.clear(); // a scene is chosen now, so why there wasn't one no longer matters
        shots.save();
    }
    notify_ui(&app);
    tauri::async_runtime::spawn(async move {
        let result = send_now(&app, &path, &scene).await;
        let state = app.state::<AppState>();
        let mut shots = lock(&state.shots);
        if let Some(shot) = shots.find_mut(&path) {
            match result {
                Ok((created, url)) => {
                    shot.status = if created { "sent" } else { "already" }.into();
                    shot.message = if created {
                        format!("Added to {}'s gallery at {}. It's live now.", scene.name, now())
                    } else {
                        format!("{} already had this exact image; nothing was added.", scene.name)
                    };
                    shot.image_url = url;
                }
                Err(message) => {
                    shot.status = "failed".into();
                    shot.message = message;
                }
            }
        }
        shots.save();
        let waiting = shots.waiting();
        drop(shots);
        update_tray(&app, waiting);
        notify_ui(&app);
    });
    Ok(())
}

async fn send_now(app: &AppHandle, path: &Path, scene: &SiteScene) -> Result<(bool, Option<String>), String> {
    let state = app.state::<AppState>();
    let site = state.settings().site().to_string();
    let token = secrets::get_token(&site)?.ok_or("No API token: add one in the settings.")?;
    let bytes = std::fs::read(path).map_err(|e| format!("Couldn't read the file: {e}"))?;
    if bytes.len() > MAX_BYTES {
        return Err("It's over 20 MB, the most the site takes.".into());
    }
    let (file, taken_at) = {
        let shots = lock(&state.shots);
        let shot = shots.list.iter().find(|s| s.path == path).ok_or("It isn't in the list any more.")?;
        (shot.file.clone(), shot.taken_at.clone())
    };
    let client = Client::new(&site, &token);
    client
        .add_scene_image(scene.id, &file, &bytes, &taken_at, RETRY_DELAYS, |err, wait| {
            if let Some(shot) = lock(&state.shots).find_mut(path) {
                shot.status = "retrying".into();
                shot.message = format!("{err}. Trying again in {} s.", wait.as_secs());
            }
            notify_ui(app);
        })
        .await
        .map(|image| (image.created, image.url.or(image.scene_url)))
        .map_err(|err| match err {
            // The token lacks `images` (or the account may not add images): say how to fix it.
            ApiError::Status { status: 403, message } => scope_help(&message, "images"),
            ApiError::Status { status: 404, .. } => {
                format!("The site has no scene {} (id {}) any more. Choose another scene.", scene.name, scene.id)
            }
            err if err.is_retryable() => {
                format!("{err}. Gave up after {} retries; press Send to try again.", RETRY_DELAYS.len())
            }
            err => err.to_string(),
        })
}

/// A 403 explained: which scope the token needs, and how to get a token that has it. The site's own
/// words come first, since a 403 can also mean the account isn't allowed ("Not allowed").
pub fn scope_help(site_message: &str, scope: &str) -> String {
    let site_message = site_message.trim_end_matches('.');
    format!(
        "The site refused it: {site_message}. Sending needs a token with the `{scope}` scope. \
         Make a new token at ADM → AI Access with `upload`, `read` and `images`, paste it under Site \
         (it replaces the saved one), then press Send again."
    )
}

/// Darren pressed Skip: drop it from the waiting list. The file stays where it is.
pub fn skip(app: &AppHandle, path: &Path) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut shots = lock(&state.shots);
    let shot = shots.find_mut(path).ok_or("That screenshot isn't in the list any more.")?;
    if matches!(shot.status.as_str(), "sending" | "retrying") {
        return Err("It's being sent.".into());
    }
    shot.status = "skipped".into();
    shot.message = "Skipped: not sent.".into();
    shots.save();
    let waiting = shots.waiting();
    drop(shots);
    update_tray(app, waiting);
    Ok(())
}

/// The tray icon's tooltip says how many screenshots wait, so Darren sees it without the window
/// popping up over the game.
pub fn update_tray(app: &AppHandle, waiting: usize) {
    if let Some(tray) = app.tray_by_id("main") {
        let text = match waiting {
            0 => "SotA Lore Companion".to_string(),
            1 => "SotA Lore Companion: 1 screenshot to review".to_string(),
            n => format!("SotA Lore Companion: {n} screenshots to review"),
        };
        let _ = tray.set_tooltip(Some(text));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_names_the_scope_and_the_fix() {
        let text = scope_help("This token doesn't have the images scope", "images");
        assert!(text.starts_with("The site refused it: This token doesn't have the images scope."));
        assert!(text.contains("`images` scope"));
        assert!(text.contains("ADM → AI Access"));
    }

    #[test]
    fn knows_image_files() {
        assert!(is_image(Path::new(r"C:\P\SotA_09-07-26_10-15_2.png")));
        assert!(is_image(Path::new(r"C:\P\shot.JPG")));
        assert!(!is_image(Path::new(r"C:\P\notes.txt")));
        assert!(!is_image(Path::new(r"C:\P\.hidden.png")));
    }

    #[test]
    fn tells_a_complete_png_from_a_part_written_one() {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(b"....IHDR rest of the image ....");
        png.extend_from_slice(&[0, 0, 0, 0]);
        png.extend_from_slice(b"IEND");
        png.extend_from_slice(&[0xAE, 0x42, 0x60, 0x82]);
        assert!(is_complete(&png));
        assert!(!is_complete(&png[..png.len() - 10]));
        assert!(!is_complete(b"not an image"));
    }

    #[test]
    fn checks_jpeg_and_webp_ends() {
        assert!(is_complete(&[0xFF, 0xD8, 1, 2, 3, 0xFF, 0xD9]));
        assert!(!is_complete(&[0xFF, 0xD8, 1, 2, 3]));
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&8u32.to_le_bytes());
        webp.extend_from_slice(b"WEBPVP8 ");
        assert!(is_complete(&webp));
        assert!(!is_complete(&webp[..14]));
    }

    #[test]
    fn a_real_screenshot_is_complete() {
        // One of Darren's QA-client screenshots, if this machine has them.
        let Some(profile) = std::env::var_os("USERPROFILE") else { return };
        let dir = PathBuf::from(profile).join(r"Pictures\SotA\DEV");
        let Some(first) = std::fs::read_dir(dir).ok().and_then(|mut d| d.find_map(|e| e.ok().map(|e| e.path()).filter(|p| is_image(p))))
        else {
            return;
        };
        assert!(is_complete(&std::fs::read(first).unwrap()));
    }

    #[test]
    fn keeps_waiting_shots_and_drops_old_finished_ones() {
        let dir = tempfile::tempdir().unwrap();
        let mut shots = Shots { path: dir.path().join("screenshots.json"), list: vec![] };
        let shot = |n: usize, status: &str| Shot {
            path: PathBuf::from(format!("{n}.png")),
            file: format!("{n}.png"),
            taken_at: String::new(),
            scene: None,
            scene_note: String::new(),
            site_scene: None,
            site_note: String::new(),
            status: status.into(),
            message: String::new(),
            image_url: None,
        };
        shots.add(shot(0, "waiting"));
        for n in 1..=KEEP {
            shots.add(shot(n, "sent"));
        }
        assert_eq!(shots.list.len(), KEEP);
        assert!(shots.list.iter().any(|s| s.file == "0.png"), "the waiting one stays");
        assert!(!shots.list.iter().any(|s| s.file == "1.png"), "the oldest sent one goes");
    }
}
