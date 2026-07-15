//! Project persistence IPC: save/load/new, session dirty state, autosave
//! wiring, crash recovery, and the recent-projects list. All real logic
//! lives in `cutty_engine::{project_file, autosave, recents}` — this file
//! only wires it to Tauri state and events.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use cutty_engine::autosave::{self, AutosaveConfig, AutosaveEvent, AutosaveTick, Autosaver};
use cutty_engine::{project_file, recents, Engine, Project, ProjectSettings};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::engine_ipc::EngineHandle;

/// Session metadata (`ProjectMeta`), emitted after every mutation and
/// every save/load/new.
pub const PROJECT_META_EVENT: &str = "project://meta";
/// Autosave outcomes (`AutosavePayload`), emitted by the worker thread.
pub const AUTOSAVE_EVENT: &str = "project://autosave";
/// Emitted instead of closing when the window's close button is pressed
/// with unsaved changes; the frontend runs the save/discard/cancel dialog
/// and destroys the window itself.
pub const CLOSE_REQUESTED_EVENT: &str = "project://close-requested";

/// Where the current project lives and what its last-saved state was.
pub struct Session {
    /// Path of the `.cutty` file; `None` until the first save.
    pub path: Option<PathBuf>,
    /// Project as of the last save/load/new. Dirty ⇔ current ≠ saved,
    /// which makes "undo back to the save point" read as clean again.
    pub saved: Project,
}

/// Managed state: the session plus resolved state directories.
///
/// Lock order across the app: engine before session — never acquire the
/// engine lock while holding the session lock.
pub struct SessionState {
    pub session: Arc<Mutex<Session>>,
    /// `~/.local/state/cutty`; `None` if the environment has no home
    /// (autosave/recents degrade gracefully to disabled).
    state_root: Option<PathBuf>,
}

impl SessionState {
    pub fn new(initial: &Project) -> Self {
        Self {
            session: Arc::new(Mutex::new(Session {
                path: None,
                saved: initial.clone(),
            })),
            state_root: cutty_engine::state_dir(),
        }
    }

    fn state_root(&self) -> Option<&Path> {
        self.state_root.as_deref()
    }

    fn autosave_dir(&self) -> Option<PathBuf> {
        self.state_root.as_ref().map(|r| r.join("autosave"))
    }
}

/// Managed wrapper around the running autosave worker (absent when no
/// state directory could be resolved).
pub struct AutosaverHandle(pub Autosaver);

/// Project/session metadata mirrored by the frontend top bar.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectMeta {
    pub path: Option<String>,
    /// File stem, or "Untitled Project".
    pub name: String,
    pub dirty: bool,
}

/// Autosave outcome for the UI indicator.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutosavePayload {
    /// Epoch ms of a successful write; `None` on failure.
    pub at_ms: Option<u64>,
    pub error: Option<String>,
}

/// A recoverable autosave offered to the user.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryOffer {
    pub key: String,
    pub project_path: Option<String>,
    /// Display name (project file stem, or "Untitled Project").
    pub name: String,
    /// Autosave mtime, epoch ms.
    pub modified_ms: u64,
}

/// One recent-projects entry, existence-checked at read time.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentEntry {
    pub path: String,
    pub name: String,
    pub exists: bool,
    pub opened_at_ms: u64,
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadResult {
    pub meta: ProjectMeta,
    /// A newer autosave exists for this project — offer to restore it.
    pub recovery: Option<RecoveryOffer>,
}

fn display_name(path: Option<&Path>) -> String {
    path.and_then(|p| p.file_stem())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Untitled Project".to_string())
}

fn meta_of(session: &Session, project: &Project) -> ProjectMeta {
    ProjectMeta {
        path: session
            .path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        name: display_name(session.path.as_deref()),
        dirty: *project != session.saved,
    }
}

fn offer_of(candidate: &autosave::RecoveryCandidate) -> RecoveryOffer {
    RecoveryOffer {
        key: candidate.key.clone(),
        project_path: candidate
            .project_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        name: display_name(candidate.project_path.as_deref()),
        modified_ms: candidate.modified_ms,
    }
}

/// The pristine project a fresh session starts with (also the dirty
/// baseline for never-saved sessions).
fn pristine_project() -> Project {
    Engine::new(ProjectSettings::default()).project().clone()
}

// -----------------------------------------------------------------------
// Hooks called from the rest of the app
// -----------------------------------------------------------------------

/// Called by the engine IPC layer after every committed or transient
/// change: keep the frontend's meta current and schedule an autosave.
pub fn notify_mutation(app: &AppHandle, project: &Project) {
    if let Some(state) = app.try_state::<SessionState>() {
        let session = state.session.lock().expect("session state poisoned");
        let _ = app.emit(PROJECT_META_EVENT, meta_of(&session, project));
    }
    if let Some(autosaver) = app.try_state::<AutosaverHandle>() {
        autosaver.0.mark_dirty();
    }
}

/// Whether the current project differs from its last-saved state (drives
/// the window-close guard).
pub fn has_unsaved_changes(app: &AppHandle) -> bool {
    let project = {
        let engine = app.state::<EngineHandle>();
        let guard = engine.0.lock().expect("engine state poisoned");
        guard.project().clone()
    };
    let state = app.state::<SessionState>();
    let session = state.session.lock().expect("session state poisoned");
    project != session.saved
}

/// Start the autosave worker (setup hook). Skipped with a warning when no
/// XDG state directory can be resolved.
pub fn start_autosaver(app: &AppHandle) {
    let session_state = app.state::<SessionState>();
    let Some(dir) = session_state.autosave_dir() else {
        eprintln!("cutty: no XDG state directory — autosave disabled");
        return;
    };
    let engine = app.state::<EngineHandle>().0.clone();
    let session = session_state.session.clone();
    let event_app = app.clone();

    let autosaver = Autosaver::start(
        dir,
        AutosaveConfig::default(),
        move || {
            // Snapshot under brief locks (engine before session), then
            // serialize lock-free. Projects are a few KB — microseconds.
            let project = engine
                .lock()
                .expect("engine state poisoned")
                .project()
                .clone();
            let (key, project_path, saved) = {
                let session = session.lock().expect("session state poisoned");
                (
                    autosave::slot_key(session.path.as_deref()),
                    session.path.clone(),
                    session.saved.clone(),
                )
            };
            if project == saved {
                AutosaveTick::Clean { key }
            } else {
                AutosaveTick::Write {
                    key,
                    project_path,
                    json: project_file::serialize(&project, None),
                }
            }
        },
        move |event| {
            let payload = match event {
                AutosaveEvent::Written { at_ms } => AutosavePayload {
                    at_ms: Some(at_ms),
                    error: None,
                },
                AutosaveEvent::Failed { message } => AutosavePayload {
                    at_ms: None,
                    error: Some(message),
                },
            };
            let _ = event_app.emit(AUTOSAVE_EVENT, payload);
        },
    );
    app.manage(AutosaverHandle(autosaver));
}

fn cancel_autosave(app: &AppHandle) {
    if let Some(autosaver) = app.try_state::<AutosaverHandle>() {
        autosaver.0.cancel_pending();
    }
}

/// Cancel pending autosaves and delete `key`'s slot, immune to races with
/// the worker. Falls back to a plain delete when the worker never started.
fn discard_slot_now(app: &AppHandle, state: &SessionState, key: &str) {
    match app.try_state::<AutosaverHandle>() {
        Some(autosaver) => autosaver.0.discard_slot_now(key),
        None => {
            if let Some(dir) = state.autosave_dir() {
                autosave::discard_slot(&dir, key);
            }
        }
    }
}

// -----------------------------------------------------------------------
// Commands
// -----------------------------------------------------------------------

/// Current session metadata (initial fetch; afterwards the frontend
/// follows `project://meta` events).
#[tauri::command]
pub fn project_meta(
    engine_state: State<'_, EngineHandle>,
    session_state: State<'_, SessionState>,
) -> ProjectMeta {
    let project = {
        let engine = engine_state.0.lock().expect("engine state poisoned");
        engine.project().clone()
    };
    let session = session_state
        .session
        .lock()
        .expect("session state poisoned");
    meta_of(&session, &project)
}

/// Save the project: to `path` if given (Save As), else to the session's
/// current path. Fails when neither exists — the frontend runs the Save
/// As dialog first.
#[tauri::command]
pub fn project_save(
    app: AppHandle,
    engine_state: State<'_, EngineHandle>,
    session_state: State<'_, SessionState>,
    path: Option<String>,
) -> Result<ProjectMeta, String> {
    let project = {
        let engine = engine_state.0.lock().expect("engine state poisoned");
        engine.project().clone()
    };
    let mut session = session_state
        .session
        .lock()
        .expect("session state poisoned");
    let target = match path.map(PathBuf::from).or_else(|| session.path.clone()) {
        Some(t) if t.extension().is_none() => t.with_extension("cutty"),
        Some(t) => t,
        None => return Err("this project has no file yet — use Save As".into()),
    };
    project_file::save(&project, &target).map_err(|e| e.to_string())?;

    // The saved work no longer needs crash protection: drop the slot of
    // the identity we saved *from* and (for Save As) the target's slot.
    let old_key = autosave::slot_key(session.path.as_deref());
    let new_key = autosave::slot_key(Some(&target));
    discard_slot_now(&app, &session_state, &old_key);
    if new_key != old_key {
        discard_slot_now(&app, &session_state, &new_key);
    }

    session.path = Some(target.clone());
    session.saved = project;
    let meta = meta_of(&session, &session.saved);
    drop(session);

    if let Some(root) = session_state.state_root() {
        recents::push(root, &target);
    }
    let _ = app.emit(PROJECT_META_EVENT, meta.clone());
    Ok(meta)
}

/// Open a `.cutty` file, replacing the current project (the frontend runs
/// the unsaved-changes guard before calling). Undo history starts empty —
/// history is session-only by design. If a newer autosave exists for this
/// project, it is reported (not applied) so the UI can offer it.
#[tauri::command]
pub fn project_load(
    app: AppHandle,
    engine_state: State<'_, EngineHandle>,
    session_state: State<'_, SessionState>,
    path: String,
) -> Result<LoadResult, String> {
    let target = PathBuf::from(&path);
    let project = project_file::load(&target).map_err(|e| e.to_string())?;
    let new_engine = Engine::from_project(project.clone()).map_err(|e| e.to_string())?;

    // Session first, so the meta emitted alongside the engine snapshot
    // already names the loaded file.
    {
        let mut session = session_state
            .session
            .lock()
            .expect("session state poisoned");
        session.path = Some(target.clone());
        session.saved = project.clone();
    }
    {
        let mut engine = engine_state.0.lock().expect("engine state poisoned");
        *engine = new_engine;
        crate::engine_ipc::emit_state(&app, &mut engine);
    }
    // The snapshot emit above scheduled an autosave tick; the fresh
    // session is clean, and that tick must not prune a recovery slot the
    // user hasn't answered for yet.
    cancel_autosave(&app);

    if let Some(root) = session_state.state_root() {
        recents::push(root, &target);
    }
    let recovery = session_state
        .autosave_dir()
        .and_then(|dir| autosave::recovery_for(&dir, &target))
        .map(|c| offer_of(&c));

    let session = session_state
        .session
        .lock()
        .expect("session state poisoned");
    Ok(LoadResult {
        meta: meta_of(&session, &project),
        recovery,
    })
}

/// Replace the current project with a fresh untitled one (the frontend
/// runs the unsaved-changes guard before calling).
#[tauri::command]
pub fn project_new(
    app: AppHandle,
    engine_state: State<'_, EngineHandle>,
    session_state: State<'_, SessionState>,
) -> ProjectMeta {
    let fresh = Engine::new(ProjectSettings::default());
    let project = fresh.project().clone();
    {
        let mut session = session_state
            .session
            .lock()
            .expect("session state poisoned");
        session.path = None;
        session.saved = project.clone();
    }
    {
        let mut engine = engine_state.0.lock().expect("engine state poisoned");
        *engine = fresh;
        crate::engine_ipc::emit_state(&app, &mut engine);
    }
    cancel_autosave(&app);

    let session = session_state
        .session
        .lock()
        .expect("session state poisoned");
    meta_of(&session, &project)
}

/// The recent-projects list, newest first.
#[tauri::command]
pub fn project_recents(session_state: State<'_, SessionState>) -> Vec<RecentEntry> {
    let Some(root) = session_state.state_root() else {
        return Vec::new();
    };
    recents::list(root)
        .into_iter()
        .map(|e| RecentEntry {
            name: display_name(Some(Path::new(&e.path))),
            exists: Path::new(&e.path).is_file(),
            path: e.path,
            opened_at_ms: e.opened_at_ms,
        })
        .collect()
}

/// Drop one entry from the recents list (e.g. its file is gone).
#[tauri::command]
pub fn project_remove_recent(
    session_state: State<'_, SessionState>,
    path: String,
) -> Vec<RecentEntry> {
    if let Some(root) = session_state.state_root() {
        recents::remove(root, Path::new(&path));
    }
    project_recents(session_state)
}

/// Launch-time crash-recovery scan: autosaves newer than their project
/// file (or orphaned), newest first. Hopeless slots are pruned during the
/// scan.
#[tauri::command]
pub fn project_recovery_scan(session_state: State<'_, SessionState>) -> Vec<RecoveryOffer> {
    let Some(dir) = session_state.autosave_dir() else {
        return Vec::new();
    };
    autosave::scan_recovery(&dir).iter().map(offer_of).collect()
}

/// Restore an autosave: the recovered project becomes the live (dirty)
/// session, pointed at the original project path when there was one. The
/// slot is kept until the next explicit save, protecting against a second
/// crash.
#[tauri::command]
pub fn project_restore_autosave(
    app: AppHandle,
    engine_state: State<'_, EngineHandle>,
    session_state: State<'_, SessionState>,
    key: String,
) -> Result<ProjectMeta, String> {
    let dir = session_state
        .autosave_dir()
        .ok_or("no XDG state directory — autosave is disabled")?;
    let autosave_path = dir.join(format!("{key}.cutty"));
    let project = project_file::load(&autosave_path).map_err(|e| e.to_string())?;
    let new_engine = Engine::from_project(project.clone()).map_err(|e| e.to_string())?;
    let project_path = autosave::slot_project_path(&dir, &key);

    {
        let mut session = session_state
            .session
            .lock()
            .expect("session state poisoned");
        // Dirty baseline: the project file's on-disk content when it
        // still loads, else pristine — either way the restored state
        // compares dirty and keeps autosaving.
        session.saved = project_path
            .as_ref()
            .and_then(|p| project_file::load(p).ok())
            .unwrap_or_else(pristine_project);
        session.path = project_path;
    }
    {
        let mut engine = engine_state.0.lock().expect("engine state poisoned");
        *engine = new_engine;
        crate::engine_ipc::emit_state(&app, &mut engine);
    }

    let session = session_state
        .session
        .lock()
        .expect("session state poisoned");
    Ok(meta_of(&session, &project))
}

/// Delete an autosave slot the user declined to restore.
#[tauri::command]
pub fn project_discard_autosave(session_state: State<'_, SessionState>, key: String) {
    if let Some(dir) = session_state.autosave_dir() {
        autosave::discard_slot(&dir, &key);
    }
}

/// Delete the *current* session's autosave and cancel pending writes —
/// the "Don't Save" path of the unsaved-changes guard, immune to racing
/// the worker.
#[tauri::command]
pub fn project_discard_current_autosave(app: AppHandle, session_state: State<'_, SessionState>) {
    let key = {
        let session = session_state
            .session
            .lock()
            .expect("session state poisoned");
        autosave::slot_key(session.path.as_deref())
    };
    discard_slot_now(&app, &session_state, &key);
}
