//! Telling the plugins' export files apart, as `sotalore/sladmin`'s `EXPORT-FORMAT.md` describes them.
//!
//! Every file starts with the same header: `apiVersion`, `plugin`, `pluginVersion` and `type`. The
//! contract says to tell files apart by `type`, never by the file name (older plugins used other names),
//! so the name only decides which files we look at, and `type` decides what happens to them.

use serde::Deserialize;
use std::path::Path;

/// The parts of the header we use. Serde fills only the fields named here and skips the rest, like
/// picking a few keys out of `JSON.parse` in Ruby. Both are optional because files written before
/// plugin 0.7.1 have no `type`.
#[derive(Debug, Deserialize)]
pub struct Header {
    #[serde(rename = "type")]
    pub file_type: Option<String>,
    /// Only recipe and item files have one: `all`, `new` or `one`.
    pub scope: Option<String>,
}

/// What we do with a file, decided by its `type`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// A file the site takes, and the API path to post it to (under `/api/v1`).
    Upload { file_type: String, endpoint: &'static str },
    /// A file with a `type` the site doesn't take (`gameSkills`, `skills`, the current-scene file...).
    NotUploaded { file_type: String },
    /// No `type` at all: written by a plugin older than 0.7.1, which the site refuses.
    NoType,
}

/// The upload endpoint for a file `type`, per "Uploads" in the site's `docs/api.md`. Anything not listed
/// here is never sent: we only upload the types the API takes.
pub fn endpoint_for(file_type: &str) -> Option<&'static str> {
    match file_type {
        "recipes" => Some("/recipe_imports"),
        "items" => Some("/item_catalog_imports"),
        "finds" => Some("/finds_imports"),
        _ => None,
    }
}

/// Whether a file in the `Lua/` folder is one to look at. The plugins name every export
/// `sotalore-<kind>.json`; its `type` is checked after it's read.
pub fn is_candidate(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    name.starts_with("sotalore-") && name.ends_with(".json")
}

/// Reads the header from a whole file. An error means the file isn't valid JSON, which usually means
/// we caught it part-written: the caller waits and tries again.
pub fn read_header(bytes: &[u8]) -> Result<Header, serde_json::Error> {
    // Parsing the whole file (not just the start) is deliberate: it proves the file is complete.
    serde_json::from_slice(bytes)
}

/// Decides what to do with a file from its header.
pub fn classify(header: &Header) -> Kind {
    match &header.file_type {
        None => Kind::NoType,
        Some(t) => match endpoint_for(t) {
            Some(endpoint) => Kind::Upload { file_type: t.clone(), endpoint },
            None => Kind::NotUploaded { file_type: t.clone() },
        },
    }
}

/// A short label for the UI: `recipes (all)`, `items (new)`, `finds`.
pub fn describe(header: &Header) -> String {
    let t = header.file_type.as_deref().unwrap_or("no type");
    match &header.scope {
        Some(scope) => format!("{t} ({scope})"),
        None => t.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind_of(json: &str) -> Kind {
        classify(&read_header(json.as_bytes()).unwrap())
    }

    #[test]
    fn uploads_the_three_types_the_api_takes() {
        // Headers copied from real files written by sl-admin 0.15.0.
        let recipes = r#"{"apiVersion":57,"plugin":"sl-admin","pluginVersion":"0.15.0","type":"recipes","scope":"new","count":0,"recipes":[]}"#;
        let items = r#"{"apiVersion":58,"plugin":"sl-admin","pluginVersion":"0.15.0","type":"items","scope":"new","count":0,"items":[]}"#;
        let finds = r#"{"apiVersion":58,"plugin":"sl-admin","pluginVersion":"0.15.0","type":"finds","scenes":[]}"#;
        assert_eq!(kind_of(recipes), Kind::Upload { file_type: "recipes".into(), endpoint: "/recipe_imports" });
        assert_eq!(kind_of(items), Kind::Upload { file_type: "items".into(), endpoint: "/item_catalog_imports" });
        assert_eq!(kind_of(finds), Kind::Upload { file_type: "finds".into(), endpoint: "/finds_imports" });
    }

    #[test]
    fn skips_types_the_site_does_not_take() {
        let game_skills = r#"{"apiVersion":57,"plugin":"sl-admin","pluginVersion":"0.15.0","type":"gameSkills","count":0,"skills":[]}"#;
        let skills = r#"{"apiVersion":57,"plugin":"sl-skills","pluginVersion":"0.1.0","type":"skills"}"#;
        assert_eq!(kind_of(game_skills), Kind::NotUploaded { file_type: "gameSkills".into() });
        assert_eq!(kind_of(skills), Kind::NotUploaded { file_type: "skills".into() });
    }

    #[test]
    fn a_file_without_a_type_is_not_uploaded() {
        assert_eq!(kind_of(r#"{"apiVersion":40,"recipes":[]}"#), Kind::NoType);
    }

    #[test]
    fn a_part_written_file_is_an_error() {
        assert!(read_header(br#"{"apiVersion":57,"type":"recipes","recipes":[{"id":1,"#).is_err());
    }

    #[test]
    fn looks_only_at_sotalore_json_files() {
        assert!(is_candidate(Path::new(r"C:\Lua\sotalore-recipes-all.json")));
        assert!(is_candidate(Path::new(r"C:\Lua\SotaLore-Item-Catalog.JSON")));
        assert!(!is_candidate(Path::new(r"C:\Lua\sotalore-recipes-all.json.tmp")));
        assert!(!is_candidate(Path::new(r"C:\Lua\other.json")));
    }

    #[test]
    fn describes_scope_when_there_is_one() {
        let h = read_header(br#"{"type":"recipes","scope":"all"}"#).unwrap();
        assert_eq!(describe(&h), "recipes (all)");
    }
}
