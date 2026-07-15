// The start state: a card over the player with recent projects, shown
// while the session is pristine (untitled, clean, nothing imported).
// Any real activity — or the ✕ — dismisses it.

import { openProject } from "../lib/projectActions";
import { useMediaStore } from "../state/mediaStore";
import { useProjectStore } from "../state/projectStore";
import { useSessionStore } from "../state/sessionStore";

function StartOverlay() {
  const meta = useSessionStore((s) => s.meta);
  const dismissed = useSessionStore((s) => s.startDismissed);
  const dismissStart = useSessionStore((s) => s.dismissStart);
  const recents = useSessionStore((s) => s.recents).filter((r) => r.exists);
  const project = useProjectStore((s) => s.project);
  const poolCount = useMediaStore((s) => s.items.length);

  const pristine =
    project !== null &&
    project.media.length === 0 &&
    project.tracks.every((t) => t.clips.length === 0);
  const show =
    !dismissed && meta.path === null && !meta.dirty && pristine && poolCount === 0;
  if (!show) return null;

  return (
    <div className="pointer-events-none absolute inset-0 z-30 flex items-center justify-center">
      <div className="pointer-events-auto w-[24rem] rounded-xl border border-zinc-700/80 bg-zinc-900/95 p-6 shadow-2xl shadow-black/60">
        <div className="mb-1 flex items-start justify-between">
          <h1 className="text-lg font-semibold text-zinc-100">
            Welcome to Cutty
          </h1>
          <button
            onClick={dismissStart}
            title="Start with an empty project"
            className="rounded px-1.5 text-zinc-500 hover:bg-zinc-800 hover:text-zinc-300"
          >
            ✕
          </button>
        </div>
        <p className="mb-4 text-sm text-zinc-500">
          Import media to start editing, or pick up where you left off.
        </p>
        <button
          onClick={() => void openProject()}
          className="mb-4 w-full rounded-md bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500"
        >
          Open Project… <span className="text-sky-200/70">Ctrl+O</span>
        </button>
        <div className="mb-1 text-[10px] uppercase tracking-wider text-zinc-600">
          Recent projects
        </div>
        {recents.length === 0 ? (
          <p className="py-2 text-sm text-zinc-600">Nothing here yet.</p>
        ) : (
          <ul className="max-h-48 overflow-y-auto">
            {recents.slice(0, 8).map((r) => (
              <li key={r.path}>
                <button
                  title={r.path}
                  onClick={() => void openProject(r.path)}
                  className="block w-full truncate rounded px-2 py-1.5 text-left text-sm text-zinc-300 hover:bg-zinc-800"
                >
                  {r.name}
                  <span className="ml-2 text-xs text-zinc-600">
                    {r.path.replace(/\/[^/]*$/, "")}
                  </span>
                </button>
              </li>
            ))}
          </ul>
        )}
      </div>
    </div>
  );
}

export default StartOverlay;
