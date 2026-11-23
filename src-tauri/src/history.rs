//! What we've uploaded, kept in the app's data folder (`uploads.json`). Each record is keyed by the
//! SHA-256 of the file's bytes, so a file the game writes again unchanged isn't sent again, and the
//! file list can say when each file was last uploaded. It holds file names, import ids and review
//! links, never the token.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub file: String,
    pub file_type: String,
    pub site: String,
    /// Local time, `2026-11-23 10:00:00`; sorts as text.
    pub at: String,
    pub created: bool,
    pub import_id: Option<i64>,
    pub review_url: Option<String>,
}

pub struct History {
    path: PathBuf,
    /// Content hash → the upload of those bytes. Like a Ruby Hash of String => Record.
    records: HashMap<String, Record>,
}

pub fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

impl History {
    pub fn load(path: &Path) -> History {
        let records = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        History { path: path.to_path_buf(), records }
    }

    /// The upload of exactly these bytes to this site, if there was one.
    pub fn find(&self, hash: &str, site: &str) -> Option<&Record> {
        self.records.get(hash).filter(|r| r.site == site)
    }

    /// The newest upload of a file by name, for the file list.
    pub fn latest_for_file(&self, file: &str, site: &str) -> Option<&Record> {
        self.records
            .values()
            .filter(|r| r.file.eq_ignore_ascii_case(file) && r.site == site)
            .max_by(|a, b| a.at.cmp(&b.at))
    }

    pub fn add(&mut self, hash: String, record: Record) {
        self.records.insert(hash, record);
        // Keep the file small: the newest 200 uploads are plenty.
        while self.records.len() > 200 {
            let oldest = self.records.iter().min_by(|a, b| a.1.at.cmp(&b.1.at)).map(|(k, _)| k.clone());
            match oldest {
                Some(key) => self.records.remove(&key),
                None => break,
            };
        }
        if let Err(e) = self.save() {
            eprintln!("couldn't save the upload history: {e}");
        }
    }

    fn save(&self) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.path, serde_json::to_string_pretty(&self.records)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(file: &str, site: &str, at: &str) -> Record {
        Record {
            file: file.into(),
            file_type: "recipes".into(),
            site: site.into(),
            at: at.into(),
            created: true,
            import_id: Some(1),
            review_url: None,
        }
    }

    #[test]
    fn finds_by_hash_and_site_and_survives_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("uploads.json");
        let mut history = History::load(&path);
        history.add(sha256(b"one"), record("a.json", "https://s.test", "2026-11-23 10:00:00"));
        let history = History::load(&path);
        assert!(history.find(&sha256(b"one"), "https://s.test").is_some());
        assert!(history.find(&sha256(b"one"), "http://localhost:4567").is_none());
        assert!(history.find(&sha256(b"two"), "https://s.test").is_none());
    }

    #[test]
    fn latest_for_file_is_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        let mut history = History::load(&dir.path().join("uploads.json"));
        history.add("h1".into(), record("a.json", "s", "2026-11-23 10:00:00"));
        history.add("h2".into(), record("a.json", "s", "2026-11-23 11:00:00"));
        assert_eq!(history.latest_for_file("A.json", "s").unwrap().at, "2026-11-23 11:00:00");
    }
}
