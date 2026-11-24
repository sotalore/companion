//! The app's settings, kept as JSON in the app's config folder
//! (`%APPDATA%\com.sotalore.companion\settings.json`). The API token is **not** here: it's a secret and
//! lives in the Windows Credential Manager (see `secrets.rs`).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_SITE_URL: &str = "https://www.sotalore.com";

#[derive(Debug, Clone, Serialize, Deserialize)]
// `default` fills any field missing from an older settings file, like `||=` on each key in Ruby.
#[serde(default)]
pub struct Settings {
    /// The game client's `Lua/` folder, where the plugins write their exports.
    pub lua_folder: Option<PathBuf>,
    pub site_url: String,
    /// Upload new exports as they're written. Off: the files are only listed, for uploading by hand.
    pub auto_upload: bool,
    /// Where the game saves screenshots (F11). New ones here are offered for their scene's gallery.
    pub screenshot_folder: Option<PathBuf>,
    /// The key held to record a spoken note, written the way the shortcut library reads keys (`F9`,
    /// `Ctrl+F9`). Blank: no push-to-talk. While the companion runs, Windows gives this key to the
    /// companion, not the game.
    pub talk_key: String,
    /// The Whisper model file (`ggml-small.en.bin` and the like) that transcribes the notes.
    pub whisper_model: Option<PathBuf>,
}

pub const DEFAULT_TALK_KEY: &str = "F9";

impl Default for Settings {
    fn default() -> Self {
        // Start with the regular client's folder when it exists.
        let lua_folder = lua_folders().into_iter().map(|(_, path)| path).find(|p| p.is_dir());
        let screenshot_folder = screenshot_folders().into_iter().map(|(_, path)| path).find(|p| p.is_dir());
        Settings {
            lua_folder,
            site_url: DEFAULT_SITE_URL.to_string(),
            auto_upload: true,
            screenshot_folder,
            talk_key: DEFAULT_TALK_KEY.to_string(),
            whisper_model: None,
        }
    }
}

/// The two clients' `Lua/` folders: the regular one and the QA ("DEV") one.
pub fn lua_folders() -> Vec<(&'static str, PathBuf)> {
    let Some(appdata) = std::env::var_os("APPDATA") else {
        return vec![];
    };
    let catnip = PathBuf::from(appdata).join("Catnip Games");
    vec![
        ("Regular client", catnip.join("Shroud of the Avatar").join("Lua")),
        ("QA client", catnip.join("Shroud of the Avatar(DEV)").join("Lua")),
    ]
}

/// The two clients' screenshot folders (EXPORT-FORMAT.md, "Where the game saves screenshots"): the
/// regular client saves in `Pictures\SotA` itself, the QA client in `Pictures\SotA\DEV`.
pub fn screenshot_folders() -> Vec<(&'static str, PathBuf)> {
    let Some(profile) = std::env::var_os("USERPROFILE") else {
        return vec![];
    };
    let sota = PathBuf::from(profile).join("Pictures").join("SotA");
    vec![("Regular client", sota.clone()), ("QA client", sota.join("DEV"))]
}

/// A warning when the two folders look like different clients: screenshots would then be matched
/// against the other client's scenes.
pub fn client_mismatch(settings: &Settings) -> Option<String> {
    let (Some(lua), Some(shots)) = (&settings.lua_folder, &settings.screenshot_folder) else {
        return None;
    };
    let lua_is_qa = lua.to_string_lossy().contains("(DEV)");
    let shots_is_qa = shots.file_name().is_some_and(|n| n.eq_ignore_ascii_case("DEV"));
    (lua_is_qa != shots_is_qa).then(|| {
        "The Lua folder and the screenshot folder look like different clients (regular and QA). \
         Screenshots are matched to the scenes of the client whose Lua folder is chosen."
            .to_string()
    })
}

impl Settings {
    /// Reads the settings file, or the defaults if there isn't one (or it can't be read).
    pub fn load(path: &Path) -> Settings {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)
    }

    /// The site URL without a trailing slash: the form used as the token's key and in the history.
    pub fn site(&self) -> &str {
        self.site_url.trim_end_matches('/')
    }
}

/// Checks a site URL typed in the settings: `https://`, or `http://` only for this machine (a test server).
pub fn check_site_url(url: &str) -> Result<String, String> {
    let url = url.trim().trim_end_matches('/');
    let local = url.starts_with("http://localhost") || url.starts_with("http://127.0.0.1");
    if url.starts_with("https://") || local {
        Ok(url.to_string())
    } else {
        Err("The site URL must start with https:// (or http://localhost for a test server).".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_and_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("settings.json");
        let settings = Settings {
            lua_folder: Some(PathBuf::from(r"C:\Lua")),
            site_url: "https://x.test".into(),
            auto_upload: false,
            screenshot_folder: None,
            ..Settings::default()
        };
        settings.save(&path).unwrap();
        let loaded = Settings::load(&path);
        assert_eq!(loaded.lua_folder, settings.lua_folder);
        assert_eq!(loaded.site_url, "https://x.test");
        assert!(!loaded.auto_upload);
    }

    #[test]
    fn fills_missing_fields_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{ "lua_folder": null }"#).unwrap();
        let loaded = Settings::load(&path);
        assert_eq!(loaded.site_url, DEFAULT_SITE_URL);
        assert!(loaded.auto_upload);
        assert_eq!(loaded.talk_key, DEFAULT_TALK_KEY, "an older settings file gets the default key");
    }

    #[test]
    fn warns_when_the_folders_are_different_clients() {
        let mut s = Settings {
            lua_folder: Some(PathBuf::from(r"C:\A\Catnip Games\Shroud of the Avatar(DEV)\Lua")),
            site_url: DEFAULT_SITE_URL.into(),
            auto_upload: true,
            screenshot_folder: Some(PathBuf::from(r"C:\Users\x\Pictures\SotA\DEV")),
            ..Settings::default()
        };
        assert!(client_mismatch(&s).is_none());
        s.screenshot_folder = Some(PathBuf::from(r"C:\Users\x\Pictures\SotA"));
        assert!(client_mismatch(&s).is_some());
    }

    #[test]
    fn site_urls_must_be_https_unless_local() {
        assert_eq!(check_site_url(" https://www.sotalore.com/ ").unwrap(), "https://www.sotalore.com");
        assert!(check_site_url("http://localhost:4567").is_ok());
        assert!(check_site_url("http://www.sotalore.com").is_err());
    }
}
