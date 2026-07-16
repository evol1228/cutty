// Canvas renderer for the timeline. Pure drawing: reads the engine
// snapshot from the store and the viewport from view.ts, paints ruler,
// track lanes, clips, playhead, and snap indicator. No timeline math here
// beyond pixel↔time conversion; clips are drawn exactly where the engine
// says they are.

import type { Clip, Project, Track, TransitionSpan } from "../lib/engineIpc";
import { useMediaStore } from "../state/mediaStore";
import { useProjectStore } from "../state/projectStore";
import { requestDraw } from "./dirty";
import {
  durationToPx,
  laneHeight,
  laneTop,
  RULER_H,
  timeToX,
  view,
  xToTime,
} from "./view";

/** Ghost of a pool item being dragged over the timeline. */
export interface DropPreview {
  trackIndex: number;
  inSec: number;
  durSec: number;
}

/** Cut points highlighted while a transition drags over a video lane. */
export interface TransitionDragOverlay {
  trackIndex: number;
  /** Every cut on the lane (touching clip pairs), seconds. */
  cuts: number[];
  /** The cut that would take the drop (nearest within reach), or null. */
  activeCut: number | null;
}

/** Transient gesture state drawn on top of engine state. */
export interface TimelineOverlay {
  /** Snap indicator line, seconds; null when not snapping. */
  snapLineSec: number | null;
  /** Pool-drag drop preview; null when no drag is over the canvas. */
  dropPreview: DropPreview | null;
  /** Lane a drag is hovering but may not drop into (locked/incompatible);
   * tinted red. Null when no invalid hover. */
  invalidLaneIndex: number | null;
  /** Transition-drag cut highlighting; null when no transition drag. */
  transitionDrag: TransitionDragOverlay | null;
}

const COLORS = {
  background: "#09090b", // zinc-950
  laneEven: "#0d0d10",
  laneSeparator: "#27272a", // zinc-800
  laneLockHatch: "rgba(255,255,255,0.05)",
  laneInvalidFill: "rgba(239,68,68,0.10)", // red-500
  laneInvalidBorder: "rgba(239,68,68,0.55)",
  ruler: "#18181b", // zinc-900
  rulerTick: "#3f3f46", // zinc-700
  rulerMinorTick: "#27272a",
  rulerLabel: "#a1a1aa", // zinc-400
  videoFill: "#0c4a6e", // sky-900
  videoBorder: "#0369a1", // sky-700
  audioFill: "#064e3b", // emerald-900
  audioBorder: "#047857", // emerald-700
  textFill: "#7c2d12", // orange-900
  textBorder: "#c2410c", // orange-700
  selectedFill: { video: "#075985", audio: "#065f46", text: "#9a3412" },
  selectedBorder: "#f59e0b", // amber-500
  missingFill: "#450a0a", // red-950
  missingBorder: "#b91c1c", // red-700
  clipLabel: "#e4e4e7", // zinc-200
  grip: "rgba(255,255,255,0.55)",
  playhead: "#ef4444", // red-500
  snapLine: "#fbbf24", // amber-400
  dropFill: "rgba(14, 165, 233, 0.25)", // sky-500
  dropBorder: "#38bdf8", // sky-400
  emptyHint: "#52525b", // zinc-600
  transitionFill: "#6d28d9", // violet-700
  transitionBorder: "#a78bfa", // violet-400
  transitionSelectedFill: "#7c3aed", // violet-600
  transitionIcon: "#ede9fe", // violet-50
  cutMark: "rgba(167, 139, 250, 0.6)", // violet-400
  cutMarkActive: "#fbbf24", // amber-400
} as const;

/** Transition chip height, px (straddles the cut mid-lane). */
const CHIP_H = 16;
/** Minimum on-screen chip width, px (short transitions stay grabbable). */
const CHIP_MIN_W = 18;

/** Hidden tracks keep their clips visible but faded. */
const HIDDEN_TRACK_ALPHA = 0.35;
/** Locked tracks dim slightly under the hatch. */
const LOCKED_TRACK_ALPHA = 0.6;

/** Major-tick ladder, seconds. Picks the first step wide enough on screen. */
const TICK_STEPS = [
  0.05, 0.1, 0.2, 0.5, 1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 1200, 1800,
  3600,
];
const MIN_MAJOR_PX = 72;
const MIN_MINOR_PX = 7;

function chooseTickStep(pxPerSec: number): number {
  for (const step of TICK_STEPS) {
    if (step * pxPerSec >= MIN_MAJOR_PX) return step;
  }
  return TICK_STEPS[TICK_STEPS.length - 1];
}

/** Format a ruler label: h:mm:ss, m:ss, or m:ss.f at sub-second zoom. */
function formatTick(t: number, step: number): string {
  const totalSec = Math.max(0, t);
  const h = Math.floor(totalSec / 3600);
  const m = Math.floor((totalSec % 3600) / 60);
  const s = totalSec % 60;
  const pad = (n: number) => String(n).padStart(2, "0");
  if (step < 1) {
    const decimals = step < 0.1 ? 2 : 1;
    const secStr = s.toFixed(decimals).padStart(3 + decimals, "0");
    return h > 0 ? `${h}:${pad(m)}:${secStr}` : `${m}:${secStr}`;
  }
  const sInt = Math.round(s);
  return h > 0 ? `${h}:${pad(m)}:${pad(sInt)}` : `${m}:${pad(sInt)}`;
}

/**
 * Clips intersecting [startSec, endSec). Track clips are sorted and
 * non-overlapping (engine invariant), so both edges are monotonic and a
 * binary search finds the first visible clip.
 */
function visibleClips(clips: Clip[], startSec: number, endSec: number): Clip[] {
  let lo = 0;
  let hi = clips.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (clips[mid].timelineOut > startSec) hi = mid;
    else lo = mid + 1;
  }
  const out: Clip[] = [];
  for (let i = lo; i < clips.length && clips[i].timelineIn < endSec; i++) {
    out.push(clips[i]);
  }
  return out;
}

function roundRectPath(
  ctx: CanvasRenderingContext2D,
  x: number,
  y: number,
  w: number,
  h: number,
  r: number,
): void {
  const radius = Math.min(r, w / 2, h / 2);
  ctx.beginPath();
  ctx.moveTo(x + radius, y);
  ctx.arcTo(x + w, y, x + w, y + h, radius);
  ctx.arcTo(x + w, y + h, x, y + h, radius);
  ctx.arcTo(x, y + h, x, y, radius);
  ctx.arcTo(x, y, x + w, y, radius);
  ctx.closePath();
}

function drawRuler(ctx: CanvasRenderingContext2D, width: number): void {
  ctx.fillStyle = COLORS.ruler;
  ctx.fillRect(0, 0, width, RULER_H);

  const major = chooseTickStep(view.pxPerSec);
  const minor = major / 5;
  const drawMinor = durationToPx(minor) >= MIN_MINOR_PX;
  const startSec = Math.max(0, xToTime(0));
  const endSec = xToTime(width);

  ctx.font = "10px system-ui, sans-serif";
  ctx.textBaseline = "top";
  ctx.textAlign = "left";

  const firstMajor = Math.floor(startSec / major) * major;
  for (let t = firstMajor; t <= endSec; t += major) {
    if (t < 0) continue;
    const x = Math.round(timeToX(t)) + 0.5;
    ctx.strokeStyle = COLORS.rulerTick;
    ctx.beginPath();
    ctx.moveTo(x, RULER_H - 9);
    ctx.lineTo(x, RULER_H);
    ctx.stroke();
    ctx.fillStyle = COLORS.rulerLabel;
    ctx.fillText(formatTick(t, major), x + 4, 4);

    if (drawMinor) {
      ctx.strokeStyle = COLORS.rulerMinorTick;
      for (let i = 1; i < 5; i++) {
        const mx = Math.round(timeToX(t + i * minor)) + 0.5;
        if (mx < 0 || mx > width) continue;
        ctx.beginPath();
        ctx.moveTo(mx, RULER_H - 5);
        ctx.lineTo(mx, RULER_H);
        ctx.stroke();
      }
    }
  }
}

/** Lane y in canvas space (content position minus vertical scroll). */
function laneCanvasY(tracks: readonly Track[], index: number): number {
  return RULER_H + laneTop(tracks, index) - view.scrollYPx;
}

function drawLanes(
  ctx: CanvasRenderingContext2D,
  width: number,
  tracks: readonly Track[],
): void {
  tracks.forEach((track, i) => {
    const y = laneCanvasY(tracks, i);
    const h = laneHeight(track.kind);
    if (i % 2 === 0) {
      ctx.fillStyle = COLORS.laneEven;
      ctx.fillRect(0, y, width, h);
    }
    ctx.strokeStyle = COLORS.laneSeparator;
    ctx.beginPath();
    ctx.moveTo(0, y + h - 0.5);
    ctx.lineTo(width, y + h - 0.5);
    ctx.stroke();
  });
}

/** Diagonal hatch across a locked lane — visible "no edits" texture. */
function drawLockHatch(
  ctx: CanvasRenderingContext2D,
  width: number,
  y: number,
  h: number,
): void {
  ctx.save();
  ctx.beginPath();
  ctx.rect(0, y, width, h);
  ctx.clip();
  ctx.strokeStyle = COLORS.laneLockHatch;
  ctx.lineWidth = 1;
  const step = 8;
  for (let x = -h; x < width + h; x += step) {
    ctx.beginPath();
    ctx.moveTo(x, y + h);
    ctx.lineTo(x + h, y);
    ctx.stroke();
  }
  ctx.restore();
}

function drawInvalidLane(
  ctx: CanvasRenderingContext2D,
  width: number,
  y: number,
  h: number,
): void {
  ctx.fillStyle = COLORS.laneInvalidFill;
  ctx.fillRect(0, y, width, h);
  ctx.strokeStyle = COLORS.laneInvalidBorder;
  ctx.setLineDash([6, 4]);
  ctx.strokeRect(0.5, y + 1.5, width - 1, h - 3);
  ctx.setLineDash([]);
}

// Decoded thumbnail images by blob URL. Entries are tiny (320px JPEGs);
// the cache lives for the session.
const thumbCache = new Map<string, HTMLImageElement | null>();

/** The decoded thumbnail for a blob URL, kicking off a load on first use. */
function thumbImage(url: string): HTMLImageElement | null {
  const cached = thumbCache.get(url);
  if (cached !== undefined) return cached;
  thumbCache.set(url, null);
  const img = new Image();
  img.onload = () => {
    thumbCache.set(url, img);
    requestDraw();
  };
  img.src = url;
  return null;
}

/** media id → thumbnail blob URL for everything the pool has thumbnails for. */
function mediaThumbMap(): Map<number, string> {
  const map = new Map<number, string>();
  for (const item of useMediaStore.getState().items) {
    if (item.mediaId !== null && item.thumbnailUrl) {
      map.set(item.mediaId, item.thumbnailUrl);
    }
  }
  return map;
}

function drawClip(
  ctx: CanvasRenderingContext2D,
  clip: Clip,
  track: Track,
  laneY: number,
  laneH: number,
  selected: boolean,
  missing: boolean,
  mediaNames: Map<number, string>,
  thumbs: Map<number, string>,
): void {
  const x = timeToX(clip.timelineIn);
  const w = Math.max(durationToPx(clip.timelineOut - clip.timelineIn), 2);
  const y = laneY + 4;
  const h = laneH - 9;

  const kind = track.kind;
  ctx.fillStyle = missing
    ? COLORS.missingFill
    : selected
      ? COLORS.selectedFill[kind]
      : kind === "video"
        ? COLORS.videoFill
        : kind === "text"
          ? COLORS.textFill
          : COLORS.audioFill;
  roundRectPath(ctx, x, y, w, h, 5);
  ctx.fill();

  // Clip visuals v1: one representative thumbnail at the clip's left edge
  // (full filmstrips are Phase 2). Missing media shows red, no stale frame.
  let labelIndent = 0;
  const thumbUrl =
    missing || clip.mediaId === undefined ? undefined : thumbs.get(clip.mediaId);
  if (thumbUrl && w >= 40 && kind === "video") {
    const img = thumbImage(thumbUrl);
    if (img) {
      const thumbW = Math.min((h / img.naturalHeight) * img.naturalWidth, w - 4);
      ctx.save();
      roundRectPath(ctx, x, y, w, h, 5);
      ctx.clip();
      ctx.drawImage(img, x + 1, y + 1, thumbW, h - 2);
      ctx.restore();
      labelIndent = thumbW;
    }
  }

  ctx.lineWidth = selected ? 2 : 1;
  ctx.strokeStyle = selected
    ? COLORS.selectedBorder
    : missing
      ? COLORS.missingBorder
      : kind === "video"
        ? COLORS.videoBorder
        : kind === "text"
          ? COLORS.textBorder
          : COLORS.audioBorder;
  roundRectPath(ctx, x, y, w, h, 5);
  ctx.stroke();
  ctx.lineWidth = 1;

  if (w >= 28 + labelIndent) {
    // Text clips label with their content's first line, "T"-prefixed;
    // media clips with their file name.
    const label =
      kind === "text"
        ? `T  ${(clip.text?.content ?? "").split("\n")[0] || "(empty)"}`
        : (() => {
            const name =
              (clip.mediaId !== undefined
                ? mediaNames.get(clip.mediaId)
                : undefined) ?? `clip ${clip.id}`;
            return missing ? `⚠ ${name}` : name;
          })();
    ctx.save();
    roundRectPath(ctx, x + 2, y, w - 4, h, 5);
    ctx.clip();
    ctx.fillStyle = COLORS.clipLabel;
    ctx.font = kind === "text" ? "bold 10px system-ui, sans-serif" : "11px system-ui, sans-serif";
    ctx.textBaseline = "top";
    ctx.textAlign = "left";
    ctx.fillText(label, x + labelIndent + 7, kind === "text" ? y + 4 : y + 5);
    ctx.restore();
  }

  // Trim-handle grips on selected clips wide enough to grab.
  if (selected && w >= 22) {
    ctx.fillStyle = COLORS.grip;
    ctx.fillRect(x + 2.5, y + h / 2 - 7, 2, 14);
    ctx.fillRect(x + w - 4.5, y + h / 2 - 7, 2, 14);
  }
}

/** On-canvas geometry of a transition chip. Shared with the controller's
 * hit testing so pixels and pointer math never drift apart. */
export function chipRect(
  span: TransitionSpan,
  tracks: readonly Track[],
): { x: number; y: number; w: number; h: number } | null {
  const trackIndex = tracks.findIndex((t) => t.id === span.trackId);
  if (trackIndex < 0) return null;
  const w = Math.max(durationToPx(span.end - span.start), CHIP_MIN_W);
  const x = timeToX(span.cut) - w / 2;
  const laneY = laneCanvasY(tracks, trackIndex);
  const laneH = laneHeight(tracks[trackIndex].kind);
  return { x, y: laneY + (laneH - CHIP_H) / 2, w, h: CHIP_H };
}

/** The transition chip: a rounded pill straddling the cut with a bowtie
 * glyph; selected chips get the amber border + duration grips. */
function drawTransitionChip(
  ctx: CanvasRenderingContext2D,
  span: TransitionSpan,
  tracks: readonly Track[],
  selected: boolean,
): void {
  const rect = chipRect(span, tracks);
  if (!rect) return;
  const { x, y, w, h } = rect;

  ctx.fillStyle = selected
    ? COLORS.transitionSelectedFill
    : COLORS.transitionFill;
  roundRectPath(ctx, x, y, w, h, h / 2);
  ctx.fill();
  ctx.lineWidth = selected ? 2 : 1;
  ctx.strokeStyle = selected ? COLORS.selectedBorder : COLORS.transitionBorder;
  roundRectPath(ctx, x, y, w, h, h / 2);
  ctx.stroke();
  ctx.lineWidth = 1;

  // Bowtie glyph (◁▷ meeting at the cut).
  const cx = x + w / 2;
  const cy = y + h / 2;
  const gw = Math.min(8, w / 2 - 3);
  if (gw >= 4) {
    ctx.fillStyle = COLORS.transitionIcon;
    ctx.beginPath();
    ctx.moveTo(cx - gw, cy - 4);
    ctx.lineTo(cx - 1, cy);
    ctx.lineTo(cx - gw, cy + 4);
    ctx.closePath();
    ctx.fill();
    ctx.beginPath();
    ctx.moveTo(cx + gw, cy - 4);
    ctx.lineTo(cx + 1, cy);
    ctx.lineTo(cx + gw, cy + 4);
    ctx.closePath();
    ctx.fill();
  }

  // Duration grips on the selected chip.
  if (selected && w >= 30) {
    ctx.fillStyle = COLORS.grip;
    ctx.fillRect(x + 2.5, cy - 4, 2, 8);
    ctx.fillRect(x + w - 4.5, cy - 4, 2, 8);
  }
}

/** Cut-point markers while a transition drags over a lane: every cut
 * gets a tick; the drop target gets the bright diamond. */
function drawCutHighlights(
  ctx: CanvasRenderingContext2D,
  overlay: TransitionDragOverlay,
  tracks: readonly Track[],
): void {
  if (overlay.trackIndex >= tracks.length) return;
  const laneY = laneCanvasY(tracks, overlay.trackIndex);
  const laneH = laneHeight(tracks[overlay.trackIndex].kind);
  for (const cut of overlay.cuts) {
    const x = Math.round(timeToX(cut)) + 0.5;
    const active = cut === overlay.activeCut;
    ctx.strokeStyle = active ? COLORS.cutMarkActive : COLORS.cutMark;
    ctx.lineWidth = active ? 2 : 1;
    ctx.beginPath();
    ctx.moveTo(x, laneY + 3);
    ctx.lineTo(x, laneY + laneH - 4);
    ctx.stroke();
    if (active) {
      const cy = laneY + laneH / 2;
      ctx.fillStyle = COLORS.cutMarkActive;
      ctx.beginPath();
      ctx.moveTo(x, cy - 6);
      ctx.lineTo(x + 5, cy);
      ctx.lineTo(x, cy + 6);
      ctx.lineTo(x - 5, cy);
      ctx.closePath();
      ctx.fill();
    }
  }
  ctx.lineWidth = 1;
}

function drawDropPreview(
  ctx: CanvasRenderingContext2D,
  preview: DropPreview,
  tracks: readonly Track[],
): void {
  if (preview.trackIndex >= tracks.length) return;
  const x = timeToX(preview.inSec);
  const w = Math.max(durationToPx(preview.durSec), 2);
  const y = laneCanvasY(tracks, preview.trackIndex) + 4;
  const h = laneHeight(tracks[preview.trackIndex].kind) - 9;
  ctx.fillStyle = COLORS.dropFill;
  roundRectPath(ctx, x, y, w, h, 5);
  ctx.fill();
  ctx.strokeStyle = COLORS.dropBorder;
  ctx.setLineDash([5, 3]);
  ctx.stroke();
  ctx.setLineDash([]);
}

function drawPlayhead(
  ctx: CanvasRenderingContext2D,
  playheadSec: number,
  width: number,
  height: number,
): void {
  const x = Math.round(timeToX(playheadSec)) + 0.5;
  if (x < -8 || x > width + 8) return;
  ctx.strokeStyle = COLORS.playhead;
  ctx.beginPath();
  ctx.moveTo(x, 0);
  ctx.lineTo(x, height);
  ctx.stroke();
  ctx.fillStyle = COLORS.playhead;
  ctx.beginPath();
  ctx.moveTo(x - 5, RULER_H - 8);
  ctx.lineTo(x + 5, RULER_H - 8);
  ctx.lineTo(x, RULER_H);
  ctx.closePath();
  ctx.fill();
}

function drawSnapLine(
  ctx: CanvasRenderingContext2D,
  snapSec: number,
  height: number,
): void {
  const x = Math.round(timeToX(snapSec)) + 0.5;
  ctx.strokeStyle = COLORS.snapLine;
  ctx.setLineDash([4, 3]);
  ctx.beginPath();
  ctx.moveTo(x, 0);
  ctx.lineTo(x, height);
  ctx.stroke();
  ctx.setLineDash([]);
}

function mediaNameMap(project: Project): Map<number, string> {
  const names = new Map<number, string>();
  for (const media of project.media) {
    const base = media.path.split("/").pop() ?? media.path;
    names.set(media.id, base);
  }
  return names;
}

/** Paint one full frame. `width`/`height` are CSS pixels; the controller
 * has already applied the devicePixelRatio transform. */
export function drawTimeline(
  ctx: CanvasRenderingContext2D,
  width: number,
  height: number,
  overlay: TimelineOverlay,
): void {
  const { project, selection, playheadSec, transitions, selectedTransition } =
    useProjectStore.getState();

  ctx.fillStyle = COLORS.background;
  ctx.fillRect(0, 0, width, height);

  const tracks = project?.tracks ?? [];

  // Everything lane-space (lanes, clips, previews) clips against the
  // area below the ruler so vertical scroll never paints over it.
  ctx.save();
  ctx.beginPath();
  ctx.rect(0, RULER_H, width, Math.max(0, height - RULER_H));
  ctx.clip();

  drawLanes(ctx, width, tracks);

  if (project) {
    const selected = new Set(selection);
    const names = mediaNameMap(project);
    const thumbs = mediaThumbMap();
    const missingIds = useMediaStore.getState().missingMediaIds;
    const startSec = xToTime(-2);
    const endSec = xToTime(width + 2);
    let hasClips = false;
    project.tracks.forEach((track, i) => {
      const laneY = laneCanvasY(tracks, i);
      const laneH = laneHeight(track.kind);
      hasClips = hasClips || track.clips.length > 0;
      if (laneY + laneH < RULER_H || laneY > height) return; // scrolled out

      const dim = track.hidden
        ? HIDDEN_TRACK_ALPHA
        : track.locked
          ? LOCKED_TRACK_ALPHA
          : 1;
      if (dim !== 1) ctx.globalAlpha = dim;
      for (const clip of visibleClips(track.clips, startSec, endSec)) {
        drawClip(
          ctx,
          clip,
          track,
          laneY,
          laneH,
          selected.has(clip.id),
          clip.mediaId !== undefined && missingIds.has(clip.mediaId),
          names,
          thumbs,
        );
      }
      ctx.globalAlpha = 1;
      if (track.locked) drawLockHatch(ctx, width, laneY, laneH);
    });

    // Transition chips straddle their cuts, above the clips.
    for (const span of transitions) {
      const trackIndex = tracks.findIndex((t) => t.id === span.trackId);
      if (trackIndex < 0) continue;
      const laneY = laneCanvasY(tracks, trackIndex);
      const laneH = laneHeight(tracks[trackIndex].kind);
      if (laneY + laneH < RULER_H || laneY > height) continue;
      const x = timeToX(span.cut);
      if (x < -60 || x > width + 60) continue;
      const dim = tracks[trackIndex].hidden
        ? HIDDEN_TRACK_ALPHA
        : tracks[trackIndex].locked
          ? LOCKED_TRACK_ALPHA
          : 1;
      if (dim !== 1) ctx.globalAlpha = dim;
      drawTransitionChip(ctx, span, tracks, span.fromClipId === selectedTransition);
      ctx.globalAlpha = 1;
    }

    if (overlay.transitionDrag) {
      drawCutHighlights(ctx, overlay.transitionDrag, tracks);
    }

    if (!hasClips && !overlay.dropPreview) {
      ctx.fillStyle = COLORS.emptyHint;
      ctx.font = "12px system-ui, sans-serif";
      ctx.textAlign = "center";
      ctx.textBaseline = "middle";
      ctx.fillText(
        "Timeline is empty — drag media here from the pool",
        width / 2,
        RULER_H + (height - RULER_H) / 2,
      );
      ctx.textAlign = "left";
    }
  }

  if (overlay.invalidLaneIndex !== null && overlay.invalidLaneIndex < tracks.length) {
    drawInvalidLane(
      ctx,
      width,
      laneCanvasY(tracks, overlay.invalidLaneIndex),
      laneHeight(tracks[overlay.invalidLaneIndex].kind),
    );
  }
  if (overlay.dropPreview) {
    drawDropPreview(ctx, overlay.dropPreview, tracks);
  }
  ctx.restore();

  drawRuler(ctx, width);
  if (overlay.snapLineSec !== null) {
    drawSnapLine(ctx, overlay.snapLineSec, height);
  }
  drawPlayhead(ctx, playheadSec, width, height);
}
