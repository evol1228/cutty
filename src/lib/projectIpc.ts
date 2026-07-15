// Typed wrappers around the project persistence IPC surface: save/load,
// session meta (name/path/dirty), autosave status, crash recovery, and
// the recent-projects list. The Rust side owns all persistence logic.

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export interface ProjectMeta {
  /** Absolute path of the .cutty file; null until first saved. */
  path: string | null;
  /** File stem, or "Untitled Project". */
  name: string;
  /** Current state differs from the last save. */
  dirty: boolean;
}

export interface AutosavePayload {
  /** Epoch ms of a successful autosave write; null on failure. */
  atMs: number | null;
  error: string | null;
}

export interface RecoveryOffer {
  key: string;
  projectPath: string | null;
  name: string;
  /** Autosave mtime, epoch ms. */
  modifiedMs: number;
}

export interface RecentEntry {
  path: string;
  name: string;
  exists: boolean;
  openedAtMs: number;
}

export interface LoadResult {
  meta: ProjectMeta;
  /** A newer autosave exists for the opened project. */
  recovery: RecoveryOffer | null;
}

export const PROJECT_META_EVENT = "project://meta";
export const AUTOSAVE_EVENT = "project://autosave";
export const CLOSE_REQUESTED_EVENT = "project://close-requested";

/** Session meta updates (fire after every mutation and save/load/new). */
export function onProjectMeta(
  handler: (meta: ProjectMeta) => void,
): Promise<UnlistenFn> {
  return listen<ProjectMeta>(PROJECT_META_EVENT, (e) => handler(e.payload));
}

/** Autosave outcomes from the background worker. */
export function onAutosave(
  handler: (payload: AutosavePayload) => void,
): Promise<UnlistenFn> {
  return listen<AutosavePayload>(AUTOSAVE_EVENT, (e) => handler(e.payload));
}

/**
 * The window close button was pressed with unsaved changes; the backend
 * prevented the close and expects the frontend to run the guard dialog.
 */
export function onCloseRequested(handler: () => void): Promise<UnlistenFn> {
  return listen(CLOSE_REQUESTED_EVENT, () => handler());
}

export function projectMeta(): Promise<ProjectMeta> {
  return invoke<ProjectMeta>("project_meta");
}

/** Save to `path` (Save As) or to the session's current path. */
export function projectSave(path?: string): Promise<ProjectMeta> {
  return invoke<ProjectMeta>("project_save", { path: path ?? null });
}

export function projectLoad(path: string): Promise<LoadResult> {
  return invoke<LoadResult>("project_load", { path });
}

export function projectNew(): Promise<ProjectMeta> {
  return invoke<ProjectMeta>("project_new");
}

export function projectRecents(): Promise<RecentEntry[]> {
  return invoke<RecentEntry[]>("project_recents");
}

export function projectRemoveRecent(path: string): Promise<RecentEntry[]> {
  return invoke<RecentEntry[]>("project_remove_recent", { path });
}

/** Launch-time crash-recovery scan (newest candidate first). */
export function projectRecoveryScan(): Promise<RecoveryOffer[]> {
  return invoke<RecoveryOffer[]>("project_recovery_scan");
}

export function projectRestoreAutosave(key: string): Promise<ProjectMeta> {
  return invoke<ProjectMeta>("project_restore_autosave", { key });
}

export function projectDiscardAutosave(key: string): Promise<void> {
  return invoke("project_discard_autosave", { key });
}

/** "Don't Save": drop the current session's autosave before discarding. */
export function projectDiscardCurrentAutosave(): Promise<void> {
  return invoke("project_discard_current_autosave");
}
