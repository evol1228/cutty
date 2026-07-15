// Orchestration of the save/open/new/close flows: native file dialogs,
// the unsaved-changes guard, crash recovery, keyboard shortcuts, and the
// session-event wiring. All persistence logic lives in the Rust backend —
// these are UI flows only.

import {
  open as openDialog,
  save as saveDialog,
} from "@tauri-apps/plugin-dialog";
import { getCurrentWindow } from "@tauri-apps/api/window";
import * as ipc from "./projectIpc";
import { pathsExist, playbackSeek } from "./ipc";
import {
  useSessionStore,
  type GuardAction,
  type GuardChoice,
} from "../state/sessionStore";
import { useMediaStore } from "../state/mediaStore";
import { useProjectStore } from "../state/projectStore";
import { toast } from "../state/toastStore";

const CUTTY_FILTER = [{ name: "Cutty Project", extensions: ["cutty"] }];

function basename(path: string): string {
  return path.split("/").pop() ?? path;
}

/** Reset per-project UI state after the engine swapped projects. */
function afterSessionSwitch(meta: ipc.ProjectMeta): void {
  useSessionStore.getState().sessionSwitched(meta);
  useProjectStore.getState().setPlayhead(0);
  useProjectStore.getState().setSelection([]);
  void playbackSeek(0).catch(() => undefined);
}

export async function refreshRecents(): Promise<void> {
  try {
    useSessionStore.getState().setRecents(await ipc.projectRecents());
  } catch {
    // recents are cosmetic — never surface an error for them
  }
}

/**
 * Run the unsaved-changes guard for a destructive action. Resolves true
 * when it is safe to proceed (clean, saved, or explicitly discarded).
 */
async function ensureSavedOrDiscarded(action: GuardAction): Promise<boolean> {
  const store = useSessionStore.getState();
  if (!store.meta.dirty) return true;
  const choice = await new Promise<GuardChoice>((resolve) => {
    store.setGuard({ action, resolve });
  });
  useSessionStore.getState().setGuard(null);
  if (choice === "cancel") return false;
  if (choice === "save") return saveProject();
  // "Don't Save": the discarded work must not resurface as crash recovery.
  await ipc.projectDiscardCurrentAutosave().catch(() => undefined);
  return true;
}

/** Ctrl+S. Falls through to Save As for never-saved projects. */
export async function saveProject(): Promise<boolean> {
  if (useSessionStore.getState().meta.path === null) return saveProjectAs();
  try {
    const meta = await ipc.projectSave();
    useSessionStore.getState().savedNow(meta);
    return true;
  } catch (err) {
    toast(`Save failed: ${String(err)}`, "error");
    return false;
  }
}

/** Ctrl+Shift+S. Returns false when the user cancels the dialog. */
export async function saveProjectAs(): Promise<boolean> {
  const meta = useSessionStore.getState().meta;
  const picked = await saveDialog({
    title: "Save Project",
    filters: CUTTY_FILTER,
    defaultPath: meta.path ?? `${meta.name}.cutty`,
  });
  if (picked === null) return false;
  try {
    const next = await ipc.projectSave(picked);
    useSessionStore.getState().savedNow(next);
    void refreshRecents();
    return true;
  } catch (err) {
    toast(`Save failed: ${String(err)}`, "error");
    return false;
  }
}

/**
 * Ctrl+O, the File menu, recents. Without `path`, shows the open dialog
 * (after the unsaved guard).
 */
export async function openProject(path?: string): Promise<void> {
  if (!(await ensureSavedOrDiscarded("open"))) return;
  let target = path;
  if (target === undefined) {
    const picked = await openDialog({
      title: "Open Project",
      multiple: false,
      filters: CUTTY_FILTER,
    });
    if (typeof picked !== "string") return;
    target = picked;
  }

  useMediaStore.getState().dropPendingImports();
  try {
    const result = await ipc.projectLoad(target);
    afterSessionSwitch(result.meta);
    if (result.recovery) {
      useSessionStore.getState().setRecovery(result.recovery);
    }
  } catch (err) {
    const [exists] = await pathsExist([target]).catch(() => [true]);
    if (!exists) {
      toast(`${basename(target)} has been moved or deleted.`, "error");
      const pruned = await ipc.projectRemoveRecent(target).catch(() => null);
      if (pruned !== null) useSessionStore.getState().setRecents(pruned);
      return;
    }
    toast(`Could not open ${basename(target)}: ${String(err)}`, "error");
  }
  void refreshRecents();
}

/** Ctrl+N. */
export async function newProject(): Promise<void> {
  if (!(await ensureSavedOrDiscarded("new"))) return;
  useMediaStore.getState().dropPendingImports();
  const meta = await ipc.projectNew();
  afterSessionSwitch(meta);
}

/** Restore a crash-recovery autosave (from the recovery dialog). */
export async function restoreRecovery(key: string): Promise<void> {
  useMediaStore.getState().dropPendingImports();
  try {
    const meta = await ipc.projectRestoreAutosave(key);
    afterSessionSwitch(meta);
    toast("Recovered unsaved work — save the project to keep it.");
  } catch (err) {
    toast(`Could not restore the autosave: ${String(err)}`, "error");
  } finally {
    useSessionStore.getState().setRecovery(null);
  }
}

/** Decline a crash-recovery offer: delete the autosave. */
export async function discardRecovery(key: string): Promise<void> {
  await ipc.projectDiscardAutosave(key).catch(() => undefined);
  useSessionStore.getState().setRecovery(null);
}

/** The backend prevented a close because of unsaved changes. */
async function handleCloseRequested(): Promise<void> {
  if (await ensureSavedOrDiscarded("close")) {
    // destroy() skips the close-requested cycle we came from.
    await getCurrentWindow().destroy();
  }
}

// -----------------------------------------------------------------------
// Global shortcuts + event wiring
// -----------------------------------------------------------------------

function onKeyDown(e: KeyboardEvent): void {
  if (!(e.ctrlKey || e.metaKey) || e.repeat) return;
  // While a modal flow is up, only its own buttons act.
  const { guard, recovery } = useSessionStore.getState();
  if (guard !== null || recovery !== null) return;

  switch (e.key.toLowerCase()) {
    case "s":
      e.preventDefault();
      void (e.shiftKey ? saveProjectAs() : saveProject());
      break;
    case "o":
      if (e.shiftKey) return;
      e.preventDefault();
      void openProject();
      break;
    case "n":
      if (e.shiftKey) return;
      e.preventDefault();
      void newProject();
      break;
    default:
      break;
  }
}

let started = false;

/**
 * Wire session events, shortcuts, the initial meta/recents fetch, and the
 * launch-time crash-recovery offer. Idempotent; lives for the app's
 * lifetime.
 */
export function startSessionSync(): void {
  if (started) return;
  started = true;

  void ipc.onProjectMeta((meta) => {
    useSessionStore.getState().setMeta(meta);
  });
  void ipc.onAutosave((p) => {
    useSessionStore.getState().autosaved(p.atMs, p.error);
  });
  void ipc.onCloseRequested(() => {
    void handleCloseRequested();
  });
  window.addEventListener("keydown", onKeyDown);

  void ipc
    .projectMeta()
    .then((meta) => useSessionStore.getState().setMeta(meta))
    .catch(() => undefined);
  void refreshRecents();

  // Offer the newest crash-recovery candidate, if any. Other candidates
  // keep their slots and are offered when their project is opened.
  void ipc
    .projectRecoveryScan()
    .then((offers) => {
      if (offers.length > 0) useSessionStore.getState().setRecovery(offers[0]);
    })
    .catch(() => undefined);
}
