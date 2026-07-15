// Bottom timeline panel: toolbar, track headers (flags + context menu),
// and the canvas-rendered timeline (renderer + interactions live in
// src/timeline/).

import { useEffect, useRef, useState } from "react";
import type { Track, TrackFlag, TransitionDef } from "../lib/engineIpc";
import {
  cachedTransitionList,
  engineAddTrack,
  engineMoveTrack,
  engineRemoveTrack,
  engineSetTrackFlag,
  engineSetTransition,
} from "../lib/engineIpc";
import { startEngineSync } from "../state/engineSync";
import { useProjectStore, type TransitionPicker } from "../state/projectStore";
import { toast } from "../state/toastStore";
import {
  deleteSelection,
  redo,
  seedCutTimeline,
  seedDummyClips,
  splitAtPlayhead,
  undo,
} from "../timeline/actions";
import { createTimelineController } from "../timeline/controller";
import {
  laneHeight,
  lanesHeight,
  MAX_PX_PER_SEC,
  MIN_PX_PER_SEC,
  RULER_H,
  scrollTracksBy,
  setZoom,
} from "../timeline/view";

const BUTTON =
  "rounded px-2 py-1 text-xs text-zinc-300 hover:bg-zinc-800 " +
  "disabled:cursor-default disabled:text-zinc-600 disabled:hover:bg-transparent";

/** Toolbar height, px (h-9). Part of the panel-height computation. */
const TOOLBAR_H = 36;

/** Slider position 0–100 ↔ zoom, log-scaled so both ends feel usable. */
function zoomToSlider(pxPerSec: number): number {
  return (
    (Math.log(pxPerSec / MIN_PX_PER_SEC) /
      Math.log(MAX_PX_PER_SEC / MIN_PX_PER_SEC)) *
    100
  );
}

function sliderToZoom(value: number): number {
  return (
    MIN_PX_PER_SEC * Math.pow(MAX_PX_PER_SEC / MIN_PX_PER_SEC, value / 100)
  );
}

// --- Track-flag icons (inline SVG — WebKitGTK-safe, theme-consistent) ---

function LockIcon({ locked }: { locked: boolean }) {
  return (
    <svg viewBox="0 0 16 16" className="h-3.5 w-3.5" fill="none">
      <rect
        x="3.5"
        y="7"
        width="9"
        height="6.5"
        rx="1.2"
        fill={locked ? "currentColor" : "none"}
        stroke="currentColor"
        strokeWidth="1.4"
      />
      {locked ? (
        <path d="M5.5 7V5a2.5 2.5 0 0 1 5 0v2" stroke="currentColor" strokeWidth="1.4" />
      ) : (
        <path d="M5.5 7V5a2.5 2.5 0 0 1 5-.6" stroke="currentColor" strokeWidth="1.4" />
      )}
    </svg>
  );
}

function MuteIcon({ muted }: { muted: boolean }) {
  return (
    <svg viewBox="0 0 16 16" className="h-3.5 w-3.5" fill="none">
      <path
        d="M2.5 6v4h2.6L9 13V3L5.1 6H2.5Z"
        fill="currentColor"
        stroke="currentColor"
        strokeWidth="1"
        strokeLinejoin="round"
      />
      {muted ? (
        <path d="M10.8 6.2l4 3.6m0-3.6l-4 3.6" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" />
      ) : (
        <path d="M11 5.5a3.5 3.5 0 0 1 0 5" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" />
      )}
    </svg>
  );
}

function EyeIcon({ hidden }: { hidden: boolean }) {
  return (
    <svg viewBox="0 0 16 16" className="h-3.5 w-3.5" fill="none">
      <path
        d="M1.5 8s2.4-4 6.5-4 6.5 4 6.5 4-2.4 4-6.5 4S1.5 8 1.5 8Z"
        stroke="currentColor"
        strokeWidth="1.3"
      />
      <circle cx="8" cy="8" r="1.8" fill="currentColor" />
      {hidden && (
        <path d="M2.5 13.5l11-11" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" />
      )}
    </svg>
  );
}

function FlagButton({
  active,
  activeClass,
  title,
  onClick,
  children,
}: {
  active: boolean;
  activeClass: string;
  title: string;
  onClick: () => void;
  children: React.ReactNode;
}) {
  return (
    <button
      title={title}
      onClick={(e) => {
        e.stopPropagation();
        onClick();
      }}
      className={`rounded p-0.5 ${
        active ? activeClass : "text-zinc-600 hover:text-zinc-300"
      }`}
    >
      {children}
    </button>
  );
}

// --- Header context menu -------------------------------------------------

interface MenuState {
  x: number;
  y: number;
  trackId: number;
}

function setFlag(track: Track, flag: TrackFlag, value: boolean): void {
  engineSetTrackFlag(track.id, flag, value).catch((err) =>
    toast(String(err), "error"),
  );
}

function TrackContextMenu({
  menu,
  tracks,
  onClose,
}: {
  menu: MenuState;
  tracks: Track[];
  onClose: () => void;
}) {
  const index = tracks.findIndex((t) => t.id === menu.trackId);
  const track = index >= 0 ? tracks[index] : null;

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onClose]);

  if (!track) return null;
  const sameKind = tracks.filter((t) => t.kind === track.kind);
  const lastOfKind = sameKind.length <= 1;

  const run = (op: Promise<unknown>) => {
    op.catch((err) => toast(String(err), "error"));
    onClose();
  };

  const ITEM =
    "block w-full px-3 py-1.5 text-left text-xs text-zinc-300 hover:bg-zinc-800 " +
    "disabled:cursor-default disabled:text-zinc-600 disabled:hover:bg-transparent";

  return (
    <div className="fixed inset-0 z-[70]" onPointerDown={onClose} onContextMenu={(e) => e.preventDefault()}>
      <div
        className="absolute z-[71] w-48 rounded-md border border-zinc-700 bg-zinc-900 py-1 shadow-xl shadow-black/50"
        style={{ left: menu.x, top: Math.min(menu.y, window.innerHeight - 180) }}
        onPointerDown={(e) => e.stopPropagation()}
      >
        {track.kind === "video" ? (
          <button
            className={ITEM}
            onClick={() => run(engineAddTrack("video", index))}
          >
            Add video track above
          </button>
        ) : (
          <button
            className={ITEM}
            onClick={() => run(engineAddTrack("audio", index + 1))}
          >
            Add audio track below
          </button>
        )}
        <div className="my-1 h-px bg-zinc-800" />
        <button
          className={ITEM}
          disabled={index === 0}
          onClick={() => run(engineMoveTrack(track.id, index - 1))}
        >
          Move up
        </button>
        <button
          className={ITEM}
          disabled={index >= tracks.length - 1}
          onClick={() => run(engineMoveTrack(track.id, index + 1))}
        >
          Move down
        </button>
        <div className="my-1 h-px bg-zinc-800" />
        <button
          className={`${ITEM} ${lastOfKind || track.locked ? "" : "text-red-400"}`}
          disabled={lastOfKind || track.locked}
          title={
            lastOfKind
              ? `The last ${track.kind} track can't be removed`
              : track.locked
                ? "Unlock the track first"
                : undefined
          }
          onClick={() => run(engineRemoveTrack(track.id))}
        >
          Remove track
        </button>
      </div>
    </div>
  );
}

// --- Track header rows ----------------------------------------------------

function TrackHeader({
  track,
  onMenu,
}: {
  track: Track;
  onMenu: (e: React.MouseEvent) => void;
}) {
  return (
    <div
      style={{ height: laneHeight(track.kind) }}
      onContextMenu={onMenu}
      className={`flex shrink-0 flex-col justify-center gap-0.5 border-b border-zinc-800/60 px-2 ${
        track.hidden ? "opacity-60" : ""
      }`}
    >
      <div className="flex items-center justify-between">
        <span
          className={`text-xs font-medium ${
            track.locked ? "text-zinc-500" : "text-zinc-300"
          }`}
        >
          {track.name}
        </span>
        <span className="text-[9px] uppercase tracking-wider text-zinc-600">
          {track.kind}
        </span>
      </div>
      <div className="flex items-center gap-0.5">
        <FlagButton
          active={track.locked}
          activeClass="text-amber-400"
          title={track.locked ? "Unlock track" : "Lock track (rejects edits)"}
          onClick={() => setFlag(track, "locked", !track.locked)}
        >
          <LockIcon locked={track.locked} />
        </FlagButton>
        <FlagButton
          active={track.muted}
          activeClass="text-red-400"
          title={track.muted ? "Unmute track" : "Mute track audio"}
          onClick={() => setFlag(track, "muted", !track.muted)}
        >
          <MuteIcon muted={track.muted} />
        </FlagButton>
        {track.kind === "video" && (
          <FlagButton
            active={track.hidden}
            activeClass="text-sky-400"
            title={
              track.hidden
                ? "Show track"
                : "Hide track (excluded from preview and export)"
            }
            onClick={() => setFlag(track, "hidden", !track.hidden)}
          >
            <EyeIcon hidden={track.hidden} />
          </FlagButton>
        )}
      </div>
    </div>
  );
}

// --- Transition picker (double-click on a chip swaps the type) ----------

function TransitionPickerMenu({
  picker,
  onClose,
}: {
  picker: TransitionPicker;
  onClose: () => void;
}) {
  const [catalog, setCatalog] = useState<TransitionDef[]>([]);
  const transitions = useProjectStore((s) => s.transitions);
  const span = transitions.find((t) => t.fromClipId === picker.clipId);

  useEffect(() => {
    let alive = true;
    cachedTransitionList()
      .then((defs) => {
        if (alive) setCatalog(defs);
      })
      .catch(() => undefined);
    return () => {
      alive = false;
    };
  }, []);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onClose]);

  if (!span) return null;

  const swap = (kind: string) => {
    engineSetTransition(picker.clipId, {
      kind,
      duration: span.requested,
    }).catch((err) => toast(String(err), "error"));
    onClose();
  };
  const remove = () => {
    engineSetTransition(picker.clipId, null).catch((err) =>
      toast(String(err), "error"),
    );
    onClose();
  };

  const ITEM =
    "block w-full px-3 py-1 text-left text-xs hover:bg-zinc-800 " +
    "text-zinc-300";

  return (
    <div className="fixed inset-0 z-[70]" onPointerDown={onClose}>
      <div
        className="absolute z-[71] max-h-72 w-44 overflow-y-auto rounded-md border border-zinc-700 bg-zinc-900 py-1 shadow-xl shadow-black/50"
        style={{
          left: Math.min(picker.x, window.innerWidth - 190),
          top: Math.min(picker.y, window.innerHeight - 300),
        }}
        onPointerDown={(e) => e.stopPropagation()}
      >
        {catalog.map((def) => (
          <button
            key={def.id}
            className={`${ITEM} ${def.id === span.kind ? "text-violet-300" : ""}`}
            onClick={() => swap(def.id)}
          >
            {def.id === span.kind ? "✓ " : ""}
            {def.label}
          </button>
        ))}
        <div className="my-1 h-px bg-zinc-800" />
        <button className={`${ITEM} text-red-400`} onClick={remove}>
          Remove transition
        </button>
      </div>
    </div>
  );
}

function Timeline() {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const project = useProjectStore((s) => s.project);
  const undoDepth = useProjectStore((s) => s.undoDepth);
  const redoDepth = useProjectStore((s) => s.redoDepth);
  const hasSelection = useProjectStore((s) => s.selection.length > 0);
  const snapEnabled = useProjectStore((s) => s.snapEnabled);
  const setSnapEnabled = useProjectStore((s) => s.setSnapEnabled);
  const pxPerSec = useProjectStore((s) => s.pxPerSec);
  const trackScrollPx = useProjectStore((s) => s.trackScrollPx);
  const transitionPicker = useProjectStore((s) => s.transitionPicker);
  const setTransitionPicker = useProjectStore((s) => s.setTransitionPicker);
  const [menu, setMenu] = useState<MenuState | null>(null);

  useEffect(() => {
    startEngineSync();
    const canvas = canvasRef.current;
    if (!canvas) return;
    return createTimelineController(canvas);
  }, []);

  const tracks = project?.tracks ?? [];

  // The panel grows with the track count (up to 45% of the window);
  // beyond that the lanes scroll vertically.
  const panelHeight = Math.max(
    256,
    TOOLBAR_H + RULER_H + lanesHeight(tracks) + 10,
  );

  return (
    <section
      className="flex shrink-0 flex-col border-t border-zinc-800 bg-zinc-900"
      style={{ height: panelHeight, maxHeight: "45vh" }}
    >
      <div className="flex h-9 shrink-0 items-center gap-1 border-b border-zinc-800 px-2">
        <button
          className={BUTTON}
          disabled={!project}
          title="Split at playhead (S)"
          onClick={() => void splitAtPlayhead()}
        >
          Split
        </button>
        <button
          className={BUTTON}
          disabled={!hasSelection}
          title="Delete selected (Del)"
          onClick={() => void deleteSelection(false)}
        >
          Delete
        </button>
        <button
          className={BUTTON}
          disabled={!hasSelection}
          title="Ripple delete selected — later clips close the gap (Shift+Del)"
          onClick={() => void deleteSelection(true)}
        >
          Ripple
        </button>
        <div className="mx-1 h-4 w-px bg-zinc-800" />
        <button
          className={BUTTON}
          disabled={undoDepth === 0}
          title="Undo (Ctrl+Z)"
          onClick={() => void undo()}
        >
          Undo
        </button>
        <button
          className={BUTTON}
          disabled={redoDepth === 0}
          title="Redo (Ctrl+Shift+Z)"
          onClick={() => void redo()}
        >
          Redo
        </button>
        <div className="mx-1 h-4 w-px bg-zinc-800" />
        <button
          className={
            snapEnabled
              ? "rounded bg-sky-600/25 px-2 py-1 text-xs text-sky-300 hover:bg-sky-600/40"
              : BUTTON
          }
          title={snapEnabled ? "Snapping on — click to disable" : "Snapping off"}
          onClick={() => setSnapEnabled(!snapEnabled)}
        >
          Snap
        </button>
        <div className="flex flex-1 items-center justify-end gap-2 text-xs text-zinc-500">
          <button
            className={BUTTON}
            disabled={!project}
            title="Seed 3 video lanes × 20 clips + audio (dev — timeline perf acceptance)"
            onClick={() => void seedDummyClips()}
          >
            Seed lanes
          </button>
          <button
            className={BUTTON}
            disabled={!project}
            title="Build a 12-cut timeline from the first 3 imported videos (dev — playback acceptance)"
            onClick={() => void seedCutTimeline()}
          >
            Seed cuts
          </button>
          <span>Zoom</span>
          <input
            type="range"
            min={0}
            max={100}
            step={1}
            value={zoomToSlider(pxPerSec)}
            onChange={(e) => setZoom(sliderToZoom(Number(e.target.value)))}
            className="w-28 accent-sky-500"
          />
        </div>
      </div>
      <div className="flex min-h-0 flex-1">
        <div
          className="flex w-36 shrink-0 flex-col overflow-hidden border-r border-zinc-800 bg-zinc-900"
          onWheel={(e) => scrollTracksBy(e.deltaY)}
        >
          <div style={{ height: RULER_H }} className="shrink-0" />
          <div
            className="min-h-0 flex-1 overflow-hidden"
            data-testid="track-headers"
          >
            <div style={{ transform: `translateY(${-trackScrollPx}px)` }}>
              {tracks.length > 0
                ? tracks.map((t) => (
                    <TrackHeader
                      key={t.id}
                      track={t}
                      onMenu={(e) => {
                        e.preventDefault();
                        setMenu({ x: e.clientX, y: e.clientY, trackId: t.id });
                      }}
                    />
                  ))
                : ["V1", "A1"].map((name) => (
                    <div
                      key={name}
                      style={{
                        height: laneHeight(name.startsWith("V") ? "video" : "audio"),
                      }}
                      className="flex shrink-0 items-center border-b border-zinc-800/60 px-2 text-xs text-zinc-600"
                    >
                      {name}
                    </div>
                  ))}
            </div>
          </div>
        </div>
        <div className="relative min-w-0 flex-1 overflow-hidden bg-zinc-950">
          <canvas
            ref={canvasRef}
            className="absolute inset-0 h-full w-full touch-none"
          />
        </div>
      </div>
      {menu && (
        <TrackContextMenu
          menu={menu}
          tracks={tracks}
          onClose={() => setMenu(null)}
        />
      )}
      {transitionPicker && (
        <TransitionPickerMenu
          picker={transitionPicker}
          onClose={() => setTransitionPicker(null)}
        />
      )}
    </section>
  );
}

export default Timeline;
