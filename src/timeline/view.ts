// Timeline viewport: zoom (pixels per second) and horizontal pan. This is
// the pixel↔time conversion layer — the only "math" the frontend is
// allowed to do on timeline data. Everything in seconds beyond conversion
// (snapping, clamping, overlap) belongs to the engine.

import { useProjectStore } from "../state/projectStore";
import { requestDraw } from "./dirty";

export const MIN_PX_PER_SEC = 2;
export const MAX_PX_PER_SEC = 800;

// Layout shared between the canvas renderer and the React track headers.
export const RULER_H = 26;
export const TRACK_H = 64;

export interface TimelineView {
  /** Zoom: CSS pixels per second. Mirrored into the store for the slider. */
  pxPerSec: number;
  /** Horizontal pan: CSS pixels scrolled from t=0. Never negative. */
  scrollPx: number;
  /** Current canvas width in CSS pixels (kept fresh by the controller). */
  widthPx: number;
}

/** Singleton view state — there is exactly one timeline. */
export const view: TimelineView = {
  pxPerSec: useProjectStore.getState().pxPerSec,
  scrollPx: 0,
  widthPx: 0,
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

/** Pan horizontally by a pixel delta (clamped so t=0 stays leftmost). */
export function panBy(dxPx: number): void {
  const next = Math.max(0, view.scrollPx + dxPx);
  if (next !== view.scrollPx) {
    view.scrollPx = next;
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
