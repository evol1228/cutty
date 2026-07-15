// Top bar: File menu (new/open/recents/save), project name with a
// dirty-state dot, the autosave indicator, and the Export button
// (enabled later in Phase 1).

import { useEffect, useState } from "react";
import {
  newProject,
  openProject,
  saveProject,
  saveProjectAs,
} from "../lib/projectActions";
import { useExportStore } from "../state/exportStore";
import { useSessionStore } from "../state/sessionStore";

/** "just now" / "42s ago" / "3m ago" / "2h ago". */
function relTime(ms: number, now: number): string {
  const s = Math.max(0, Math.floor((now - ms) / 1000));
  if (s < 10) return "just now";
  if (s < 60) return `${s}s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  return `${Math.floor(m / 60)}h ago`;
}

/** "Saved just now" style status next to the project name. */
function SaveIndicator() {
  const meta = useSessionStore((s) => s.meta);
  const lastSavedMs = useSessionStore((s) => s.lastSavedMs);
  const lastAutosaveMs = useSessionStore((s) => s.lastAutosaveMs);
  const autosaveError = useSessionStore((s) => s.autosaveError);
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 10_000);
    return () => clearInterval(timer);
  }, []);

  if (meta.dirty && autosaveError !== null) {
    return (
      <span className="text-xs text-red-400" title={autosaveError}>
        ⚠ Autosave failed
      </span>
    );
  }
  if (meta.dirty) {
    return (
      <span className="text-xs text-zinc-500">
        {lastAutosaveMs !== null
          ? `Autosaved ${relTime(lastAutosaveMs, now)}`
          : "Unsaved changes"}
      </span>
    );
  }
  if (meta.path !== null) {
    return (
      <span className="text-xs text-zinc-500">
        {lastSavedMs !== null ? `Saved ${relTime(lastSavedMs, now)}` : "Saved"}
      </span>
    );
  }
  return null;
}

interface MenuEntry {
  label: string;
  shortcut?: string;
  disabled?: boolean;
  run: () => void;
}

function FileMenu() {
  const [open, setOpen] = useState(false);
  const recents = useSessionStore((s) => s.recents).filter((r) => r.exists);

  const entries: MenuEntry[] = [
    { label: "New Project", shortcut: "Ctrl+N", run: () => void newProject() },
    { label: "Open…", shortcut: "Ctrl+O", run: () => void openProject() },
    { label: "Save", shortcut: "Ctrl+S", run: () => void saveProject() },
    {
      label: "Save As…",
      shortcut: "Ctrl+Shift+S",
      run: () => void saveProjectAs(),
    },
  ];

  return (
    <div className="relative">
      <button
        onClick={() => setOpen((o) => !o)}
        className={`rounded px-2 py-1 text-zinc-300 hover:bg-zinc-800 ${
          open ? "bg-zinc-800" : ""
        }`}
      >
        File
      </button>
      {open && (
        <>
          {/* click-away backdrop */}
          <div className="fixed inset-0 z-40" onClick={() => setOpen(false)} />
          <div className="absolute left-0 top-full z-50 mt-1 w-64 rounded-md border border-zinc-700 bg-zinc-900 py-1 shadow-xl shadow-black/50">
            {entries.slice(0, 2).map((e) => (
              <MenuItem key={e.label} entry={e} close={() => setOpen(false)} />
            ))}
            {recents.length > 0 && (
              <>
                <div className="mx-3 my-1 border-t border-zinc-800" />
                <div className="px-3 py-1 text-[10px] uppercase tracking-wider text-zinc-600">
                  Recent projects
                </div>
                {recents.slice(0, 6).map((r) => (
                  <button
                    key={r.path}
                    title={r.path}
                    onClick={() => {
                      setOpen(false);
                      void openProject(r.path);
                    }}
                    className="block w-full truncate px-3 py-1.5 text-left text-sm text-zinc-300 hover:bg-zinc-800"
                  >
                    {r.name}
                    <span className="ml-2 text-xs text-zinc-600">
                      {r.path.replace(/\/[^/]*$/, "")}
                    </span>
                  </button>
                ))}
              </>
            )}
            <div className="mx-3 my-1 border-t border-zinc-800" />
            {entries.slice(2).map((e) => (
              <MenuItem key={e.label} entry={e} close={() => setOpen(false)} />
            ))}
          </div>
        </>
      )}
    </div>
  );
}

function MenuItem({ entry, close }: { entry: MenuEntry; close: () => void }) {
  return (
    <button
      disabled={entry.disabled}
      onClick={() => {
        close();
        entry.run();
      }}
      className="flex w-full items-center justify-between px-3 py-1.5 text-sm text-zinc-300 hover:bg-zinc-800 disabled:opacity-40"
    >
      <span>{entry.label}</span>
      {entry.shortcut && (
        <span className="text-xs text-zinc-600">{entry.shortcut}</span>
      )}
    </button>
  );
}

function TopBar() {
  const meta = useSessionStore((s) => s.meta);

  return (
    <header className="flex h-11 shrink-0 items-center gap-1 border-b border-zinc-800 bg-zinc-900 px-3">
      <span className="mr-2 font-semibold tracking-wide text-zinc-100">
        Cutty
      </span>
      <FileMenu />
      {["Edit", "View", "Help"].map((m) => (
        <button
          key={m}
          disabled
          className="rounded px-2 py-1 text-zinc-400 hover:bg-zinc-800 disabled:cursor-default"
        >
          {m}
        </button>
      ))}
      <div className="flex flex-1 items-center justify-center gap-2">
        <span className="text-zinc-300" title={meta.path ?? "Not saved yet"}>
          {meta.name}
        </span>
        {meta.dirty && (
          <span
            className="h-1.5 w-1.5 rounded-full bg-amber-400"
            title="Unsaved changes"
          />
        )}
        <SaveIndicator />
      </div>
      <ExportButton />
    </header>
  );
}

/** Export entry point; shows live progress while a job runs (the export
 * continues in the background — clicking reopens the dialog). */
function ExportButton() {
  const phase = useExportStore((s) => s.phase);
  const percent = useExportStore((s) => s.percent);
  const openDialog = useExportStore((s) => s.openDialog);

  const running = phase === "running";
  return (
    <button
      id="export-button"
      onClick={openDialog}
      title={running ? "Show export progress" : "Export the timeline"}
      className="relative min-w-24 overflow-hidden rounded-md bg-sky-600 px-4 py-1.5 font-medium text-white hover:bg-sky-500"
    >
      {running && (
        <span
          className="absolute inset-y-0 left-0 bg-sky-400/40 transition-[width] duration-300"
          style={{ width: `${percent}%` }}
        />
      )}
      <span className="relative">
        {running ? `${percent.toFixed(0)}%` : "Export"}
      </span>
    </button>
  );
}

export default TopBar;
