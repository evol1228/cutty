// Left panel: the media pool. Import via native dialog or OS drag-and-drop,
// thumbnail grid with live background-job status, drag items onto the
// timeline (pointer-based — see timeline/poolDrag.ts), delete with an
// in-use warning. The engine owns which media belong to the project; this
// component renders mediaStore's job-state view of it.

import { useEffect, useState } from "react";
import { ask, open } from "@tauri-apps/plugin-dialog";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import {
  cachedTransitionList,
  engineRemoveMedia,
  type TransitionDef,
} from "../lib/engineIpc";
import {
  AUDIO_EXTENSIONS,
  defaultClipDuration,
  IMAGE_EXTENSIONS,
  useMediaStore,
  VIDEO_EXTENSIONS,
  type PoolItem,
} from "../state/mediaStore";
import { useProjectStore } from "../state/projectStore";
import { toast } from "../state/toastStore";
import { addTextAtPlayhead } from "../timeline/actions";
import { TEXT_PRESETS } from "../lib/textPresets";
import { beginPoolDrag, beginTransitionDrag } from "../timeline/poolDrag";

const TABS = ["Import", "Text", "Transitions", "Library"] as const;
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
          // Stills have no intrinsic duration: the drag ghost and drop
          // use the default still clip length.
          durationSec: defaultClipDuration(item),
          hasVideo: item.hasVideo,
          hasAudio: item.hasAudio,
          thumbnailUrl: item.thumbnailUrl,
        });
      }}
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
            {item.kind === "image"
              ? "Still"
              : item.kind === "gif"
                ? `GIF ${formatDuration(item.durationSec)}`
                : formatDuration(item.durationSec)}
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

/** The Text tab: "Add text" plus the built-in style presets. Clicking a
 * tile drops a clip at the playhead (the engine picks/creates the lane);
 * `T` on the timeline does the same with the default style. */
function TextPanel() {
  const hasProject = useProjectStore((s) => s.project !== null);
  return (
    <div className="flex min-h-0 flex-1 flex-col gap-3 p-3">
      <button
        onClick={() => void addTextAtPlayhead()}
        disabled={!hasProject}
        className="shrink-0 rounded-md border border-zinc-700 bg-zinc-800 px-3 py-2 text-zinc-200 hover:bg-zinc-700 disabled:cursor-default disabled:text-zinc-600 disabled:hover:bg-zinc-800"
        title="Add a default text clip at the playhead (T)"
      >
        + Add text
      </button>
      <p className="shrink-0 text-[11px] leading-snug text-zinc-500">
        Click a style to add it at the playhead. Press{" "}
        <kbd className="rounded border border-zinc-700 bg-zinc-800 px-1">T</kbd>{" "}
        on the timeline for the default style.
      </p>
      <div className="grid min-h-0 flex-1 auto-rows-min grid-cols-2 gap-2 overflow-y-auto pb-2">
        {TEXT_PRESETS.map((preset) => (
          <button
            key={preset.id}
            onClick={() => void addTextAtPlayhead(preset)}
            disabled={!hasProject}
            className="group text-left disabled:cursor-default"
            title={`${preset.label} — add at the playhead`}
          >
            <div className="flex aspect-video items-center justify-center overflow-hidden rounded-md border border-zinc-800 bg-gradient-to-br from-zinc-900 via-zinc-950 to-black px-1.5 group-hover:border-orange-600">
              <span
                className="max-w-full select-none whitespace-pre text-center text-sm leading-tight"
                style={preset.css}
              >
                {preset.sampleContent}
              </span>
            </div>
            <div className="mt-1 truncate text-center text-[11px] text-zinc-300">
              {preset.label}
            </div>
          </button>
        ))}
      </div>
    </div>
  );
}

/** A schematic per-kind glyph: A→B panels with the transition's motion. */
function TransitionGlyph({ id }: { id: string }) {
  const arrow = (d: string) => (
    <path d={d} stroke="currentColor" strokeWidth="2" fill="none" strokeLinecap="round" strokeLinejoin="round" />
  );
  let motif: React.ReactNode;
  if (id.startsWith("wipe") || id.startsWith("slide")) {
    motif =
      id.endsWith("left") ? arrow("M26 20 H14 M18 15 l-5 5 5 5")
      : id.endsWith("right") ? arrow("M14 20 H26 M22 15 l5 5 -5 5")
      : id.endsWith("up") ? arrow("M20 26 V14 M15 18 l5 -5 5 5")
      : arrow("M20 14 V26 M15 22 l5 5 5 -5");
  } else if (id.startsWith("circle") || id === "radial") {
    motif = <circle cx="20" cy="20" r="7" stroke="currentColor" strokeWidth="2" fill="none" />;
  } else if (id === "crosszoom") {
    motif = arrow("M20 12 v16 M12 20 h16 M15 15 l10 10 M25 15 l-10 10");
  } else if (id === "pixelize" || id === "glitchmemories") {
    motif = (
      <g fill="currentColor">
        <rect x="14" y="14" width="5" height="5" />
        <rect x="22" y="17" width="5" height="5" />
        <rect x="16" y="22" width="5" height="5" />
      </g>
    );
  } else if (id === "cube" || id === "doorway") {
    motif = arrow("M14 14 l6 3 6 -3 M20 17 v9 M14 14 v9 l6 3 6 -3 v-9");
  } else if (id === "windowslice") {
    motif = (
      <g stroke="currentColor" strokeWidth="2">
        <line x1="15" y1="13" x2="15" y2="27" />
        <line x1="20" y1="13" x2="20" y2="27" />
        <line x1="25" y1="13" x2="25" y2="27" />
      </g>
    );
  } else {
    // fade / linearblur: two overlapping panels.
    motif = (
      <g>
        <rect x="12" y="13" width="11" height="11" rx="1.5" fill="currentColor" opacity="0.45" />
        <rect x="17" y="16" width="11" height="11" rx="1.5" fill="currentColor" opacity="0.85" />
      </g>
    );
  }
  return (
    <svg viewBox="0 0 40 40" className="h-10 w-10 text-violet-300">
      {motif}
    </svg>
  );
}

/** The Transitions tab: a grid of draggable transition tiles. Drag one
 * onto a cut point in the timeline (cuts light up while dragging). */
function TransitionsPanel() {
  const [catalog, setCatalog] = useState<TransitionDef[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    cachedTransitionList()
      .then((defs) => {
        if (alive) setCatalog(defs);
      })
      .catch((err) => {
        if (alive) setError(String(err));
      });
    return () => {
      alive = false;
    };
  }, []);

  if (error) {
    return (
      <div className="flex flex-1 items-center justify-center px-4 text-center text-xs text-red-400">
        Could not load transitions: {error}
      </div>
    );
  }
  if (!catalog) {
    return (
      <div className="flex flex-1 items-center justify-center text-xs text-zinc-600">
        Loading…
      </div>
    );
  }
  return (
    <div className="flex min-h-0 flex-1 flex-col gap-2 p-3">
      <p className="shrink-0 text-[11px] leading-snug text-zinc-500">
        Drag a transition onto a cut between two touching clips.
      </p>
      <div className="grid min-h-0 flex-1 auto-rows-min grid-cols-2 gap-2 overflow-y-auto pb-2">
        {catalog.map((def) => (
          <div
            key={def.id}
            title={`${def.label} — drag onto a cut (${def.defaultDuration.toFixed(1)}s)`}
            className="group cursor-grab select-none"
            onPointerDown={(e) =>
              beginTransitionDrag(e.nativeEvent, {
                id: def.id,
                label: def.label,
                defaultDuration: def.defaultDuration,
              })
            }
          >
            <div className="flex aspect-video items-center justify-center rounded-md border border-zinc-800 bg-zinc-950 group-hover:border-violet-600">
              <TransitionGlyph id={def.id} />
            </div>
            <div className="mt-1 truncate text-center text-[11px] text-zinc-300">
              {def.label}
            </div>
          </div>
        ))}
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
      {tab === "Text" ? (
        <TextPanel />
      ) : tab === "Transitions" ? (
        <TransitionsPanel />
      ) : tab === "Import" ? (
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
