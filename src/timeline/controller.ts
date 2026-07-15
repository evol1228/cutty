// Timeline canvas controller: sizing/DPR, the dirty-flag draw loop, and
// the pointer/keyboard gesture machine. Every gesture becomes engine
// commands over IPC — drags open an engine transaction on the first real
// movement, stream transient commands while the mouse moves, and commit
// exactly one undo entry on release. Snap positions come from the engine;
// the only local math is pixel↔time conversion and hit-testing.

import type { Clip, Track, TrimEdge } from "../lib/engineIpc";
import {
  engineAddClip,
  engineBeginTransaction,
  engineCommitTransaction,
  engineMoveClip,
  engineRollbackTransaction,
  engineSnapClipMove,
  engineSnapTime,
  engineTrimClip,
} from "../lib/engineIpc";
import { playbackSeek, playbackToggle } from "../lib/ipc";
import { useMediaStore } from "../state/mediaStore";
import { useProjectStore } from "../state/projectStore";
import { toast } from "../state/toastStore";
import {
  deleteSelection,
  goToEnd,
  goToStart,
  redo,
  splitAtPlayhead,
  stepFrame,
  trimSelectedToPlayhead,
  undo,
} from "./actions";
import { requestDraw, setDrawCallback } from "./dirty";
import {
  setTimelineDropTarget,
  type DragMedia,
  type TimelineDropTarget,
} from "./poolDrag";
import { drawTimeline, type TimelineOverlay } from "./renderer";
import {
  pxToDuration,
  RULER_H,
  timeToX,
  TRACK_H,
  view,
  xToTime,
  zoomBy,
} from "./view";

/** Width of the trim-handle zone at each clip edge, CSS px. */
const EDGE_PX = 6;
/** Pointer travel before a press becomes a drag, CSS px. */
const DRAG_THRESHOLD_PX = 3;
/** Snap radius, CSS px (converted to seconds at the current zoom). */
const SNAP_PX = 8;

type Zone = "start" | "end" | "body";

type Hit =
  | { region: "ruler" }
  | { region: "outside" }
  | { region: "lane"; track: Track; clip: Clip | null; zone: Zone | null };

type Gesture =
  | { type: "idle" }
  | {
      type: "pending";
      startX: number;
      startY: number;
      clip: Clip;
      zone: Zone;
      grabOffsetSec: number;
    }
  | { type: "move"; clipId: number; grabOffsetSec: number; cancelled: boolean }
  | { type: "trim"; clipId: number; edge: TrimEdge; cancelled: boolean }
  | { type: "pan"; lastClientX: number }
  | { type: "scrub" };

/**
 * Attach the renderer and all interaction handlers to the timeline
 * canvas. Returns a dispose function.
 */
export function createTimelineController(
  canvas: HTMLCanvasElement,
): () => void {
  const maybeContainer = canvas.parentElement;
  const maybeCtx = canvas.getContext("2d");
  if (!maybeContainer || !maybeCtx) {
    throw new Error("timeline canvas must be mounted inside a container");
  }
  const container: HTMLElement = maybeContainer;
  const ctx: CanvasRenderingContext2D = maybeCtx;

  const overlay: TimelineOverlay = { snapLineSec: null, dropPreview: null };
  let gesture: Gesture = { type: "idle" };
  let lastPointerX: number | null = null;
  let disposed = false;

  // --- Serialized engine-command queue -------------------------------
  //
  // Pointer moves outrun IPC, so gesture commands go through a tiny
  // queue: ordered tasks (begin/commit/rollback) run FIFO, while
  // per-mousemove tasks coalesce into a single latest-wins slot. One
  // task runs at a time, so the engine always sees begin → moves →
  // commit in order and stale intermediate positions are skipped.

  const fifo: Array<() => Promise<void>> = [];
  let coalesced: (() => Promise<void>) | null = null;
  let pumping = false;

  function enqueue(task: () => Promise<void>): void {
    fifo.push(task);
    void pump();
  }

  function coalesce(task: () => Promise<void>): void {
    coalesced = task;
    void pump();
  }

  async function pump(): Promise<void> {
    if (pumping) return;
    pumping = true;
    try {
      while (fifo.length > 0 || coalesced !== null) {
        let task: () => Promise<void>;
        if (fifo.length > 0) {
          task = fifo.shift() as () => Promise<void>;
        } else {
          task = coalesced as () => Promise<void>;
          coalesced = null;
        }
        try {
          await task();
        } catch (err) {
          console.warn("engine command failed", err);
        }
      }
    } finally {
      pumping = false;
    }
  }

  // --- Gesture command bodies ----------------------------------------

  /** One transient move step; overlap rejections are expected mid-drag. */
  async function applyMove(clipId: number, desiredIn: number): Promise<void> {
    const { snapEnabled, playheadSec } = useProjectStore.getState();
    let target = desiredIn;
    let snapPoint: number | null = null;
    if (snapEnabled) {
      const snapped = await engineSnapClipMove(
        clipId,
        desiredIn,
        pxToDuration(SNAP_PX),
        playheadSec,
      );
      target = snapped.timelineIn;
      snapPoint = snapped.snapPoint;
    }
    try {
      await engineMoveClip(clipId, target);
    } catch {
      // Would overlap a neighbor — the clip stays at its last valid spot.
    }
    overlay.snapLineSec = snapPoint;
    requestDraw();
  }

  /** One transient trim step; the engine clamps to media/duration bounds. */
  async function applyTrim(
    clipId: number,
    edge: TrimEdge,
    to: number,
  ): Promise<void> {
    const { snapEnabled, playheadSec } = useProjectStore.getState();
    let target = to;
    let snapPoint: number | null = null;
    if (snapEnabled) {
      snapPoint = await engineSnapTime(to, pxToDuration(SNAP_PX), playheadSec, [
        clipId,
      ]);
      if (snapPoint !== null) target = snapPoint;
    }
    try {
      await engineTrimClip(clipId, edge, target);
    } catch {
      // Would overlap a neighbor — keep the last valid trim.
    }
    overlay.snapLineSec = snapPoint;
    requestDraw();
  }

  function finishGesture(finalStep: (() => Promise<void>) | null): void {
    enqueue(async () => {
      coalesced = null; // a stale intermediate step is superseded
      if (finalStep) await finalStep();
      await engineCommitTransaction();
      overlay.snapLineSec = null;
      requestDraw();
    });
  }

  function cancelGesture(): void {
    enqueue(async () => {
      coalesced = null;
      await engineRollbackTransaction();
      overlay.snapLineSec = null;
      requestDraw();
    });
  }

  // --- Pool-item drop target -------------------------------------------
  //
  // The media pool starts pointer drags (poolDrag.ts); this target turns
  // them into a live drop preview and, on release, one AddClip command.
  // Track choice is by media kind — video files land on the video track
  // (carrying their own audio via clip volume), audio files on the audio
  // track. Only the drop X matters.

  function dropTrack(media: DragMedia): { track: Track; index: number } | null {
    const project = useProjectStore.getState().project;
    if (!project) return null;
    const kind = media.hasVideo ? "video" : "audio";
    const index = project.tracks.findIndex((t) => t.kind === kind);
    if (index < 0) return null;
    return { track: project.tracks[index], index };
  }

  /** Snap a prospective drop: both clip edges compete, like clip moves. */
  async function snappedDropIn(
    canvasX: number,
    media: DragMedia,
  ): Promise<{ inSec: number; snapPoint: number | null }> {
    const desired = Math.max(0, xToTime(canvasX));
    const { snapEnabled, playheadSec } = useProjectStore.getState();
    if (!snapEnabled) return { inSec: desired, snapPoint: null };
    const threshold = pxToDuration(SNAP_PX);
    const [left, right] = await Promise.all([
      engineSnapTime(desired, threshold, playheadSec, []),
      engineSnapTime(desired + media.durationSec, threshold, playheadSec, []),
    ]);
    const candidates: Array<{ inSec: number; snapPoint: number; dist: number }> =
      [];
    if (left !== null) {
      candidates.push({
        inSec: left,
        snapPoint: left,
        dist: Math.abs(left - desired),
      });
    }
    if (right !== null && right - media.durationSec >= 0) {
      candidates.push({
        inSec: right - media.durationSec,
        snapPoint: right,
        dist: Math.abs(right - (desired + media.durationSec)),
      });
    }
    candidates.sort((a, b) => a.dist - b.dist);
    const best = candidates[0];
    return best
      ? { inSec: Math.max(0, best.inSec), snapPoint: best.snapPoint }
      : { inSec: desired, snapPoint: null };
  }

  function clearDropPreview(): void {
    if (overlay.dropPreview !== null || overlay.snapLineSec !== null) {
      overlay.dropPreview = null;
      overlay.snapLineSec = null;
      requestDraw();
    }
  }

  const dropTarget: TimelineDropTarget = {
    over: (clientX, clientY, media) => {
      const rect = canvas.getBoundingClientRect();
      const x = clientX - rect.left;
      const y = clientY - rect.top;
      if (x < 0 || y < 0 || x > rect.width || y > rect.height) {
        clearDropPreview();
        return;
      }
      const target = dropTrack(media);
      if (!target) return;
      coalesce(async () => {
        const { inSec, snapPoint } = await snappedDropIn(x, media);
        overlay.dropPreview = {
          trackIndex: target.index,
          inSec,
          durSec: media.durationSec,
        };
        overlay.snapLineSec = snapPoint;
        requestDraw();
      });
    },
    drop: (clientX, clientY, media) => {
      const rect = canvas.getBoundingClientRect();
      const x = clientX - rect.left;
      const y = clientY - rect.top;
      const inside = x >= 0 && y >= 0 && x <= rect.width && y <= rect.height;
      enqueue(async () => {
        coalesced = null; // a stale preview update is superseded
        clearDropPreview();
        if (!inside) return;
        const target = dropTrack(media);
        if (!target) return;
        const { inSec } = await snappedDropIn(x, media);
        try {
          const clipId = await engineAddClip(
            target.track.id,
            media.mediaId,
            inSec,
            0,
            media.durationSec,
          );
          useProjectStore.getState().setSelection([clipId]);
        } catch {
          toast(
            `No room for ${media.name} there — it would overlap another clip.`,
            "error",
          );
        }
      });
    },
    leave: clearDropPreview,
  };

  // --- Hit testing -----------------------------------------------------

  function hitTest(x: number, y: number): Hit {
    const project = useProjectStore.getState().project;
    if (y < RULER_H) return { region: "ruler" };
    if (!project) return { region: "outside" };
    const trackIndex = Math.floor((y - RULER_H) / TRACK_H);
    if (trackIndex < 0 || trackIndex >= project.tracks.length) {
      return { region: "outside" };
    }
    const track = project.tracks[trackIndex];
    for (const clip of track.clips) {
      const x0 = timeToX(clip.timelineIn);
      const x1 = timeToX(clip.timelineOut);
      if (x < x0 || x > x1) continue;
      const edgeW = Math.min(EDGE_PX, (x1 - x0) / 4);
      const zone: Zone =
        x <= x0 + edgeW ? "start" : x >= x1 - edgeW ? "end" : "body";
      return { region: "lane", track, clip, zone };
    }
    return { region: "lane", track, clip: null, zone: null };
  }

  function cursorFor(hit: Hit): string {
    if (hit.region !== "lane" || !hit.clip) return "default";
    return hit.zone === "body" ? "grab" : "ew-resize";
  }

  function canvasPos(e: { clientX: number; clientY: number }): {
    x: number;
    y: number;
  } {
    const rect = canvas.getBoundingClientRect();
    return { x: e.clientX - rect.left, y: e.clientY - rect.top };
  }

  // --- Scrubbing ---------------------------------------------------------
  //
  // The store playhead moves immediately (snappy marker); the engine seek
  // rides the coalescing queue, so a fast drag sends the engine only the
  // positions it can keep up with — the engine additionally collapses
  // whatever queues up on its side and cancels stale seeks.

  function scrubTo(t: number): void {
    const clamped = Math.max(0, t);
    useProjectStore.getState().setPlayhead(clamped);
    coalesce(async () => {
      await playbackSeek(clamped).catch(() => undefined);
    });
  }

  // --- Pointer handlers -------------------------------------------------

  function onPointerDown(e: PointerEvent): void {
    if (gesture.type !== "idle") return;
    const { x, y } = canvasPos(e);
    const store = useProjectStore.getState();

    if (e.button === 1) {
      e.preventDefault();
      gesture = { type: "pan", lastClientX: e.clientX };
      canvas.setPointerCapture(e.pointerId);
      canvas.style.cursor = "grabbing";
      return;
    }
    if (e.button !== 0) return;

    const hit = hitTest(x, y);
    if (hit.region === "ruler") {
      gesture = { type: "scrub" };
      canvas.setPointerCapture(e.pointerId);
      scrubTo(xToTime(x));
      return;
    }
    if (hit.region === "lane" && hit.clip) {
      if (e.ctrlKey || e.metaKey) {
        store.toggleSelected(hit.clip.id);
        return;
      }
      if (!store.selection.includes(hit.clip.id)) {
        store.setSelection([hit.clip.id]);
      }
      gesture = {
        type: "pending",
        startX: x,
        startY: y,
        clip: hit.clip,
        zone: hit.zone ?? "body",
        grabOffsetSec: xToTime(x) - hit.clip.timelineIn,
      };
      canvas.setPointerCapture(e.pointerId);
      return;
    }
    // Empty lane or below the tracks: clear the selection.
    store.setSelection([]);
  }

  function onPointerMove(e: PointerEvent): void {
    const { x, y } = canvasPos(e);
    lastPointerX = x;

    switch (gesture.type) {
      case "idle": {
        const hit = hitTest(x, y);
        canvas.style.cursor = cursorFor(hit);
        // Missing-media tooltip on hovered clips.
        let title = "";
        if (hit.region === "lane" && hit.clip) {
          const mediaId = hit.clip.mediaId;
          const media = useMediaStore.getState();
          if (media.missingMediaIds.has(mediaId)) {
            const item = media.items.find((i) => i.mediaId === mediaId);
            title = `Missing media: ${item?.path ?? "source file not found"}`;
          }
        }
        if (canvas.title !== title) canvas.title = title;
        return;
      }
      case "pending": {
        const dx = x - gesture.startX;
        const dy = y - gesture.startY;
        if (dx * dx + dy * dy < DRAG_THRESHOLD_PX * DRAG_THRESHOLD_PX) return;
        const { clip, zone, grabOffsetSec } = gesture;
        enqueue(() => engineBeginTransaction());
        if (zone === "body") {
          gesture = {
            type: "move",
            clipId: clip.id,
            grabOffsetSec,
            cancelled: false,
          };
          canvas.style.cursor = "grabbing";
        } else {
          gesture = {
            type: "trim",
            clipId: clip.id,
            edge: zone,
            cancelled: false,
          };
        }
        return;
      }
      case "move": {
        const { clipId, grabOffsetSec } = gesture;
        const desiredIn = xToTime(x) - grabOffsetSec;
        coalesce(() => applyMove(clipId, desiredIn));
        return;
      }
      case "trim": {
        const { clipId, edge } = gesture;
        const to = xToTime(x);
        coalesce(() => applyTrim(clipId, edge, to));
        return;
      }
      case "pan": {
        const dx = gesture.lastClientX - e.clientX;
        gesture.lastClientX = e.clientX;
        view.scrollPx = Math.max(0, view.scrollPx + dx);
        requestDraw();
        return;
      }
      case "scrub":
        scrubTo(xToTime(x));
        return;
    }
  }

  function onPointerUp(e: PointerEvent): void {
    const { x } = canvasPos(e);
    switch (gesture.type) {
      case "move": {
        const { clipId, grabOffsetSec, cancelled } = gesture;
        if (!cancelled) {
          const desiredIn = xToTime(x) - grabOffsetSec;
          finishGesture(() => applyMove(clipId, desiredIn));
        }
        break;
      }
      case "trim": {
        const { clipId, edge, cancelled } = gesture;
        if (!cancelled) {
          const to = xToTime(x);
          finishGesture(() => applyTrim(clipId, edge, to));
        }
        break;
      }
      default:
        break;
    }
    gesture = { type: "idle" };
    canvas.style.cursor = "default";
  }

  function onPointerCancel(): void {
    if (gesture.type === "move" || gesture.type === "trim") {
      cancelGesture();
    }
    gesture = { type: "idle" };
    canvas.style.cursor = "default";
  }

  function onWheel(e: WheelEvent): void {
    e.preventDefault();
    const { x } = canvasPos(e);
    if (e.ctrlKey || e.metaKey) {
      // Zoom around the cursor; deltaY < 0 (scroll up / pinch out) zooms in.
      zoomBy(Math.exp(-e.deltaY * 0.0015), x);
    } else {
      // Plain and Shift+wheel both pan horizontally.
      const delta = e.deltaX !== 0 ? e.deltaX : e.deltaY;
      view.scrollPx = Math.max(0, view.scrollPx + delta);
      requestDraw();
    }
  }

  // --- Keyboard ----------------------------------------------------------

  function isEditableTarget(e: KeyboardEvent): boolean {
    const t = e.target;
    if (!(t instanceof HTMLElement)) return false;
    return (
      t.tagName === "INPUT" ||
      t.tagName === "TEXTAREA" ||
      t.tagName === "SELECT" ||
      t.isContentEditable
    );
  }

  function onKeyDown(e: KeyboardEvent): void {
    if (isEditableTarget(e)) return;

    // Mid-drag, only Escape (cancel) is live.
    if (gesture.type === "move" || gesture.type === "trim") {
      if (e.key === "Escape") {
        gesture.cancelled = true;
        cancelGesture();
        gesture = { type: "idle" };
        canvas.style.cursor = "default";
      }
      return;
    }

    const ctrl = e.ctrlKey || e.metaKey;
    const key = e.key;

    if (ctrl && key.toLowerCase() === "z") {
      e.preventDefault();
      void (e.shiftKey ? redo() : undo());
      return;
    }
    if (ctrl && key.toLowerCase() === "y") {
      e.preventDefault();
      void redo();
      return;
    }
    if (ctrl && key.toLowerCase() === "b") {
      e.preventDefault();
      void splitAtPlayhead();
      return;
    }
    if (ctrl) return;

    switch (key) {
      case " ": {
        // A focused button consumes Space itself (e.g. the play button —
        // toggling here too would double-fire).
        const target = e.target as HTMLElement | null;
        if (target?.closest("button")) break;
        e.preventDefault();
        if (!e.repeat) void playbackToggle().catch(() => undefined);
        break;
      }
      case "s":
      case "S":
        void splitAtPlayhead();
        break;
      case "Delete":
      case "Backspace":
        void deleteSelection(e.shiftKey);
        break;
      case "q":
      case "Q":
        void trimSelectedToPlayhead("start");
        break;
      case "w":
      case "W":
        void trimSelectedToPlayhead("end");
        break;
      case "ArrowLeft":
        e.preventDefault();
        stepFrame(-1);
        break;
      case "ArrowRight":
        e.preventDefault();
        stepFrame(1);
        break;
      case "+":
      case "=":
        zoomBy(1.25, lastPointerX ?? undefined);
        break;
      case "-":
      case "_":
        zoomBy(0.8, lastPointerX ?? undefined);
        break;
      case "Home":
        e.preventDefault();
        goToStart();
        break;
      case "End":
        e.preventDefault();
        goToEnd();
        break;
      case "Escape":
        useProjectStore.getState().setSelection([]);
        break;
      default:
        break;
    }
  }

  // --- Sizing, DPR, and the draw loop -------------------------------------

  function syncCanvasSize(): boolean {
    const dpr = window.devicePixelRatio || 1;
    const width = container.clientWidth;
    const height = container.clientHeight;
    view.widthPx = width;
    const deviceW = Math.max(1, Math.round(width * dpr));
    const deviceH = Math.max(1, Math.round(height * dpr));
    if (canvas.width !== deviceW || canvas.height !== deviceH) {
      canvas.width = deviceW;
      canvas.height = deviceH;
      return true;
    }
    return false;
  }

  function draw(): void {
    if (disposed) return;
    syncCanvasSize();
    const dpr = window.devicePixelRatio || 1;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    drawTimeline(ctx, container.clientWidth, container.clientHeight, overlay);
  }

  /** Re-arm a one-shot listener for devicePixelRatio changes (monitor
   * moves, fractional scaling) — each change needs a fresh media query. */
  function armDprListener(): void {
    const dprQuery = window.matchMedia(
      `(resolution: ${window.devicePixelRatio}dppx)`,
    );
    dprQuery.addEventListener(
      "change",
      () => {
        if (disposed) return;
        requestDraw();
        armDprListener();
      },
      { once: true },
    );
  }

  setDrawCallback(draw);
  const resizeObserver = new ResizeObserver(() => requestDraw());
  resizeObserver.observe(container);
  armDprListener();

  // Any store change (engine snapshot, selection, playhead, zoom, snap
  // toggle, thumbnails, missing-media flags) marks the canvas dirty;
  // nothing redraws while idle.
  const unsubscribe = useProjectStore.subscribe(() => requestDraw());
  const unsubscribeMedia = useMediaStore.subscribe(() => requestDraw());
  setTimelineDropTarget(dropTarget);

  canvas.addEventListener("pointerdown", onPointerDown);
  canvas.addEventListener("pointermove", onPointerMove);
  canvas.addEventListener("pointerup", onPointerUp);
  canvas.addEventListener("pointercancel", onPointerCancel);
  canvas.addEventListener("wheel", onWheel, { passive: false });
  window.addEventListener("keydown", onKeyDown);

  requestDraw();

  return () => {
    disposed = true;
    setDrawCallback(null);
    setTimelineDropTarget(null);
    resizeObserver.disconnect();
    unsubscribe();
    unsubscribeMedia();
    canvas.removeEventListener("pointerdown", onPointerDown);
    canvas.removeEventListener("pointermove", onPointerMove);
    canvas.removeEventListener("pointerup", onPointerUp);
    canvas.removeEventListener("pointercancel", onPointerCancel);
    canvas.removeEventListener("wheel", onWheel);
    window.removeEventListener("keydown", onKeyDown);
  };
}
