// Left panel: the media pool. Import via native dialog or OS drag-and-drop,
// thumbnail grid with live background-job status, drag items onto the
// timeline (pointer-based — see timeline/poolDrag.ts), delete with an
// in-use warning. The engine owns which media belong to the project; this
// component renders mediaStore's job-state view of it.

import { useEffect, useState } from "react";
import { ask, open } from "@tauri-apps/plugin-dialog";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { closePlayer, openPlayer, probeMedia } from "../lib/ipc";
import { engineRemoveMedia } from "../lib/engineIpc";
import { dispatchFrame } from "../lib/frameSink";
import {
  AUDIO_EXTENSIONS,
  IMAGE_EXTENSIONS,
  useMediaStore,
  VIDEO_EXTENSIONS,
  type PoolItem,
} from "../state/mediaStore";
import { usePlayerStore } from "../state/playerStore";
import { useProjectStore } from "../state/projectStore";
import { toast } from "../state/toastStore";
import { beginPoolDrag } from "../timeline/poolDrag";

const TABS = ["Import", "Library"] as const;
type Tab = (typeof TABS)[number];

function formatDuration(sec: number): string {
  const s = Math.max(0, Math.round(sec));
  const m = Math.floor(s / 60);
  const h = Math.floor(m / 60);
  const pad = (n: number) => String(n).padStart(2, "0");
  return h > 0 ? `${h}:${pad(m % 60)}:${pad(s % 60)}` : `${m}:${pad(s % 60)}`;
}

async function pickAndImport(): Promise<void> {
  const picked = await open({
    multiple: true,
    filters: [
      {
        name: "Media",
        extensions: [...VIDEO_EXTENSIONS, ...AUDIO_EXTENSIONS, ...IMAGE_EXTENSIONS],
      },
      { name: "Video", extensions: VIDEO_EXTENSIONS },
      { name: "Audio", extensions: AUDIO_EXTENSIONS },
      { name: "Images", extensions: IMAGE_EXTENSIONS },
    ],
  });
  if (!picked) return;
  const paths = Array.isArray(picked) ? picked : [picked];
  await useMediaStore.getState().importFiles(paths);
}

/** How many timeline clips reference this media right now. */
function clipsUsing(mediaId: number | null): number {
  if (mediaId === null) return 0;
  const project = useProjectStore.getState().project;
  if (!project) return 0;
  let n = 0;
  for (const track of project.tracks) {
    for (const clip of track.clips) if (clip.mediaId === mediaId) n++;
  }
  return n;
}

async function deleteItem(item: PoolItem): Promise<void> {
  // Never registered with the engine (probe in flight or failed): the
  // item only exists locally.
  if (item.mediaId === null) {
    useMediaStore.getState().forgetItem(item.path);
    return;
  }
  const used = clipsUsing(item.mediaId);
  if (used > 0) {
    const confirmed = await ask(
      `"${item.name}" is used by ${used} clip${used === 1 ? "" : "s"} on the ` +
        `timeline. Removing it also removes ${used === 1 ? "that clip" : "those clips"} ` +
        `(a single undo step).`,
      { title: "Remove media?", kind: "warning" },
    );
    if (!confirmed) return;
  }
  try {
    // The engine removes media + clips as one undoable command; the next
    // snapshot reconciles the pool item away.
    await engineRemoveMedia(item.mediaId);
  } catch (err) {
    toast(`Could not remove ${item.name}: ${String(err)}`, "error");
  }
}

/** Double-click: preview the proxy in the Phase 0 player. */
async function openInPlayer(item: PoolItem): Promise<void> {
  if (!item.hasVideo || !item.proxyPath || item.missing) return;
  try {
    await closePlayer().catch(() => undefined);
    const info = item.info ?? (await probeMedia(item.path));
    const player = usePlayerStore.getState();
    player.setMedia(info);
    player.setProxyPath(item.proxyPath);
    player.setPlayerInfo(await openPlayer(item.proxyPath, dispatchFrame));
  } catch (err) {
    toast(`Could not open ${item.name}: ${String(err)}`, "error");
  }
}

function statusLabel(item: PoolItem): string | null {
  switch (item.status) {
    case "probing":
      return "Probing…";
    case "processing":
      return item.proxyProgress !== null
        ? `Proxy ${item.proxyProgress.toFixed(0)}%`
        : "Preparing…";
    case "error":
      return "Import failed";
    case "ready":
      return null;
  }
}

function PoolItemCard({ item }: { item: PoolItem }) {
  const draggable = item.mediaId !== null && !item.missing && item.status !== "error";
  const borderClass =
    item.missing || item.status === "error"
      ? "border-red-700"
      : "border-zinc-800";
  const title = item.missing
    ? `File not found: ${item.path}`
    : item.status === "error"
      ? (item.error ?? "Import failed")
      : item.path;
  const label = statusLabel(item);

  return (
    <div
      className={`group relative select-none ${draggable ? "cursor-grab" : ""}`}
      title={title}
      onPointerDown={(e) => {
        if (!draggable || item.durationSec === null || item.mediaId === null) {
          return;
        }
        beginPoolDrag(e.nativeEvent, {
          mediaId: item.mediaId,
          name: item.name,
          durationSec: item.durationSec,
          hasVideo: item.hasVideo,
          hasAudio: item.hasAudio,
          thumbnailUrl: item.thumbnailUrl,
        });
      }}
      onDoubleClick={() => void openInPlayer(item)}
    >
      <div
        className={`relative aspect-video overflow-hidden rounded-md border bg-zinc-950 ${borderClass}`}
      >
        {item.thumbnailUrl ? (
          <img
            src={item.thumbnailUrl}
            alt=""
            draggable={false}
            className="h-full w-full object-cover"
          />
        ) : (
          <div className="flex h-full w-full items-center justify-center text-xl text-zinc-600">
            {item.hasAudio && !item.hasVideo ? "♪" : "🎞"}
          </div>
        )}
        {item.missing && (
          <div className="absolute inset-0 flex items-center justify-center bg-red-950/60 text-xs font-medium text-red-300">
            missing
          </div>
        )}
        {item.durationSec !== null && !item.missing && (
          <span className="absolute bottom-1 right-1 rounded bg-black/70 px-1 text-[10px] tabular-nums text-zinc-200">
            {formatDuration(item.durationSec)}
          </span>
        )}
        {label && (
          <div className="absolute inset-x-0 bottom-0 bg-black/70 px-1.5 py-0.5">
            <span
              className={`text-[10px] ${item.status === "error" ? "text-red-400" : "text-sky-300"}`}
            >
              {label}
            </span>
            {item.status === "processing" && (
              <div className="mt-0.5 h-0.5 overflow-hidden rounded bg-zinc-800">
                <div
                  className="h-full bg-sky-500"
                  style={{ width: `${item.proxyProgress ?? 8}%` }}
                />
              </div>
            )}
          </div>
        )}
      </div>
      <div className="mt-1 flex items-center gap-1">
        <span className="min-w-0 flex-1 truncate text-[11px] text-zinc-300">
          {item.name}
        </span>
        <button
          className="shrink-0 rounded px-1 text-xs text-zinc-500 opacity-0 hover:bg-zinc-700 hover:text-zinc-200 group-hover:opacity-100"
          title="Remove from pool"
          onClick={() => void deleteItem(item)}
        >
          ✕
        </button>
      </div>
    </div>
  );
}

function MediaPool() {
  const [tab, setTab] = useState<Tab>("Import");
  const [dropHover, setDropHover] = useState(false);
  const items = useMediaStore((s) => s.items);

  // OS drag-and-drop (Tauri webview event — native file drags, Wayland
  // included). Dropping anywhere in the window imports into the pool.
  useEffect(() => {
    const unlisten = getCurrentWebview().onDragDropEvent((event) => {
      if (event.payload.type === "drop") {
        setDropHover(false);
        if (event.payload.paths.length > 0) {
          void useMediaStore.getState().importFiles(event.payload.paths);
        }
      } else {
        setDropHover(event.payload.type !== "leave");
      }
    });
    return () => {
      void unlisten.then((fn) => fn());
    };
  }, []);

  return (
    <aside className="flex w-72 shrink-0 flex-col border-r border-zinc-800 bg-zinc-900">
      <nav className="flex gap-1 border-b border-zinc-800 px-2 pt-2">
        {TABS.map((t) => (
          <button
            key={t}
            onClick={() => setTab(t)}
            className={`rounded-t px-3 py-1.5 ${
              tab === t
                ? "bg-zinc-800 text-zinc-100"
                : "text-zinc-500 hover:text-zinc-300"
            }`}
          >
            {t}
          </button>
        ))}
      </nav>
      {tab === "Import" ? (
        <div
          className={`flex min-h-0 flex-1 flex-col gap-3 p-3 ${
            dropHover ? "bg-sky-950/30 ring-1 ring-inset ring-sky-600" : ""
          }`}
        >
          <button
            onClick={() => void pickAndImport()}
            className="shrink-0 rounded-md border border-zinc-700 bg-zinc-800 px-3 py-2 text-zinc-200 hover:bg-zinc-700"
          >
            Import
          </button>
          {items.length === 0 ? (
            <div className="flex flex-1 items-center justify-center px-4 text-center text-xs text-zinc-600">
              Import media or drop files here, then drag them onto the
              timeline.
            </div>
          ) : (
            <div className="grid min-h-0 flex-1 auto-rows-min grid-cols-2 gap-2 overflow-y-auto pb-2">
              {items.map((item) => (
                <PoolItemCard key={item.path} item={item} />
              ))}
            </div>
          )}
        </div>
      ) : (
        <div className="flex flex-1 items-center justify-center text-zinc-600">
          Library is empty
        </div>
      )}
    </aside>
  );
}

export default MediaPool;
