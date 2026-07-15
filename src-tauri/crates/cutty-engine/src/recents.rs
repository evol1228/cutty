//! The recent-projects list, persisted as `recents.json` in the app's
//! state directory. Most recent first, deduplicated by path, capped at
//! [`MAX_RECENTS`]. Entries whose file has vanished are kept on disk (the
//! project might live on an unplugged drive) — callers filter for
//! existence at display time.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::project_file::atomic_write;

/// Maximum number of remembered projects.
pub const MAX_RECENTS: usize = 10;

/// One remembered project.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentProject {
    /// Absolute path of the `.cutty` file.
    pub path: String,
    /// When it was last opened or saved, ms since the Unix epoch.
    pub opened_at_ms: u64,
}

fn recents_file(state_dir: &Path) -> PathBuf {
    state_dir.join("recents.json")
}

/// Read the list (most recent first). Missing or corrupt files read as
/// empty — recents are best-effort state, never an error source.
pub fn list(state_dir: &Path) -> Vec<RecentProject> {
    std::fs::read_to_string(recents_file(state_dir))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Record that `path` was just opened or saved; returns the new list.
pub fn push(state_dir: &Path, path: &Path) -> Vec<RecentProject> {
    let path_str = path.to_string_lossy().into_owned();
    let mut entries = list(state_dir);
    entries.retain(|e| e.path != path_str);
    entries.insert(
        0,
        RecentProject {
            path: path_str,
            opened_at_ms: epoch_ms(),
        },
    );
    entries.truncate(MAX_RECENTS);
    write(state_dir, &entries);
    entries
}

/// Drop `path` from the list (e.g. the user clicked a recent that no
/// longer exists); returns the new list.
pub fn remove(state_dir: &Path, path: &Path) -> Vec<RecentProject> {
    let path_str = path.to_string_lossy();
    let mut entries = list(state_dir);
    entries.retain(|e| e.path != path_str);
    write(state_dir, &entries);
    entries
}

fn write(state_dir: &Path, entries: &[RecentProject]) {
    if let Ok(json) = serde_json::to_string_pretty(entries) {
        // Best-effort: a failed write costs a stale recents list, nothing
        // more.
        let _ = atomic_write(&recents_file(state_dir), json.as_bytes());
    }
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}
