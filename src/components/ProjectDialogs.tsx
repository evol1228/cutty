// Modal flows for project persistence: the unsaved-changes guard
// (Save / Don't Save / Cancel) and the crash-recovery offer
// (Restore / Discard). Both render from sessionStore state so any code
// path (shortcut, menu, window close) shares one implementation.

import { useEffect } from "react";
import type { ReactNode } from "react";
import { discardRecovery, restoreRecovery } from "../lib/projectActions";
import { useSessionStore, type GuardChoice } from "../state/sessionStore";

function Modal({ children }: { children: ReactNode }) {
  return (
    <div className="fixed inset-0 z-[90] flex items-center justify-center bg-black/60">
      <div className="w-[26rem] rounded-lg border border-zinc-700 bg-zinc-900 p-5 shadow-2xl shadow-black/60">
        {children}
      </div>
    </div>
  );
}

const GUARD_VERBS = {
  new: "creating a new project",
  open: "opening another project",
  close: "closing",
} as const;

function UnsavedChangesDialog() {
  const guard = useSessionStore((s) => s.guard);
  const name = useSessionStore((s) => s.meta.name);

  // Enter saves, Escape cancels — the standard dance.
  useEffect(() => {
    if (!guard) return;
    const pick = (choice: GuardChoice) => guard.resolve(choice);
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Enter") {
        e.preventDefault();
        e.stopPropagation();
        pick("save");
      } else if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        pick("cancel");
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [guard]);

  if (!guard) return null;
  return (
    <Modal>
      <h2 className="mb-2 font-semibold text-zinc-100">Unsaved changes</h2>
      <p className="mb-5 text-sm text-zinc-400">
        “{name}” has unsaved changes. Save them before{" "}
        {GUARD_VERBS[guard.action]}?
      </p>
      <div className="flex justify-end gap-2">
        <button
          onClick={() => guard.resolve("cancel")}
          className="rounded-md px-3 py-1.5 text-sm text-zinc-300 hover:bg-zinc-800"
        >
          Cancel
        </button>
        <button
          onClick={() => guard.resolve("discard")}
          className="rounded-md px-3 py-1.5 text-sm text-red-400 hover:bg-zinc-800"
        >
          Don’t Save
        </button>
        <button
          autoFocus
          onClick={() => guard.resolve("save")}
          className="rounded-md bg-sky-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-sky-500"
        >
          Save
        </button>
      </div>
    </Modal>
  );
}

function RecoveryDialog() {
  const recovery = useSessionStore((s) => s.recovery);
  if (!recovery) return null;

  const when = new Date(recovery.modifiedMs).toLocaleString([], {
    dateStyle: "medium",
    timeStyle: "short",
  });
  return (
    <Modal>
      <h2 className="mb-2 font-semibold text-zinc-100">
        Restore unsaved work?
      </h2>
      <p className="mb-5 text-sm text-zinc-400">
        Cutty didn’t shut down cleanly. An autosave of “{recovery.name}” from{" "}
        {when}
        {recovery.projectPath
          ? " is newer than the saved project file."
          : " was never saved to a project file."}
      </p>
      <div className="flex justify-end gap-2">
        <button
          onClick={() => void discardRecovery(recovery.key)}
          className="rounded-md px-3 py-1.5 text-sm text-red-400 hover:bg-zinc-800"
        >
          Discard
        </button>
        <button
          autoFocus
          onClick={() => void restoreRecovery(recovery.key)}
          className="rounded-md bg-sky-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-sky-500"
        >
          Restore
        </button>
      </div>
    </Modal>
  );
}

function ProjectDialogs() {
  return (
    <>
      <UnsavedChangesDialog />
      <RecoveryDialog />
    </>
  );
}

export default ProjectDialogs;
