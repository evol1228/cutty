// Timeline viewport: zoom (pixels per second), horizontal pan, vertical
// track scroll, and lane geometry (per-kind lane heights). This is the
// pixel↔time/lane conversion layer — the only "math" the frontend is
// allowed to do on timeline data. Everything in seconds beyond conversion
// (snapping, clamping, overlap) belongs to the engine.

import type { Track, TrackKind } from "../lib/engineIpc";
import { useProjectStore } from "../state/projectStore";
import { requestDraw } from "./dirty";

export const MIN_PX_PER_SEC = 2;
export const MAX_PX_PER_SEC = 800;

// Layout shared between the canvas renderer and the React track headers.
export const RULER_H = 26;
/** Lane heights by track kind — video lanes carry thumbnails, audio
 * lanes are compact (the CapCut proportions). */
export const VIDEO_LANE_H = 56;
export const AUDIO_LANE_H = 36;

export function laneHeight(kind: TrackKind): number {
  return kind === "video" ? VIDEO_LANE_H : AUDIO_LANE_H;
}

/** Top of lane `index` in *content* space (0 = directly below the ruler
 * at zero scroll). */
export function laneTop(tracks: readonly Track[], index: number): number {
  let y = 0;
  for (let i = 0; i < index && i < tracks.length; i++) {
    y += laneHeight(tracks[i].kind);
  }
  return y;
}

/** Total height of all lanes, content space. */
export function lanesHeight(tracks: readonly Track[]): number {
  return laneTop(tracks, tracks.length);
}

/** Lane index at a content-space y, or null outside all lanes. */
export function laneIndexAtY(
  tracks: readonly Track[],
  contentY: number,
): number | null {
  if (contentY < 0) return null;
  let y = 0;
  for (let i = 0; i < tracks.length; i++) {
    y += laneHeight(tracks[i].kind);
    if (contentY < y) return i;
  }
  return null;
}

export interface TimelineView {
  /** Zoom: CSS pixels per second. Mirrored into the store for the slider. */
  pxPerSec: number;
  /** Horizontal pan: CSS pixels scrolled from t=0. Never negative. */
  scrollPx: number;
  /** Vertical track scroll: CSS pixels. Mirrored into the store so the
   * React header column translates in lockstep. */
  scrollYPx: number;
  /** Current canvas width in CSS pixels (kept fresh by the controller). */
  widthPx: number;
  /** Current canvas height in CSS pixels (kept fresh by the controller). */
  heightPx: number;
}

/** Singleton view state — there is exactly one timeline. */
export const view: TimelineView = {
  pxPerSec: useProjectStore.getState().pxPerSec,
  scrollPx: 0,
  scrollYPx: 0,
  widthPx: 0,
  heightPx: 0,
};

export function timeToX(t: number): number {
  return t * view.pxPerSec - view.scrollPx;
}

export function xToTime(x: number): number {
  return (x + view.scrollPx) / view.pxPerSec;
}

export function durationToPx(seconds: number): number {
  return seconds * view.pxPerSec;
}

export function pxToDuration(px: number): number {
  return px / view.pxPerSec;
}

/** Canvas y → content-space y (below-ruler coordinates). */
export function canvasYToContentY(canvasY: number): number {
  return canvasY - RULER_H + view.scrollYPx;
}

/** Pan horizontally by a pixel delta (clamped so t=0 stays leftmost). */
export function panBy(dxPx: number): void {
  const next = Math.max(0, view.scrollPx + dxPx);
  if (next !== view.scrollPx) {
    view.scrollPx = next;
    requestDraw();
  }
}

/** Scroll the track lanes vertically, clamped to the content height. */
export function scrollTracksBy(dyPx: number): void {
  setTrackScroll(view.scrollYPx + dyPx);
}

export function setTrackScroll(px: number): void {
  const tracks = useProjectStore.getState().project?.tracks ?? [];
  const viewport = Math.max(0, view.heightPx - RULER_H);
  const max = Math.max(0, lanesHeight(tracks) - viewport);
  const next = Math.min(max, Math.max(0, px));
  if (next !== view.scrollYPx) {
    view.scrollYPx = next;
    useProjectStore.getState().setTrackScrollPx(next);
    requestDraw();
  }
}

/**
 * Set zoom, keeping the time under `anchorX` (CSS px, canvas-relative)
 * stationary on screen. Defaults to the viewport center.
 */
export function setZoom(pxPerSec: number, anchorX?: number): void {
  const next = Math.min(MAX_PX_PER_SEC, Math.max(MIN_PX_PER_SEC, pxPerSec));
  if (next === view.pxPerSec) return;
  const anchor = anchorX ?? view.widthPx / 2;
  const anchorTime = xToTime(anchor);
  view.pxPerSec = next;
  view.scrollPx = Math.max(0, anchorTime * next - anchor);
  useProjectStore.getState().setPxPerSec(next);
  requestDraw();
}

/** Multiply zoom by a factor (wheel / keyboard zoom). */
export function zoomBy(factor: number, anchorX?: number): void {
  setZoom(view.pxPerSec * factor, anchorX);
}

/** Scroll the minimum amount needed to bring time `t` into view. */
export function ensureVisible(t: number, marginPx = 24): void {
  const x = timeToX(t);
  if (x < marginPx) {
    view.scrollPx = Math.max(0, t * view.pxPerSec - marginPx);
    requestDraw();
  } else if (x > view.widthPx - marginPx) {
    view.scrollPx = Math.max(0, t * view.pxPerSec - view.widthPx + marginPx);
    requestDraw();
  }
}
