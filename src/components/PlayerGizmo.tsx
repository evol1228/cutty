// Transform gizmo over the player canvas: the selected video *or text*
// clip gets a bounding box — drag the body to reposition, corner handles
// for uniform scale, the lollipop above the box to rotate. All geometry
// mirrors the compositor's layer placement (video: contain-fit in the
// project canvas; text: the engine-measured block box), so the box hugs
// the rendered pixels; every mutation goes through the engine's
// transaction API (one drag = one undo entry), and the paused-frame
// re-composite makes the picture chase the box live.

import { useEffect, useRef, useState } from "react";
import type { RefObject } from "react";
import {
  engineBeginTransaction,
  engineCommitTransaction,
  engineRollbackTransaction,
  engineSetClipTransform,
  textMeasure,
  type Clip,
  type Track,
  type Transform,
} from "../lib/engineIpc";
import { useMediaStore } from "../state/mediaStore";
import { useProjectStore } from "../state/projectStore";

/** Scale clamp, matching the Inspector's 1%–1000%. */
const MIN_SCALE = 0.01;
const MAX_SCALE = 10;
/** Rotation-handle stem length, CSS px (screen-constant). */
const ROT_STEM = 22;
const HANDLE = 8;

interface Rect {
  left: number;
  top: number;
  width: number;
  height: number;
}

/** The canvas's CSS box relative to the overlay container, kept fresh
 * across resizes of either. */
function useCanvasRect(
  canvasRef: RefObject<HTMLCanvasElement | null>,
  containerRef: RefObject<HTMLDivElement | null>,
): Rect | null {
  const [rect, setRect] = useState<Rect | null>(null);
  useEffect(() => {
    const canvas = canvasRef.current;
    const container = containerRef.current;
    if (!canvas || !container) return;
    const measure = () => {
      const c = canvas.getBoundingClientRect();
      const p = container.getBoundingClientRect();
      setRect({
        left: c.left - p.left,
        top: c.top - p.top,
        width: c.width,
        height: c.height,
      });
    };
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(canvas);
    ro.observe(container);
    return () => ro.disconnect();
  }, [canvasRef, containerRef]);
  return rect;
}

/** The selected clip when it is exactly one, on a video or text track
 * (the kinds the gizmo can place). */
function selectedGizmoClip(): { clip: Clip; track: Track } | null {
  const { project, selection } = useProjectStore.getState();
  if (!project || selection.length !== 1) return null;
  for (const track of project.tracks) {
    const clip = track.clips.find((c) => c.id === selection[0]);
    if (clip) {
      return track.kind === "video" || track.kind === "text"
        ? { clip, track }
        : null;
    }
  }
  return null;
}

type Mode = "move" | "scale" | "rotate";

interface Gesture {
  mode: Mode;
  pointerId: number;
  clipId: number;
  /** Transform at gesture start. */
  t0: Transform;
  /** Pointer position at gesture start, container px. */
  startPx: { x: number; y: number };
  /** Box center at gesture start, container px. */
  centerPx: { x: number; y: number };
  cancelled: boolean;
}

function PlayerGizmo({
  canvasRef,
  containerRef,
}: {
  canvasRef: RefObject<HTMLCanvasElement | null>;
  containerRef: RefObject<HTMLDivElement | null>;
}) {
  const project = useProjectStore((s) => s.project);
  const selection = useProjectStore((s) => s.selection);
  const playheadSec = useProjectStore((s) => s.playheadSec);
  const mediaItems = useMediaStore((s) => s.items);
  const rect = useCanvasRect(canvasRef, containerRef);

  const gesture = useRef<Gesture | null>(null);
  const raf = useRef(0);
  const pending = useRef<Transform | null>(null);
  // Live transform during a drag: the engine echoes state back per step,
  // but driving the box from local state keeps it glitch-free.
  const [dragTransform, setDragTransform] = useState<Transform | null>(null);
  // Measured block size (project px) of the selected *text* clip — the
  // engine's own layout, so the box hugs the rendered glyphs. Keyed by
  // the payload; re-measures on content/style edits.
  const [textBlock, setTextBlock] = useState<{
    key: string;
    w: number;
    h: number;
  } | null>(null);

  useEffect(() => {
    const text = selectedGizmoClip()?.clip.text;
    if (!text) return;
    const key = JSON.stringify(text);
    if (textBlock?.key === key) return;
    let alive = true;
    textMeasure(text)
      .then(([w, h]) => {
        if (alive) setTextBlock({ key, w, h });
      })
      .catch(() => undefined);
    return () => {
      alive = false;
    };
  });

  // Escape cancels an in-flight gesture (engine state rolls back).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape" || !gesture.current) return;
      gesture.current.cancelled = true;
      gesture.current = null;
      pending.current = null;
      setDragTransform(null);
      void engineRollbackTransaction().catch(() => undefined);
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, []);

  if (!project || !rect || selection.length !== 1) return null;
  const selected = selectedGizmoClip();
  if (!selected) return null;
  const { clip, track } = selected;
  const isText = track.kind === "text";
  const media = isText
    ? null
    : project.media.find((m) => m.id === clip.mediaId);
  if (!isText && !media?.hasVideo) return null;
  if (track.hidden || track.locked) return null;
  // The box only makes sense while the clip is on screen.
  if (playheadSec < clip.timelineIn || playheadSec >= clip.timelineOut) {
    return null;
  }

  // --- project space ↔ container px (mirrors compose::layer_placement) --
  const pw = project.settings.width;
  const ph = project.settings.height;
  const s = Math.min(rect.width / pw, rect.height / ph);
  const offX = rect.left + (rect.width - pw * s) / 2;
  const offY = rect.top + (rect.height - ph * s) / 2;

  // Base box in project px: for video, the source frame fit inside the
  // project canvas (contain, centered — probe supplies dimensions); for
  // text, the engine-measured block at scale 1 (no canvas fit).
  let srcW: number;
  let srcH: number;
  let base: number;
  if (isText) {
    if (!clip.text || textBlock?.key !== JSON.stringify(clip.text)) {
      return null; // measure in flight — the box appears next frame
    }
    srcW = textBlock.w;
    srcH = textBlock.h;
    base = 1;
  } else {
    const info = mediaItems.find((i) => i.mediaId === clip.mediaId)?.info;
    srcW = info?.video?.width ?? pw;
    srcH = info?.video?.height ?? ph;
    base = Math.min(pw / srcW, ph / srcH);
  }

  const t = dragTransform ?? clip.transform;
  // Empty text measures 0×0 — keep a grabbable minimum so the clip can
  // still be placed while its content is blank.
  const boxW = Math.max(srcW * base * t.scale * s, isText ? 24 : 0);
  const boxH = Math.max(srcH * base * t.scale * s, isText ? 24 : 0);
  const cx = (pw / 2 + t.x) * s + offX;
  const cy = (ph / 2 + t.y) * s + offY;

  // --- gesture plumbing --------------------------------------------------

  const sendTransform = (next: Transform) => {
    setDragTransform(next);
    pending.current = next;
    if (raf.current) return;
    raf.current = requestAnimationFrame(() => {
      raf.current = 0;
      const g = gesture.current;
      const value = pending.current;
      pending.current = null;
      if (!g || !value) return;
      void engineSetClipTransform(g.clipId, value).catch(() => undefined);
    });
  };

  const beginGesture = (e: React.PointerEvent) => {
    const mode = (e.target as SVGElement).dataset?.mode as Mode | undefined;
    if (!mode || gesture.current) return;
    e.preventDefault();
    e.stopPropagation();
    const container = containerRef.current;
    if (!container) return;
    const p = container.getBoundingClientRect();
    gesture.current = {
      mode,
      pointerId: e.pointerId,
      clipId: clip.id,
      t0: { ...clip.transform },
      startPx: { x: e.clientX - p.left, y: e.clientY - p.top },
      centerPx: { x: cx, y: cy },
      cancelled: false,
    };
    (e.target as Element).setPointerCapture(e.pointerId);
    void engineBeginTransaction().catch(() => {
      gesture.current = null;
    });
  };

  const moveGesture = (e: React.PointerEvent) => {
    const g = gesture.current;
    const container = containerRef.current;
    if (!g || !container || e.pointerId !== g.pointerId) return;
    const p = container.getBoundingClientRect();
    const px = { x: e.clientX - p.left, y: e.clientY - p.top };

    switch (g.mode) {
      case "move": {
        sendTransform({
          ...g.t0,
          x: g.t0.x + (px.x - g.startPx.x) / s,
          y: g.t0.y + (px.y - g.startPx.y) / s,
        });
        break;
      }
      case "scale": {
        const d0 = Math.hypot(
          g.startPx.x - g.centerPx.x,
          g.startPx.y - g.centerPx.y,
        );
        const d1 = Math.hypot(px.x - g.centerPx.x, px.y - g.centerPx.y);
        if (d0 < 1) break;
        const scale = Math.min(
          MAX_SCALE,
          Math.max(MIN_SCALE, (g.t0.scale * d1) / d0),
        );
        sendTransform({ ...g.t0, scale });
        break;
      }
      case "rotate": {
        const a0 = Math.atan2(
          g.startPx.y - g.centerPx.y,
          g.startPx.x - g.centerPx.x,
        );
        const a1 = Math.atan2(px.y - g.centerPx.y, px.x - g.centerPx.x);
        sendTransform({
          ...g.t0,
          rotation: g.t0.rotation + ((a1 - a0) * 180) / Math.PI,
        });
        break;
      }
    }
  };

  const endGesture = (e: React.PointerEvent) => {
    const g = gesture.current;
    if (!g || e.pointerId !== g.pointerId) return;
    gesture.current = null;
    pending.current = null;
    setDragTransform(null);
    if (!g.cancelled) {
      void engineCommitTransaction().catch(() => undefined);
    }
  };

  /** Interactive shapes carry their gesture mode; the svg root routes
   * pointer events (the shapes opt back into hit-testing). */
  const hot = (mode: Mode, cursor: string) => ({
    "data-mode": mode,
    style: { cursor, pointerEvents: "auto" as const },
  });

  const corners: Array<[number, number, string]> = [
    [-boxW / 2, -boxH / 2, "nwse-resize"],
    [boxW / 2, -boxH / 2, "nesw-resize"],
    [-boxW / 2, boxH / 2, "nesw-resize"],
    [boxW / 2, boxH / 2, "nwse-resize"],
  ];

  return (
    <svg
      data-testid="player-gizmo"
      className="absolute inset-0 h-full w-full"
      style={{ pointerEvents: "none" }}
      onPointerDown={beginGesture}
      onPointerMove={moveGesture}
      onPointerUp={endGesture}
      onPointerCancel={endGesture}
    >
      <g transform={`translate(${cx} ${cy}) rotate(${t.rotation})`}>
        {/* Body: transparent hit area + outline. */}
        <rect
          x={-boxW / 2}
          y={-boxH / 2}
          width={boxW}
          height={boxH}
          fill="transparent"
          stroke="#38bdf8"
          strokeWidth={1.5}
          {...hot("move", "move")}
        />
        {/* Rotation lollipop above the top edge. */}
        <line
          x1={0}
          y1={-boxH / 2}
          x2={0}
          y2={-boxH / 2 - ROT_STEM}
          stroke="#38bdf8"
          strokeWidth={1.5}
        />
        <circle
          cx={0}
          cy={-boxH / 2 - ROT_STEM}
          r={5}
          fill="#0c4a6e"
          stroke="#38bdf8"
          strokeWidth={1.5}
          {...hot("rotate", "grab")}
        />
        {/* Corner scale handles. */}
        {corners.map(([hx, hy, cursor], i) => (
          <rect
            key={i}
            x={hx - HANDLE / 2}
            y={hy - HANDLE / 2}
            width={HANDLE}
            height={HANDLE}
            fill="#e0f2fe"
            stroke="#0284c7"
            strokeWidth={1}
            {...hot("scale", cursor)}
          />
        ))}
      </g>
    </svg>
  );
}

export default PlayerGizmo;
