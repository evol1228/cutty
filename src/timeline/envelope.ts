// Presentation-side mirror of the engine's volume-keyframe semantics
// (crates/cutty-engine/src/keyframes.rs) — evaluation for *drawing* the
// rubber-band line, fade detection for handle placement, and the
// value↔pixel mapping shared by renderer and controller hit testing.
// The engine stays the authority: every mutation goes through keyframe
// commands, and the audible envelope is evaluated in Rust.

import type { Clip, Easing, Keyframe } from "../lib/engineIpc";

/** Mirrors the engine's minimum keyframe separation, seconds. */
export const KEYFRAME_MIN_DT = 1e-3;
/** Mirrors the engine's fade-detection silence threshold. */
const FADE_SILENT = 1e-4;
/** Top of the envelope drawing scale: a gain of 2.0 (200%, the volume
 * slider's ceiling) draws at the clip's top edge; unity sits mid-clip. */
export const ENVELOPE_VMAX = 2;

/** The clip's volume lane (sorted, possibly empty). */
export function volumeLane(clip: Clip): Keyframe[] {
  return clip.keyframes?.volume ?? [];
}

function applyEasing(easing: Easing, x: number): number {
  const c = Math.min(1, Math.max(0, x));
  switch (easing) {
    case "linear":
      return c;
    case "easeIn":
      return c * c;
    case "easeOut":
      return c * (2 - c);
    case "easeInOut":
      return c * c * (3 - 2 * c);
  }
}

/** Evaluate a lane at clip-relative time `t` (engine semantics: hold
 * the first/last value outside the range, left keyframe's easing in
 * between). Empty lane = unity gain. */
export function evalLane(lane: readonly Keyframe[], t: number): number {
  if (lane.length === 0) return 1;
  if (t <= lane[0].t) return lane[0].value;
  const last = lane[lane.length - 1];
  if (t >= last.t) return last.value;
  let i = 1;
  while (i < lane.length && lane[i].t <= t) i++;
  const a = lane[i - 1];
  const b = lane[i];
  const span = b.t - a.t;
  if (span <= 0) return b.value;
  return a.value + (b.value - a.value) * applyEasing(a.easing, (t - a.t) / span);
}

/** Detected fade-in duration (lane starts with a silent keyframe at the
 * clip start), or null. Mirrors the engine convention. */
export function fadeInDuration(lane: readonly Keyframe[]): number | null {
  if (lane.length < 2) return null;
  const [first, second] = [lane[0], lane[1]];
  return first.t <= KEYFRAME_MIN_DT && first.value <= FADE_SILENT
    ? second.t
    : null;
}

/** Detected fade-out duration (lane ends with a silent keyframe at the
 * clip end), or null. */
export function fadeOutDuration(
  lane: readonly Keyframe[],
  clipDuration: number,
): number | null {
  if (lane.length < 2) return null;
  const last = lane[lane.length - 1];
  const prev = lane[lane.length - 2];
  return last.t >= clipDuration - KEYFRAME_MIN_DT && last.value <= FADE_SILENT
    ? clipDuration - prev.t
    : null;
}

/** Canvas-space rectangle of a clip's body (the rounded box the
 * renderer paints). */
export interface ClipBodyRect {
  x: number;
  y: number;
  w: number;
  h: number;
}

/** y of an envelope value inside a clip body (0 = bottom, VMAX = top). */
export function envelopeValueToY(rect: ClipBodyRect, value: number): number {
  const frac = Math.min(1, Math.max(0, value / ENVELOPE_VMAX));
  return rect.y + rect.h * (1 - frac);
}

/** Envelope value for a pointer y inside a clip body (clamped 0..VMAX). */
export function envelopeYToValue(rect: ClipBodyRect, y: number): number {
  const frac = 1 - (y - rect.y) / rect.h;
  return Math.min(ENVELOPE_VMAX, Math.max(0, frac * ENVELOPE_VMAX));
}
