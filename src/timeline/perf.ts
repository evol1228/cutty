// Timeline draw-performance instrumentation (dev tool). The controller
// records every draw's wall time; the renderer paints a small HUD when
// enabled. The timeline redraws on demand (dirty scheduler), so "fps"
// here means: while redraws are continuous (playback, drags, pans), how
// fast consecutive frames actually come — gaps from idle periods are
// excluded rather than averaged in.

const WINDOW = 180;

const drawMs: number[] = [];
const frameGaps: number[] = [];
let lastDrawStart = 0;

let enabled =
  typeof localStorage !== "undefined" && localStorage.getItem("cutty-hud") === "1";

export function hudEnabled(): boolean {
  return enabled;
}

export function toggleHud(): void {
  enabled = !enabled;
  try {
    localStorage.setItem("cutty-hud", enabled ? "1" : "0");
  } catch {
    // Private mode etc. — the toggle still works for the session.
  }
}

/** Session-only enable (the bench uses this: no localStorage residue). */
export function setHudForSession(value: boolean): void {
  enabled = value;
}

/** Record one draw: its start timestamp and how long it took. */
export function recordDraw(startTs: number, durationMs: number): void {
  drawMs.push(durationMs);
  if (drawMs.length > WINDOW) drawMs.shift();
  if (lastDrawStart > 0) {
    const gap = startTs - lastDrawStart;
    // Only consecutive frames count toward fps (a 60 Hz stream has
    // ~16.7 ms gaps; anything over ~90 ms is an idle pause, not a slow
    // frame — the dirty scheduler simply had nothing to draw).
    if (gap > 0 && gap < 90) {
      frameGaps.push(gap);
      if (frameGaps.length > WINDOW) frameGaps.shift();
    }
  }
  lastDrawStart = startTs;
}

export interface DrawStats {
  avgMs: number;
  p95Ms: number;
  worstMs: number;
  fps: number | null;
  samples: number;
}

/** Clear the rolling windows (bench warm-up → measurement boundary). */
export function resetStats(): void {
  drawMs.length = 0;
  frameGaps.length = 0;
  lastDrawStart = 0;
}

export function drawStats(): DrawStats {
  if (drawMs.length === 0) {
    return { avgMs: 0, p95Ms: 0, worstMs: 0, fps: null, samples: 0 };
  }
  const sorted = [...drawMs].sort((a, b) => a - b);
  const avg = drawMs.reduce((s, v) => s + v, 0) / drawMs.length;
  const p95 = sorted[Math.min(Math.floor(sorted.length * 0.95), sorted.length - 1)];
  const worst = sorted[sorted.length - 1];
  let fps: number | null = null;
  if (frameGaps.length >= 10) {
    const mean = frameGaps.reduce((s, v) => s + v, 0) / frameGaps.length;
    fps = 1000 / mean;
  }
  return { avgMs: avg, p95Ms: p95, worstMs: worst, fps, samples: drawMs.length };
}
