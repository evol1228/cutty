//! Debounced background autosave and the crash-recovery scan.
//!
//! ## Disk layout
//!
//! Autosaves live in one directory (the app passes
//! `<XDG state dir>/cutty/autosave`), one *slot* per project:
//!
//! - `<key>.cutty` — a regular project file (absolute media paths, so the
//!   autosave is valid regardless of where it sits)
//! - `<key>.meta.json` — `{ "projectPath": "/abs/project.cutty" | null }`,
//!   linking the slot back to the project it protects
//!
//! `key` is [`slot_key`]: `untitled` for never-saved projects, else a slug
//! plus hash of the project path.
//!
//! ## Scheduling
//!
//! [`Autosaver::mark_dirty`] is called after every committed command. It
//! only moves a deadline under a mutex — O(1), never blocks the caller.
//! The worker thread wakes at the deadline: `debounce` after the *last*
//! command, but never later than `max_interval` after the *first* unsaved
//! one (so continuous editing still autosaves periodically). At the
//! deadline it pulls a snapshot via the `tick` callback and writes (or, if
//! the project turned out clean, deletes the stale slot).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::project_file::{self, atomic_write};

/// Autosave timing. Defaults: write 2s after the last committed command,
/// but at most 20s after the first unsaved one.
#[derive(Debug, Clone, Copy)]
pub struct AutosaveConfig {
    /// Quiet time after the last change before writing.
    pub debounce: Duration,
    /// Upper bound on how long unsaved work may sit before a write, even
    /// while changes keep streaming in.
    pub max_interval: Duration,
}

impl Default for AutosaveConfig {
    fn default() -> Self {
        Self {
            debounce: Duration::from_secs(2),
            max_interval: Duration::from_secs(20),
        }
    }
}

/// What the worker should do at a deadline, as answered by the `tick`
/// callback (the app compares current state against the last save).
pub enum AutosaveTick {
    /// Unsaved changes exist: write this snapshot to its slot.
    Write {
        key: String,
        /// The project file this autosave belongs to; `None` when the
        /// project has never been saved.
        project_path: Option<PathBuf>,
        /// Serialized project (from [`project_file::serialize`] with no
        /// project dir, i.e. absolute media paths).
        json: String,
    },
    /// State matches the last save: drop any stale autosave in this slot
    /// (e.g. the user undid back to the saved state, then crashed — there
    /// is nothing to recover).
    Clean { key: String },
}

/// Outcome reported after each worker action, for the UI indicator.
#[derive(Debug, Clone)]
pub enum AutosaveEvent {
    /// Autosave written successfully (epoch ms).
    Written { at_ms: u64 },
    /// Autosave failed (disk full, permissions, …).
    Failed { message: String },
}

#[derive(Default)]
struct Sched {
    /// When the next write is due; `None` when idle.
    deadline: Option<Instant>,
    /// When the current dirty streak started (caps the deadline).
    dirty_since: Option<Instant>,
    /// Bumped by cancellation; a tick whose epoch is stale must not
    /// write (it raced an explicit save / discard / project switch).
    epoch: u64,
    shutdown: bool,
}

struct Shared {
    state: Mutex<Sched>,
    cv: Condvar,
    /// Serializes slot writes against [`Autosaver::discard_slot_now`], so
    /// "discard then close" can never lose to an in-flight write.
    /// Lock order: `write_guard` before `state`.
    write_guard: Mutex<()>,
    dir: PathBuf,
}

/// Background autosaver. Owns a worker thread for the lifetime of the app;
/// dropping it shuts the worker down (without a final write — explicit
/// save paths handle graceful exits).
pub struct Autosaver {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    config: AutosaveConfig,
}

impl Autosaver {
    /// Start the worker. `tick` is called on the worker thread at each
    /// deadline and must return quickly (snapshot + serialize); `on_event`
    /// reports write outcomes.
    pub fn start(
        dir: PathBuf,
        config: AutosaveConfig,
        tick: impl Fn() -> AutosaveTick + Send + 'static,
        on_event: impl Fn(AutosaveEvent) + Send + 'static,
    ) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(Sched::default()),
            cv: Condvar::new(),
            write_guard: Mutex::new(()),
            dir,
        });
        let worker_shared = shared.clone();
        let worker = std::thread::Builder::new()
            .name("cutty-autosave".into())
            .spawn(move || worker_loop(&worker_shared, &tick, &on_event))
            .expect("spawn autosave worker");
        Self {
            shared,
            worker: Some(worker),
            config,
        }
    }

    /// Note that the project (may have) changed. Reschedules the next
    /// write to `debounce` from now, capped at `max_interval` after the
    /// first unsaved change. Cheap and non-blocking; call it after every
    /// committed command.
    pub fn mark_dirty(&self) {
        let mut sched = self.shared.state.lock().expect("autosave state poisoned");
        let now = Instant::now();
        let since = *sched.dirty_since.get_or_insert(now);
        let deadline = (now + self.config.debounce).min(since + self.config.max_interval);
        sched.deadline = Some(deadline);
        drop(sched);
        self.shared.cv.notify_one();
    }

    /// Cancel any pending write (the user just saved explicitly, or the
    /// session switched projects). A tick already in flight is invalidated
    /// too — it will not write.
    pub fn cancel_pending(&self) {
        let mut sched = self.shared.state.lock().expect("autosave state poisoned");
        sched.deadline = None;
        sched.dirty_since = None;
        sched.epoch += 1;
    }

    /// Cancel pending work *and* delete a slot, atomically with respect to
    /// the worker: after this returns, no write racing the call can
    /// resurrect the slot. Used on explicit save and on "Don't Save".
    pub fn discard_slot_now(&self, key: &str) {
        let _guard = self
            .shared
            .write_guard
            .lock()
            .expect("autosave write guard poisoned");
        self.cancel_pending();
        discard_slot(&self.shared.dir, key);
    }
}

impl Drop for Autosaver {
    fn drop(&mut self) {
        {
            let mut sched = self.shared.state.lock().expect("autosave state poisoned");
            sched.shutdown = true;
        }
        self.shared.cv.notify_one();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn worker_loop(
    shared: &Shared,
    tick: &(impl Fn() -> AutosaveTick + Send + 'static),
    on_event: &(impl Fn(AutosaveEvent) + Send + 'static),
) {
    loop {
        // Sleep until the current deadline (which mark_dirty may keep
        // pushing back) or shutdown.
        let epoch = {
            let mut sched = shared.state.lock().expect("autosave state poisoned");
            loop {
                if sched.shutdown {
                    return;
                }
                match sched.deadline {
                    None => sched = shared.cv.wait(sched).expect("autosave state poisoned"),
                    Some(deadline) => {
                        let now = Instant::now();
                        if deadline <= now {
                            break;
                        }
                        let (guard, _) = shared
                            .cv
                            .wait_timeout(sched, deadline - now)
                            .expect("autosave state poisoned");
                        sched = guard;
                    }
                }
            }
            sched.deadline = None;
            sched.dirty_since = None;
            sched.epoch
        };

        let directive = tick();

        // Perform the disk action under the write guard, unless a
        // cancellation (save / discard / project switch) landed after
        // this tick was scheduled.
        let _guard = shared
            .write_guard
            .lock()
            .expect("autosave write guard poisoned");
        let stale = shared.state.lock().expect("autosave state poisoned").epoch != epoch;
        if stale {
            continue;
        }
        match directive {
            AutosaveTick::Write {
                key,
                project_path,
                json,
            } => {
                let result =
                    write_slot(&shared.dir, &key, project_path.as_deref(), json.as_bytes());
                on_event(match result {
                    Ok(()) => AutosaveEvent::Written { at_ms: epoch_ms() },
                    Err(e) => AutosaveEvent::Failed {
                        message: e.to_string(),
                    },
                });
            }
            AutosaveTick::Clean { key } => discard_slot(&shared.dir, &key),
        }
    }
}

// ---------------------------------------------------------------------
// Slots on disk
// ---------------------------------------------------------------------

/// Sidecar linking an autosave slot to its project file.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SlotMeta {
    project_path: Option<String>,
}

/// The autosave slot key for a project path (`None` = never saved).
/// Stable across runs; distinct paths get distinct keys.
pub fn slot_key(project_path: Option<&Path>) -> String {
    match project_path {
        None => "untitled".to_string(),
        Some(path) => {
            let text = path.to_string_lossy();
            let slug: String = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() {
                        c.to_ascii_lowercase()
                    } else {
                        '-'
                    }
                })
                .take(32)
                .collect();
            format!("{slug}-{:016x}", fnv1a64(text.as_bytes()))
        }
    }
}

/// FNV-1a, 64-bit: tiny, dependency-free, stable across builds (unlike
/// `DefaultHasher`, which documents no cross-version stability).
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn slot_paths(dir: &Path, key: &str) -> (PathBuf, PathBuf) {
    (
        dir.join(format!("{key}.cutty")),
        dir.join(format!("{key}.meta.json")),
    )
}

fn write_slot(
    dir: &Path,
    key: &str,
    project_path: Option<&Path>,
    json: &[u8],
) -> std::io::Result<()> {
    let (autosave, meta) = slot_paths(dir, key);
    let meta_json = serde_json::to_string(&SlotMeta {
        project_path: project_path.map(|p| p.to_string_lossy().into_owned()),
    })
    .expect("slot meta serializes");
    // Autosave first: a crash between the two writes leaves a slot whose
    // stale/missing meta reads as "untitled", which still gets offered.
    atomic_write(&autosave, json)?;
    atomic_write(&meta, meta_json.as_bytes())
}

/// The project file an autosave slot belongs to, per its meta sidecar
/// (`None` for untitled slots or missing/corrupt meta).
pub fn slot_project_path(dir: &Path, key: &str) -> Option<PathBuf> {
    let (_, meta) = slot_paths(dir, key);
    std::fs::read_to_string(meta)
        .ok()
        .and_then(|s| serde_json::from_str::<SlotMeta>(&s).ok())
        .and_then(|m| m.project_path)
        .map(PathBuf::from)
}

/// Remove an autosave slot (both files). Missing files are fine.
pub fn discard_slot(dir: &Path, key: &str) {
    let (autosave, meta) = slot_paths(dir, key);
    let _ = std::fs::remove_file(autosave);
    let _ = std::fs::remove_file(meta);
}

// ---------------------------------------------------------------------
// Crash-recovery scan
// ---------------------------------------------------------------------

/// An autosave that holds work newer than what is on disk.
#[derive(Debug, Clone)]
pub struct RecoveryCandidate {
    pub key: String,
    /// The autosaved `.cutty` file itself.
    pub autosave_path: PathBuf,
    /// The project the work belongs to; `None` for never-saved projects.
    pub project_path: Option<PathBuf>,
    /// Autosave mtime, ms since the Unix epoch.
    pub modified_ms: u64,
}

/// Scan the autosave directory for slots worth offering to restore,
/// newest first. Slots that are *not* worth restoring are pruned on the
/// spot: corrupt files, empty untitled projects, autosaves older than
/// their project file, and autosaves whose content matches the project
/// file exactly.
pub fn scan_recovery(dir: &Path) -> Vec<RecoveryCandidate> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new(); // no autosave dir yet
    };
    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let autosave_path = entry.path();
        if autosave_path.extension().and_then(|e| e.to_str()) != Some("cutty") {
            continue;
        }
        let Some(key) = autosave_path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_owned)
        else {
            continue;
        };
        match assess_slot(dir, &key, &autosave_path) {
            Some(candidate) => candidates.push(candidate),
            None => discard_slot(dir, &key),
        }
    }
    candidates.sort_by_key(|c| std::cmp::Reverse(c.modified_ms));
    candidates
}

/// Decide whether one slot is a genuine recovery candidate. `None` means
/// "prune it".
fn assess_slot(dir: &Path, key: &str, autosave_path: &Path) -> Option<RecoveryCandidate> {
    let autosaved = project_file::load(autosave_path).ok()?; // corrupt → prune
    let project_path = slot_project_path(dir, key);
    let modified_ms = mtime_ms(autosave_path)?;

    match &project_path {
        Some(path) if path.is_file() => {
            // Older than the project file → the save superseded it.
            if mtime_ms(path).is_some_and(|project_ms| modified_ms <= project_ms) {
                return None;
            }
            // Content identical to the project file → nothing to recover
            // (e.g. the process died between an explicit save and the
            // autosave cleanup).
            if let Ok(saved) = project_file::load(path) {
                if saved == autosaved {
                    return None;
                }
            }
        }
        // Orphan (never saved, or the project file vanished): only worth
        // offering when there is actual content.
        _ => {
            let empty =
                autosaved.media.is_empty() && autosaved.tracks.iter().all(|t| t.clips.is_empty());
            if empty {
                return None;
            }
        }
    }

    Some(RecoveryCandidate {
        key: key.to_string(),
        autosave_path: autosave_path.to_path_buf(),
        project_path,
        modified_ms,
    })
}

/// The recovery candidate for one specific project path, if any (used
/// when opening a project: its autosave may hold newer work).
pub fn recovery_for(dir: &Path, project_path: &Path) -> Option<RecoveryCandidate> {
    let key = slot_key(Some(project_path));
    let (autosave_path, _) = slot_paths(dir, &key);
    if !autosave_path.is_file() {
        return None;
    }
    match assess_slot(dir, &key, &autosave_path) {
        Some(c) => Some(c),
        None => {
            discard_slot(dir, &key);
            None
        }
    }
}

fn mtime_ms(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let since = modified.duration_since(SystemTime::UNIX_EPOCH).ok()?;
    Some(u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_keys_are_stable_and_distinct() {
        assert_eq!(slot_key(None), "untitled");
        let a = slot_key(Some(Path::new("/home/u/short.cutty")));
        let b = slot_key(Some(Path::new("/home/u/other.cutty")));
        assert_eq!(a, slot_key(Some(Path::new("/home/u/short.cutty"))));
        assert_ne!(a, b);
        assert!(a.starts_with("short-"));
    }

    #[test]
    fn fnv_matches_reference_vector() {
        // Known FNV-1a 64 test vector.
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
