//! Which scene the player was in, and when: the timeline screenshots are matched against.
//!
//! `sl-admin` (0.16.0 and later) writes `sotalore-current-scene.json` (`"type": "currentScene"`) in the
//! `Lua/` folder each time the scene changes; `sladmin/EXPORT-FORMAT.md`, "The current scene", is the
//! contract. The file holds only the current scene, so we record each version of it as it's written,
//! and a screenshot's scene is the last change before the screenshot's time.

use serde::{Deserialize, Serialize};

/// The current-scene file, as the plugin writes it. Fields we don't use are skipped.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")] // `rawName` in the file is `raw_name` here
struct SceneFile {
    #[serde(rename = "type")]
    file_type: Option<String>,
    state: String,
    name: Option<String>,
    raw_name: Option<String>,
    dungeon_name: Option<String>,
    dungeon_owner: Option<String>,
    character: Option<String>,
    entered_at: i64,
    #[serde(default = "yes")]
    entered_at_exact: bool,
}

fn yes() -> bool {
    true
}

/// One scene change: from `entered_at` until the next change, the player was here (or, when `state`
/// isn't `inScene`, loading or logged out, in no scene at all).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SceneEntry {
    /// `inScene`, `loading` or `loggedOut`.
    pub state: String,
    /// The display name, in the player's language.
    pub name: Option<String>,
    /// The game's internal scene id (`OwlsHead_01`). Compare scenes with this, not `name`.
    pub raw_name: Option<String>,
    /// Inside a player dungeon or basement: its name and owner. `name`/`raw_name` are still the real scene.
    pub dungeon_name: Option<String>,
    pub dungeon_owner: Option<String>,
    pub character: Option<String>,
    /// Seconds since 1970 UTC.
    pub entered_at: i64,
    /// `false`: the plugin started up already in the scene, so the scene began before `entered_at`.
    pub exact: bool,
}

impl SceneEntry {
    pub fn in_scene(&self) -> bool {
        self.state == "inScene" && self.raw_name.is_some()
    }
}

/// Reads a current-scene file. `None` if it isn't one (another `type`), or isn't complete JSON.
pub fn parse(bytes: &[u8]) -> Option<SceneEntry> {
    let file: SceneFile = serde_json::from_slice(bytes).ok()?;
    if file.file_type.as_deref() != Some("currentScene") {
        return None;
    }
    Some(SceneEntry {
        state: file.state,
        name: file.name,
        raw_name: file.raw_name,
        dungeon_name: file.dungeon_name,
        dungeon_owner: file.dungeon_owner,
        character: file.character,
        entered_at: file.entered_at,
        exact: file.entered_at_exact,
    })
}

/// Where the player was at some moment.
#[derive(Debug, Clone, PartialEq)]
pub enum SceneAt {
    /// In this scene.
    Scene(SceneEntry),
    /// Loading or logged out (the state's name): the screenshot belongs to no scene.
    NoScene(String),
    /// We hadn't seen the current-scene file by then (the app wasn't running, or the plugin is older).
    Unknown,
}

/// The scene changes seen since the app started, oldest first.
#[derive(Default)]
pub struct Timeline {
    entries: Vec<SceneEntry>,
}

/// Plenty for a long session; older changes can't match a new screenshot anyway.
const KEEP: usize = 500;

impl Timeline {
    /// Records a version of the file. Returns `false` if it's the same as one already recorded (the
    /// plugin rewrites the file unchanged after a `/lua reload`, and the watcher can report it twice).
    pub fn record(&mut self, entry: SceneEntry) -> bool {
        if self.entries.contains(&entry) {
            return false;
        }
        // A rewrite for the same moment (say a `/lua reload` that learned more) replaces the earlier one.
        self.entries.retain(|e| !(e.entered_at == entry.entered_at && e.state == entry.state));
        // Keep the list in time order; files normally arrive in order, so this is usually the end.
        let at = self.entries.partition_point(|e| e.entered_at <= entry.entered_at);
        self.entries.insert(at, entry);
        if self.entries.len() > KEEP {
            self.entries.remove(0);
        }
        true
    }

    /// The newest change: where the player is now.
    pub fn current(&self) -> Option<&SceneEntry> {
        self.entries.last()
    }

    /// Where the player was at `time` (seconds since 1970 UTC): the last change at or before it.
    pub fn at(&self, time: i64) -> SceneAt {
        // `partition_point` finds how many entries are at or before `time`, by binary search.
        let before = self.entries.partition_point(|e| e.entered_at <= time);
        match before.checked_sub(1).map(|i| &self.entries[i]) {
            None => SceneAt::Unknown,
            Some(e) if e.in_scene() => SceneAt::Scene(e.clone()),
            Some(e) => SceneAt::NoScene(e.state.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The example from EXPORT-FORMAT.md.
    const OWLS_HEAD: &str = r#"{"apiVersion":58,"plugin":"sl-admin","pluginVersion":"0.16.0","type":"currentScene","state":"inScene",
 "name":"Owl's Head","rawName":"OwlsHead_01","dungeonName":null,"dungeonOwner":null,"character":"Gravidy",
 "enteredAt":1795463924,"enteredAtUtc":"2026-11-23T19:58:44Z","enteredAtExact":true,"writtenAt":1795463924}"#;

    fn entry(state: &str, raw: Option<&str>, at: i64) -> SceneEntry {
        SceneEntry {
            state: state.into(),
            name: raw.map(String::from),
            raw_name: raw.map(String::from),
            dungeon_name: None,
            dungeon_owner: None,
            character: None,
            entered_at: at,
            exact: true,
        }
    }

    #[test]
    fn parses_the_contract_example() {
        let e = parse(OWLS_HEAD.as_bytes()).unwrap();
        assert_eq!(e.state, "inScene");
        assert_eq!(e.name.as_deref(), Some("Owl's Head"));
        assert_eq!(e.raw_name.as_deref(), Some("OwlsHead_01"));
        assert_eq!(e.character.as_deref(), Some("Gravidy"));
        assert_eq!(e.entered_at, 1795463924);
        assert!(e.exact && e.in_scene());
    }

    #[test]
    fn parses_loading_with_null_names() {
        let e = parse(br#"{"type":"currentScene","state":"loading","name":null,"rawName":null,"dungeonName":null,
            "dungeonOwner":null,"character":null,"enteredAt":100,"enteredAtExact":true,"writtenAt":100}"#)
        .unwrap();
        assert!(!e.in_scene());
    }

    #[test]
    fn other_files_and_part_written_ones_are_not_scenes() {
        assert!(parse(br#"{"type":"recipes","state":"inScene","enteredAt":1}"#).is_none());
        assert!(parse(&OWLS_HEAD.as_bytes()[..80]).is_none());
    }

    #[test]
    fn matches_a_time_to_the_last_change_before_it() {
        let mut t = Timeline::default();
        t.record(entry("inScene", Some("OwlsHead_01"), 100));
        t.record(entry("loading", None, 200));
        t.record(entry("inScene", Some("Novia"), 230));
        assert_eq!(t.at(99), SceneAt::Unknown);
        assert_eq!(t.at(100), SceneAt::Scene(entry("inScene", Some("OwlsHead_01"), 100)));
        assert_eq!(t.at(199), SceneAt::Scene(entry("inScene", Some("OwlsHead_01"), 100)));
        assert_eq!(t.at(210), SceneAt::NoScene("loading".into()));
        assert_eq!(t.at(5000), SceneAt::Scene(entry("inScene", Some("Novia"), 230)));
        assert_eq!(t.current().unwrap().raw_name.as_deref(), Some("Novia"));
    }

    #[test]
    fn ignores_repeats_and_keeps_time_order() {
        let mut t = Timeline::default();
        assert!(t.record(entry("inScene", Some("B"), 200)));
        assert!(!t.record(entry("inScene", Some("B"), 200)));
        assert!(t.record(entry("inScene", Some("A"), 100))); // arrived late
        assert_eq!(t.at(150), SceneAt::Scene(entry("inScene", Some("A"), 100)));
        assert_eq!(t.current().unwrap().raw_name.as_deref(), Some("B"));
    }
}
