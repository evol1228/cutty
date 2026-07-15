// Bottom timeline panel: toolbar, track headers, and the canvas-rendered
// timeline (renderer + interactions live in src/timeline/).

import { useEffect, useRef } from "react";
import { startEngineSync } from "../state/engineSync";
import { useProjectStore } from "../state/projectStore";
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
  MAX_PX_PER_SEC,
  MIN_PX_PER_SEC,
  RULER_H,
  setZoom,
  TRACK_H,
} from "../timeline/view";

const BUTTON =
  "rounded px-2 py-1 text-xs text-zinc-300 hover:bg-zinc-800 " +
  "disabled:cursor-default disabled:text-zinc-600 disabled:hover:bg-transparent";

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

function Timeline() {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const project = useProjectStore((s) => s.project);
  const undoDepth = useProjectStore((s) => s.undoDepth);
  const redoDepth = useProjectStore((s) => s.redoDepth);
  const hasSelection = useProjectStore((s) => s.selection.length > 0);
  const snapEnabled = useProjectStore((s) => s.snapEnabled);
  const setSnapEnabled = useProjectStore((s) => s.setSnapEnabled);
  const pxPerSec = useProjectStore((s) => s.pxPerSec);

  useEffect(() => {
    startEngineSync();
    const canvas = canvasRef.current;
    if (!canvas) return;
    return createTimelineController(canvas);
  }, []);

  const tracks = project?.tracks ?? [];

  return (
    <section className="flex h-64 shrink-0 flex-col border-t border-zinc-800 bg-zinc-900">
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
            title="Add 50 dummy clips (dev — timeline perf acceptance)"
            onClick={() => void seedDummyClips()}
          >
            Seed 50
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
        <div className="flex w-24 shrink-0 flex-col border-r border-zinc-800 bg-zinc-900">
          <div style={{ height: RULER_H }} className="shrink-0" />
          {tracks.length > 0
            ? tracks.map((t) => (
                <div
                  key={t.id}
                  style={{ height: TRACK_H }}
                  className="flex shrink-0 items-center justify-between border-b border-zinc-800/60 px-2 text-xs text-zinc-400"
                >
                  <span className="font-medium">{t.name}</span>
                  <span className="uppercase text-[9px] tracking-wider text-zinc-600">
                    {t.kind}
                  </span>
                </div>
              ))
            : ["V1", "A1"].map((name) => (
                <div
                  key={name}
                  style={{ height: TRACK_H }}
                  className="flex shrink-0 items-center border-b border-zinc-800/60 px-2 text-xs text-zinc-600"
                >
                  {name}
                </div>
              ))}
        </div>
        <div className="relative min-w-0 flex-1 overflow-hidden bg-zinc-950">
          <canvas
            ref={canvasRef}
            className="absolute inset-0 h-full w-full touch-none"
          />
        </div>
      </div>
    </section>
  );
}

export default Timeline;
