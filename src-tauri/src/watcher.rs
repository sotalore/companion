//! Watching the game's `Lua/` folder for export files.
//!
//! Windows tells us when a file in the folder changes (no polling, so no CPU while idle). The game
//! writes a file whole, but the events come in bursts and we can see a file half-written, so the
//! debouncer waits until a file has been quiet for a moment, then hands each changed file to the upload
//! queue once. The queue's worker (`uploader.rs`) reads the file and decides what to do with it.

use crate::exports;
use notify_debouncer_full::notify::{event::ModifyKind, EventKind, RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{new_debouncer, DebounceEventResult, Debouncer, RecommendedCache};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

/// How long a file must be quiet before we look at it.
const QUIET_FOR: Duration = Duration::from_millis(1500);

/// A file for the upload queue.
#[derive(Debug, Clone)]
pub struct Job {
    pub path: PathBuf,
    /// `true` when the user pressed Upload: send it even if it was sent before.
    pub manual: bool,
}

/// The running watcher. Dropping it stops the watching (Rust runs cleanup when a value goes out of
/// scope, so there's no `stop` to forget).
pub type FolderWatcher = Debouncer<RecommendedWatcher, RecommendedCache>;

/// Starts watching `folder` (not its subfolders) and sends each changed export file to `queue`.
pub fn watch(folder: &Path, queue: UnboundedSender<Job>) -> Result<FolderWatcher, String> {
    // The closure runs on the debouncer's own thread each time a batch of events is ready.
    // `move` gives it its own copy of `queue`, like a Ruby block that keeps the variable alive.
    let handler = move |result: DebounceEventResult| match result {
        Ok(events) => {
            let mut sent = HashSet::new();
            for event in events {
                if !is_content_change(&event.kind) {
                    continue;
                }
                for path in &event.paths {
                    if exports::is_candidate(path) && sent.insert(path.clone()) {
                        let _ = queue.send(Job { path: path.clone(), manual: false });
                    }
                }
            }
        }
        Err(errors) => {
            for e in errors {
                eprintln!("watching the Lua folder: {e}");
            }
        }
    };
    let mut debouncer =
        new_debouncer(QUIET_FOR, None, handler).map_err(|e| format!("Couldn't start watching: {e}"))?;
    debouncer
        .watch(folder, RecursiveMode::NonRecursive)
        .map_err(|e| format!("Couldn't watch {}: {e}", folder.display()))?;
    Ok(debouncer)
}

/// A file was made, written, or renamed into place. Changes to only its times or attributes don't count.
fn is_content_change(kind: &EventKind) -> bool {
    match kind {
        EventKind::Create(_) => true,
        EventKind::Modify(ModifyKind::Metadata(_)) => false,
        EventKind::Modify(_) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn sends_an_export_written_to_the_folder_once() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _watcher = watch(dir.path(), tx).unwrap();

        // Write it in two parts, as a slow writer might, and a file we don't care about.
        let path = dir.path().join("sotalore-recipes-new.json");
        std::fs::write(&path, r#"{"type":"recipes","#).unwrap();
        std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        std::fs::write(&path, r#"{"type":"recipes","recipes":[]}"#).unwrap();

        let job = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap();
        assert_eq!(job.path.file_name().unwrap(), "sotalore-recipes-new.json");
        assert!(!job.manual);
        // Nothing else: not the second write, not notes.txt.
        assert!(tokio::time::timeout(Duration::from_secs(3), rx.recv()).await.is_err());
    }
}
