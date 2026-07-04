// Canvas renderer for the timeline. Pure drawing: reads the engine
// snapshot from the store and the viewport from view.ts, paints ruler,
// track lanes, clips, playhead, and snap indicator. No timeline math here
// beyond pixel↔time conversion; clips are drawn exactly where the engine
// says they are.

import type { Clip, Project, Track } from "../lib/engineIpc";
import { useProjectStore } from "../state/projectStore";
import { durationToPx, RULER_H, timeToX, TRACK_H, view, xToTime } from "./view";

/** Transient gesture state drawn on top of engine state. */
export interface TimelineOverlay {
  /** Snap indicator line, seconds; null when not snapping. */
  snapLineSec: number | null;
}

const COLORS = {
  background: "#09090b", // zinc-950
  laneEven: "#0d0d10",
  laneSeparator: "#27272a", // zinc-800
  ruler: "#18181b", // zinc-900
  rulerTick: "#3f3f46", // zinc-700
  rulerMinorTick: "#27272a",
  rulerLabel: "#a1a1aa", // zinc-400
  videoFill: "#0c4a6e", // sky-900
  videoBorder: "#0369a1", // sky-700
  audioFill: "#064e3b", // emerald-900
  audioBorder: "#047857", // emerald-700
  selectedFill: { video: "#075985", audio: "#065f46" },
  selectedBorder: "#f59e0b", // amber-500
  clipLabel: "#e4e4e7", // zinc-200
  grip: "rgba(255,255,255,0.55)",
  playhead: "#ef4444", // red-500
  snapLine: "#fbbf24", // amber-400
  emptyHint: "#52525b", // zinc-600
} as const;

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

function drawLanes(
  ctx: CanvasRenderingContext2D,
  width: number,
  trackCount: number,
): void {
  for (let i = 0; i < trackCount; i++) {
    const y = RULER_H + i * TRACK_H;
    if (i % 2 === 0) {
      ctx.fillStyle = COLORS.laneEven;
      ctx.fillRect(0, y, width, TRACK_H);
    }
    ctx.strokeStyle = COLORS.laneSeparator;
    ctx.beginPath();
    ctx.moveTo(0, y + TRACK_H - 0.5);
    ctx.lineTo(width, y + TRACK_H - 0.5);
    ctx.stroke();
  }
}

function drawClip(
  ctx: CanvasRenderingContext2D,
  clip: Clip,
  track: Track,
  laneY: number,
  selected: boolean,
  mediaNames: Map<number, string>,
): void {
  const x = timeToX(clip.timelineIn);
  const w = Math.max(durationToPx(clip.timelineOut - clip.timelineIn), 2);
  const y = laneY + 4;
  const h = TRACK_H - 9;

  const kind = track.kind;
  ctx.fillStyle = selected
    ? COLORS.selectedFill[kind]
    : kind === "video"
      ? COLORS.videoFill
      : COLORS.audioFill;
  roundRectPath(ctx, x, y, w, h, 5);
  ctx.fill();
  ctx.lineWidth = selected ? 2 : 1;
  ctx.strokeStyle = selected
    ? COLORS.selectedBorder
    : kind === "video"
      ? COLORS.videoBorder
      : COLORS.audioBorder;
  ctx.stroke();
  ctx.lineWidth = 1;

  if (w >= 28) {
    const label = mediaNames.get(clip.mediaId) ?? `clip ${clip.id}`;
    ctx.save();
    roundRectPath(ctx, x + 2, y, w - 4, h, 5);
    ctx.clip();
    ctx.fillStyle = COLORS.clipLabel;
    ctx.font = "11px system-ui, sans-serif";
    ctx.textBaseline = "top";
    ctx.textAlign = "left";
    ctx.fillText(label, x + 7, y + 6);
    ctx.restore();
  }

  // Trim-handle grips on selected clips wide enough to grab.
  if (selected && w >= 22) {
    ctx.fillStyle = COLORS.grip;
    ctx.fillRect(x + 2.5, y + h / 2 - 7, 2, 14);
    ctx.fillRect(x + w - 4.5, y + h / 2 - 7, 2, 14);
  }
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
  const { project, selection, playheadSec } = useProjectStore.getState();

  ctx.fillStyle = COLORS.background;
  ctx.fillRect(0, 0, width, height);

  const trackCount = project?.tracks.length ?? 2;
  drawLanes(ctx, width, trackCount);
  drawRuler(ctx, width);

  if (project) {
    const selected = new Set(selection);
    const names = mediaNameMap(project);
    const startSec = xToTime(-2);
    const endSec = xToTime(width + 2);
    let hasClips = false;
    project.tracks.forEach((track, i) => {
      const laneY = RULER_H + i * TRACK_H;
      for (const clip of visibleClips(track.clips, startSec, endSec)) {
        drawClip(ctx, clip, track, laneY, selected.has(clip.id), names);
      }
      hasClips = hasClips || track.clips.length > 0;
    });

    if (!hasClips) {
      ctx.fillStyle = COLORS.emptyHint;
      ctx.font = "12px system-ui, sans-serif";
      ctx.textAlign = "center";
      ctx.textBaseline = "middle";
      ctx.fillText(
        "Timeline is empty — use Seed 50 to add test clips",
        width / 2,
        RULER_H + (height - RULER_H) / 2,
      );
      ctx.textAlign = "left";
    }
  }

  if (overlay.snapLineSec !== null) {
    drawSnapLine(ctx, overlay.snapLineSec, height);
  }
  drawPlayhead(ctx, playheadSec, width, height);
}
