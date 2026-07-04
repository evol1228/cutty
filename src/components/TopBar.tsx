import { useState } from "react";
import { save } from "@tauri-apps/plugin-dialog";
import { exportTrim } from "../lib/ipc";
import { usePlayerStore } from "../state/playerStore";

const MENUS = ["File", "Edit", "View", "Help"];

function TopBar() {
  const { media, playerInfo, inPointSec, outPointSec } = usePlayerStore();
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState<string | null>(null);

  async function onExport() {
    if (!media) return;
    const srcName = media.path.split("/").pop() ?? "clip.mp4";
    const base = srcName.replace(/\.[^.]+$/, "");
    const dst = await save({
      defaultPath: `${base}-trim.mp4`,
      filters: [{ name: "MP4 video", extensions: ["mp4"] }],
    });
    if (!dst) return;

    const inSec = inPointSec ?? 0;
    const outSec = outPointSec ?? media.durationSec;
    setBusy(true);
    setStatus(null);
    try {
      const result = await exportTrim(media.path, dst, inSec, outSec);
      const snapped = inSec - result.actualStartSec;
      setStatus(
        `Exported ${dst.split("/").pop()} (${result.durationSec.toFixed(2)}s` +
          (snapped > 0.01
            ? `, in point snapped ${snapped.toFixed(2)}s back to a keyframe)`
            : ")"),
      );
    } catch (e) {
      setStatus(String(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <header className="flex h-11 shrink-0 items-center gap-1 border-b border-zinc-800 bg-zinc-900 px-3">
      <span className="mr-2 font-semibold tracking-wide text-zinc-100">
        Cutty
      </span>
      {MENUS.map((m) => (
        <button
          key={m}
          disabled
          className="rounded px-2 py-1 text-zinc-400 hover:bg-zinc-800 disabled:cursor-default"
        >
          {m}
        </button>
      ))}
      <div className="flex flex-1 items-center justify-center gap-2">
        <span className="text-zinc-400">Untitled Project</span>
        <span
          className="h-1.5 w-1.5 rounded-full bg-emerald-500"
          title="Autosaved"
        />
      </div>
      {status && (
        <span className="max-w-64 truncate text-xs text-zinc-500" title={status}>
          {status}
        </span>
      )}
      <button
        id="export-button"
        disabled={!media || !playerInfo || busy}
        onClick={() => void onExport()}
        title="Lossless trim export (uses In/Out marks, defaults to the whole clip)"
        className="rounded-md bg-sky-600 px-4 py-1.5 font-medium text-white hover:bg-sky-500 disabled:opacity-40"
      >
        {busy ? "Exporting…" : "Export"}
      </button>
    </header>
  );
}

export default TopBar;
