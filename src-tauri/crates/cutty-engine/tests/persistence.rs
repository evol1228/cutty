//! Save/load, autosave, and crash-recovery behavior — the persistence
//! half of the Phase 1 acceptance criteria.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use cutty_engine::autosave::{
    discard_slot, recovery_for, scan_recovery, slot_key, AutosaveConfig, AutosaveEvent,
    AutosaveTick, Autosaver,
};
use cutty_engine::{project_file, recents, Engine, Project, ProjectSettings, TrackKind};
use tempfile::TempDir;

/// A project dir holding a media file and an engine that references it
/// with a couple of clips — the standard save/load fixture.
fn project_on_disk(dir: &Path) -> (Engine, PathBuf) {
    let media_path = dir.join("footage.mp4");
    fs::write(&media_path, b"not really video").expect("write media");
    let mut engine = Engine::new(ProjectSettings::default());
    let media = engine
        .add_media(media_path.to_string_lossy(), 10.0, true, true)
        .expect("add media");
    let video = common::track_of_kind(&engine, TrackKind::Video);
    let clip = engine.add_clip(video, media, 0.0, 1.0, 6.0).expect("clip");
    engine.split_clip(clip, 2.5).expect("split");
    (engine, dir.join("edit.cutty"))
}

#[test]
fn save_load_round_trip_is_identical() {
    let tmp = TempDir::new().expect("tempdir");
    let (engine, project_path) = project_on_disk(tmp.path());

    project_file::save(engine.project(), &project_path).expect("save");
    let loaded = project_file::load(&project_path).expect("load");

    // The in-memory model survives the disk round trip exactly.
    assert_eq!(&loaded, engine.project());

    // And serialization is deterministic: re-saving the loaded project
    // produces byte-identical file content.
    let first = fs::read_to_string(&project_path).expect("read");
    project_file::save(&loaded, &project_path).expect("re-save");
    let second = fs::read_to_string(&project_path).expect("re-read");
    assert_eq!(first, second);
}

#[test]
fn saved_file_stores_relative_media_paths() {
    let tmp = TempDir::new().expect("tempdir");
    let (engine, project_path) = project_on_disk(tmp.path());
    project_file::save(engine.project(), &project_path).expect("save");

    let text = fs::read_to_string(&project_path).expect("read");
    assert!(
        text.contains(r#""relativePath": "footage.mp4""#),
        "media next to the project file should be stored relative:\n{text}"
    );
}

#[test]
fn moved_project_folder_finds_media_via_relative_path() {
    let tmp = TempDir::new().expect("tempdir");
    let original = tmp.path().join("original");
    fs::create_dir_all(&original).expect("mkdir");
    let (engine, project_path) = project_on_disk(&original);
    project_file::save(engine.project(), &project_path).expect("save");

    // Move the whole folder — project file and media together.
    let moved = tmp.path().join("moved");
    fs::rename(&original, &moved).expect("move folder");

    let loaded = project_file::load(&moved.join("edit.cutty")).expect("load");
    let expected = moved.join("footage.mp4");
    assert_eq!(loaded.media[0].path, expected.to_string_lossy());
    // Everything but the media path is untouched.
    assert_eq!(loaded.tracks, engine.project().tracks);
}

#[test]
fn absolute_path_is_the_fallback_when_relative_breaks() {
    let tmp = TempDir::new().expect("tempdir");
    // Media lives *outside* the project folder.
    let media_path = tmp.path().join("elsewhere.mp4");
    fs::write(&media_path, b"x").expect("write media");
    let project_dir = tmp.path().join("proj");
    fs::create_dir_all(&project_dir).expect("mkdir");

    let mut engine = Engine::new(ProjectSettings::default());
    engine
        .add_media(media_path.to_string_lossy(), 5.0, true, false)
        .expect("add media");
    let project_path = project_dir.join("edit.cutty");
    project_file::save(engine.project(), &project_path).expect("save");

    // Move only the project file; the stored relative path now dangles
    // but the absolute one still resolves.
    let moved = tmp.path().join("moved-alone");
    fs::create_dir_all(&moved).expect("mkdir");
    let moved_project = moved.join("edit.cutty");
    fs::rename(&project_path, &moved_project).expect("move project");

    let loaded = project_file::load(&moved_project).expect("load");
    assert_eq!(loaded.media[0].path, media_path.to_string_lossy());
}

#[test]
fn project_with_missing_media_still_loads() {
    let tmp = TempDir::new().expect("tempdir");
    let (engine, project_path) = project_on_disk(tmp.path());
    project_file::save(engine.project(), &project_path).expect("save");

    fs::remove_file(tmp.path().join("footage.mp4")).expect("delete media");

    let loaded = project_file::load(&project_path).expect("project must open anyway");
    // The stored absolute path is kept so the UI can mark it missing.
    assert_eq!(
        loaded.media[0].path,
        tmp.path().join("footage.mp4").to_string_lossy()
    );
    assert_eq!(loaded.tracks[0].clips.len(), 2);
}

#[test]
fn undo_history_is_session_only() {
    let tmp = TempDir::new().expect("tempdir");
    let (engine, project_path) = project_on_disk(tmp.path());
    assert!(engine.undo_depth() > 0, "fixture performed undoable work");
    project_file::save(engine.project(), &project_path).expect("save");

    // The file format has no history fields at all…
    let text = fs::read_to_string(&project_path).expect("read");
    assert!(!text.contains("undo"), "no undo state in the file:\n{text}");

    // …and a freshly loaded engine starts with empty stacks.
    let loaded = project_file::load(&project_path).expect("load");
    let mut reopened = Engine::from_project(loaded).expect("adopt");
    assert_eq!(reopened.undo_depth(), 0);
    assert_eq!(reopened.redo_depth(), 0);
    assert!(
        !reopened.undo().expect("undo"),
        "nothing to undo after load"
    );
}

// -----------------------------------------------------------------------
// Autosave + crash recovery
// -----------------------------------------------------------------------

/// Fast autosave timings so the tests run in tens of milliseconds.
fn fast_config() -> AutosaveConfig {
    AutosaveConfig {
        debounce: Duration::from_millis(60),
        max_interval: Duration::from_millis(400),
    }
}

/// Pins the acceptance contract: a crash loses at most ~2 seconds of
/// quiet-period work (and never more than 20 seconds under continuous
/// editing).
#[test]
fn default_autosave_config_matches_the_acceptance_contract() {
    let config = AutosaveConfig::default();
    assert_eq!(config.debounce, Duration::from_secs(2));
    assert_eq!(config.max_interval, Duration::from_secs(20));
}

fn wait_for<T>(rx: &mpsc::Receiver<T>, timeout: Duration) -> Option<T> {
    rx.recv_timeout(timeout).ok()
}

#[test]
fn autosave_writes_after_debounce_and_recovers_after_kill() {
    let tmp = TempDir::new().expect("tempdir");
    let autosave_dir = tmp.path().join("autosave");
    let (engine, _) = project_on_disk(tmp.path());
    let snapshot = engine.project().clone();

    let (tx, rx) = mpsc::channel();
    let tick_project = snapshot.clone();
    let autosaver = Autosaver::start(
        autosave_dir.clone(),
        fast_config(),
        move || AutosaveTick::Write {
            key: slot_key(None),
            project_path: None,
            json: project_file::serialize(&tick_project, None),
        },
        move |event| {
            let _ = tx.send(event);
        },
    );

    // A burst of "commands", then quiet — one write lands ~debounce later.
    let start = Instant::now();
    for _ in 0..5 {
        autosaver.mark_dirty();
        std::thread::sleep(Duration::from_millis(5));
    }
    let event = wait_for(&rx, Duration::from_secs(5)).expect("autosave fires");
    assert!(matches!(event, AutosaveEvent::Written { .. }));
    assert!(
        start.elapsed() >= Duration::from_millis(60),
        "wrote before the debounce elapsed"
    );

    // Simulate kill -9: no shutdown, no cleanup — the process just ends.
    // (Dropping the autosaver joins the thread but deletes nothing, which
    // is exactly the on-disk state a SIGKILL leaves behind.)
    drop(autosaver);

    // Relaunch: the scan offers the orphaned autosave…
    let candidates = scan_recovery(&autosave_dir);
    assert_eq!(candidates.len(), 1);
    let candidate = &candidates[0];
    assert_eq!(candidate.key, "untitled");
    assert_eq!(candidate.project_path, None);

    // …and restoring it reproduces the pre-crash project exactly.
    let recovered = project_file::load(&candidate.autosave_path).expect("restore");
    assert_eq!(recovered, snapshot);
}

#[test]
fn continuous_editing_still_autosaves_via_the_interval_cap() {
    let tmp = TempDir::new().expect("tempdir");
    let (tx, rx) = mpsc::channel();
    let project = Engine::new(ProjectSettings::default()).project().clone();
    let autosaver = Autosaver::start(
        tmp.path().join("autosave"),
        fast_config(),
        move || AutosaveTick::Write {
            key: "untitled".into(),
            project_path: None,
            json: project_file::serialize(&project, None),
        },
        move |event| {
            let _ = tx.send(event);
        },
    );

    // Keep marking dirty faster than the debounce forever; the
    // max_interval cap must force a write anyway.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut written = false;
    while Instant::now() < deadline {
        autosaver.mark_dirty();
        if rx.try_recv().is_ok() {
            written = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(written, "interval cap never forced a write");
}

#[test]
fn clean_tick_prunes_the_stale_slot() {
    let tmp = TempDir::new().expect("tempdir");
    let dir = tmp.path().join("autosave");
    let project = Engine::new(ProjectSettings::default()).project().clone();

    // Round 1: dirty → slot written.
    let (tx, rx) = mpsc::channel();
    let p = project.clone();
    let dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let dirty_flag = dirty.clone();
    let autosaver = Autosaver::start(
        dir.clone(),
        fast_config(),
        move || {
            if dirty_flag.load(std::sync::atomic::Ordering::SeqCst) {
                AutosaveTick::Write {
                    key: "untitled".into(),
                    project_path: None,
                    json: project_file::serialize(&p, None),
                }
            } else {
                AutosaveTick::Clean {
                    key: "untitled".into(),
                }
            }
        },
        move |e| {
            let _ = tx.send(e);
        },
    );
    autosaver.mark_dirty();
    wait_for(&rx, Duration::from_secs(5)).expect("write");
    assert!(dir.join("untitled.cutty").is_file());

    // Round 2: project returned to the saved state (undo) → slot pruned.
    dirty.store(false, std::sync::atomic::Ordering::SeqCst);
    autosaver.mark_dirty();
    let gone = Instant::now() + Duration::from_secs(5);
    while dir.join("untitled.cutty").exists() && Instant::now() < gone {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !dir.join("untitled.cutty").exists(),
        "stale slot not pruned"
    );
}

fn write_slot_for_test(dir: &Path, key: &str, project: &Project, project_path: Option<&Path>) {
    fs::create_dir_all(dir).expect("mkdir");
    fs::write(
        dir.join(format!("{key}.cutty")),
        project_file::serialize(project, None),
    )
    .expect("write autosave");
    let meta = match project_path {
        Some(p) => format!(r#"{{"projectPath":"{}"}}"#, p.to_string_lossy()),
        None => r#"{"projectPath":null}"#.to_string(),
    };
    fs::write(dir.join(format!("{key}.meta.json")), meta).expect("write meta");
}

#[test]
fn scan_prunes_empty_orphans_stale_and_identical_slots() {
    let tmp = TempDir::new().expect("tempdir");
    let dir = tmp.path().join("autosave");
    let (engine, project_path) = project_on_disk(tmp.path());
    let edited = engine.project().clone();
    let empty = Engine::new(ProjectSettings::default()).project().clone();

    // 1. Empty untitled orphan → pruned.
    write_slot_for_test(&dir, "untitled", &empty, None);

    // 2. Autosave identical to its (existing) project file → pruned.
    project_file::save(&edited, &project_path).expect("save");
    let key_same = slot_key(Some(&project_path));
    write_slot_for_test(&dir, &key_same, &edited, Some(&project_path));

    // 3. Corrupt autosave → pruned.
    fs::write(dir.join("broken.cutty"), "{ nope").expect("write corrupt");

    let candidates = scan_recovery(&dir);
    assert!(
        candidates.is_empty(),
        "expected all slots pruned, got {candidates:?}"
    );
    assert!(!dir.join("untitled.cutty").exists());
    assert!(!dir.join(format!("{key_same}.cutty")).exists());
    assert!(!dir.join("broken.cutty").exists());
}

#[test]
fn scan_offers_autosave_newer_than_its_project_file() {
    let tmp = TempDir::new().expect("tempdir");
    let dir = tmp.path().join("autosave");
    let (mut engine, project_path) = project_on_disk(tmp.path());
    project_file::save(engine.project(), &project_path).expect("save");

    // More work after the save, captured only by the autosave. Ensure the
    // autosave mtime is strictly newer than the project file's.
    std::thread::sleep(Duration::from_millis(20));
    let clip = engine.project().tracks[0].clips[0].id;
    engine.ripple_delete(clip).expect("ripple delete");
    let key = slot_key(Some(&project_path));
    write_slot_for_test(&dir, &key, engine.project(), Some(&project_path));

    let candidates = scan_recovery(&dir);
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].project_path.as_deref(), Some(&*project_path));

    // The per-project lookup (used when opening that project) agrees.
    let single = recovery_for(&dir, &project_path).expect("offer");
    assert_eq!(single.key, key);

    // Restoring reproduces the unsaved work.
    let recovered = project_file::load(&candidates[0].autosave_path).expect("load");
    assert_eq!(&recovered, engine.project());
    assert_ne!(recovered, project_file::load(&project_path).expect("old"));

    // Discarding leaves nothing behind.
    discard_slot(&dir, &key);
    assert!(scan_recovery(&dir).is_empty());
}

// -----------------------------------------------------------------------
// Recents
// -----------------------------------------------------------------------

#[test]
fn recents_dedupe_order_and_cap() {
    let tmp = TempDir::new().expect("tempdir");
    let dir = tmp.path();

    assert!(recents::list(dir).is_empty());
    for i in 0..12 {
        recents::push(dir, &dir.join(format!("p{i}.cutty")));
    }
    let entries = recents::list(dir);
    assert_eq!(entries.len(), recents::MAX_RECENTS);
    assert!(entries[0].path.ends_with("p11.cutty"), "newest first");

    // Re-opening an older entry moves it to the front without duplicating.
    recents::push(dir, &dir.join("p5.cutty"));
    let entries = recents::list(dir);
    assert_eq!(entries.len(), recents::MAX_RECENTS);
    assert!(entries[0].path.ends_with("p5.cutty"));
    let count = entries
        .iter()
        .filter(|e| e.path.ends_with("p5.cutty"))
        .count();
    assert_eq!(count, 1);

    let removed = recents::remove(dir, &dir.join("p5.cutty"));
    assert!(!removed.iter().any(|e| e.path.ends_with("p5.cutty")));
}
