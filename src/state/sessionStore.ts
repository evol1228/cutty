// Session/persistence UI state: project meta (name, path, dirty),
// autosave status, recents, and the modal flows (unsaved-changes guard,
// crash recovery). Mirrors backend events — the Rust side owns the truth.

import { create } from "zustand";
import type {
  ProjectMeta,
  RecentEntry,
  RecoveryOffer,
} from "../lib/projectIpc";

export type GuardChoice = "save" | "discard" | "cancel";
export type GuardAction = "new" | "open" | "close";

export interface GuardRequest {
  /** What the user was doing, for the dialog copy. */
  action: GuardAction;
  resolve: (choice: GuardChoice) => void;
}

interface SessionState {
  meta: ProjectMeta;
  /** Epoch ms of the last explicit save in this session. */
  lastSavedMs: number | null;
  /** Epoch ms of the last background autosave. */
  lastAutosaveMs: number | null;
  autosaveError: string | null;
  recents: RecentEntry[];
  /** Pending crash-recovery offer (modal). */
  recovery: RecoveryOffer | null;
  /** Pending unsaved-changes guard (modal). */
  guard: GuardRequest | null;
  /** The user closed the start card manually. */
  startDismissed: boolean;

  setMeta: (meta: ProjectMeta) => void;
  /** An explicit save just succeeded. */
  savedNow: (meta: ProjectMeta) => void;
  /** A project was just loaded / created / restored. */
  sessionSwitched: (meta: ProjectMeta) => void;
  autosaved: (atMs: number | null, error: string | null) => void;
  setRecents: (recents: RecentEntry[]) => void;
  setRecovery: (offer: RecoveryOffer | null) => void;
  setGuard: (guard: GuardRequest | null) => void;
  dismissStart: () => void;
}

export const useSessionStore = create<SessionState>((set) => ({
  meta: { path: null, name: "Untitled Project", dirty: false },
  lastSavedMs: null,
  lastAutosaveMs: null,
  autosaveError: null,
  recents: [],
  recovery: null,
  guard: null,
  startDismissed: false,

  setMeta: (meta) => set({ meta }),
  savedNow: (meta) =>
    set({
      meta,
      lastSavedMs: Date.now(),
      lastAutosaveMs: null,
      autosaveError: null,
    }),
  sessionSwitched: (meta) =>
    set({
      meta,
      lastSavedMs: null,
      lastAutosaveMs: null,
      autosaveError: null,
    }),
  autosaved: (atMs, error) =>
    set(
      error !== null
        ? { autosaveError: error }
        : { lastAutosaveMs: atMs, autosaveError: null },
    ),
  setRecents: (recents) => set({ recents }),
  setRecovery: (recovery) => set({ recovery }),
  setGuard: (guard) => set({ guard }),
  dismissStart: () => set({ startDismissed: true }),
}));
