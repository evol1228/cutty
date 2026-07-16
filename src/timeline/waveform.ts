// Waveform data for timeline clips: fetch/parse the engine's packed peak
// files, hold decoded arrays in a module cache keyed by media *path*
// (paths are the pool identity; media ids restart across projects), and
// aggregate to pixels via power-of-two mip levels.
//
// The render path NEVER decodes or waits: `peaksFor` returns null until
// the async fetch lands (the clip draws its flat placeholder fill), then
// requests a redraw — the same contract the thumbnail cache uses.

import { mediaPeaks } from "../lib/ipc";
import { requestDraw } from "./dirty";

export interface Peaks {
  /** Windows per second (base level). */
  perSec: number;
  /** Mip chain: level 0 = base pairs, each next level halves the window
   * count. Interleaved [min, max] i8 pairs. */
  mips: Int8Array[];
  /** Window counts per level. */
  counts: number[];
}

/** Parse the `CPKS` binary layout (see cutty_media::peaks). */
export function parsePeaks(buf: ArrayBuffer): Peaks | null {
  const bytes = new Uint8Array(buf);
  if (bytes.length < 16) return null;
  if (bytes[0] !== 0x43 || bytes[1] !== 0x50 || bytes[2] !== 0x4b || bytes[3] !== 0x53) {
    return null; // "CPKS"
  }
  const view = new DataView(buf);
  const version = view.getUint32(4, true);
  if (version !== 1) return null;
  const perSec = view.getUint32(8, true);
  const count = view.getUint32(12, true);
  if (perSec === 0 || bytes.length < 16 + count * 2) return null;
  const base = new Int8Array(buf, 16, count * 2);

  // Mip chain down to ~64 windows: pairwise min/max so any zoom level
  // aggregates at most two windows per drawn column.
  const mips: Int8Array[] = [base];
  const counts: number[] = [count];
  let level = base;
  let n = count;
  while (n > 64) {
    const half = Math.ceil(n / 2);
    const next = new Int8Array(half * 2);
    for (let i = 0; i < half; i++) {
      const a = i * 2;
      const b = Math.min(a + 1, n - 1);
      next[i * 2] = Math.min(level[a * 2], level[b * 2]);
      next[i * 2 + 1] = Math.max(level[a * 2 + 1], level[b * 2 + 1]);
    }
    mips.push(next);
    counts.push(half);
    level = next;
    n = half;
  }
  return { perSec, mips, counts };
}

type CacheEntry = Peaks | "pending" | "failed";
const cache = new Map<string, CacheEntry>();

/** Decoded peaks for a media path, or null while loading/failed. First
 * call kicks off the fetch; arrival triggers a timeline redraw. */
export function peaksFor(path: string): Peaks | null {
  const hit = cache.get(path);
  if (hit === undefined) {
    cache.set(path, "pending");
    mediaPeaks(path)
      .then((buf) => {
        const parsed = parsePeaks(buf);
        cache.set(path, parsed ?? "failed");
        if (parsed) requestDraw();
      })
      .catch(() => {
        cache.set(path, "failed");
      });
    return null;
  }
  return typeof hit === "object" ? hit : null;
}

/** Drop cached peaks (media removed / replaced on disk). */
export function forgetPeaks(path: string): void {
  cache.delete(path);
}

/**
 * Render the min/max waveform columns into `ctx` starting at (0, 0) —
 * the builder for the per-clip cache. Columns step 1 px; the mip chain
 * keeps each column's aggregation ≤ 2 windows.
 */
function renderWaveformColumns(
  ctx: CanvasRenderingContext2D,
  peaks: Peaks,
  w: number,
  h: number,
  srcStart: number,
  srcEnd: number,
  color: string,
): void {
  const mid = h / 2;
  const amp = (h / 2) * (1 / 127);

  const baseWindowsPerCol = ((srcEnd - srcStart) * peaks.perSec) / w;
  let level = 0;
  let windowsPerCol = baseWindowsPerCol;
  while (windowsPerCol > 2 && level + 1 < peaks.mips.length) {
    level++;
    windowsPerCol /= 2;
  }
  const data = peaks.mips[level];
  const count = peaks.counts[level];
  const perSec = peaks.perSec / 2 ** level;

  ctx.beginPath();
  for (let px = 0; px < w; px++) {
    const t0 = srcStart + ((srcEnd - srcStart) * px) / w;
    const t1 = srcStart + ((srcEnd - srcStart) * (px + 1)) / w;
    let i0 = Math.floor(t0 * perSec);
    let i1 = Math.ceil(t1 * perSec);
    if (i1 <= i0) i1 = i0 + 1;
    if (i0 >= count || i1 <= 0) continue; // beyond the media: silence
    i0 = Math.max(0, i0);
    i1 = Math.min(count, i1);
    let lo = 127;
    let hi = -127;
    for (let i = i0; i < i1; i++) {
      const l = data[i * 2];
      const m = data[i * 2 + 1];
      if (l < lo) lo = l;
      if (m > hi) hi = m;
    }
    if (hi < lo) continue;
    // At least a 1px tick so silence still reads as a center line.
    const top = mid - Math.max(hi * amp, 0.5);
    const bottom = mid - Math.min(lo * amp, -0.5);
    ctx.rect(px, top, 1, Math.max(bottom - top, 1));
  }
  ctx.fillStyle = color;
  ctx.fill();
}

interface WaveCacheEntry {
  w: number;
  h: number;
  srcStart: number;
  srcEnd: number;
  color: string;
  canvas: HTMLCanvasElement;
}

// Per-clip rendered waveforms: a clip's waveform image is static for a
// given zoom (width) and source range, so pans blit instead of re-rating
// thousands of columns per frame. Rebuilt on zoom/trim/select changes.
const waveCache = new Map<number, WaveCacheEntry>();
const WAVE_CACHE_MAX = 256;
/** Widest cached waveform; wider clips render direct (rare mega-zoom). */
const WAVE_CACHE_MAX_W = 8192;

/** Drop cached renders (project switched). */
export function clearWaveformRenders(): void {
  waveCache.clear();
}

/**
 * Draw the waveform for the clip body span `[x, x+w)`, restricted to the
 * visible window `[visX0, visX1)` — cached per clip so a pan costs one
 * blit. `srcStart`/`srcEnd` are the clip's full source range.
 */
export function drawWaveform(
  ctx: CanvasRenderingContext2D,
  peaks: Peaks,
  clipId: number,
  x: number,
  y: number,
  w: number,
  h: number,
  srcStart: number,
  srcEnd: number,
  visX0: number,
  visX1: number,
  color: string,
): void {
  const wi = Math.round(w);
  const hi = Math.round(h);
  if (wi < 4 || hi < 6 || srcEnd <= srcStart) return;
  const bx0 = Math.round(Math.max(x, visX0));
  const bx1 = Math.round(Math.min(x + w, visX1));
  if (bx1 <= bx0) return;

  if (wi > WAVE_CACHE_MAX_W) {
    // Too wide to cache: render just the visible span directly.
    ctx.save();
    ctx.translate(bx0, Math.round(y));
    const secPerPx = (srcEnd - srcStart) / w;
    renderWaveformColumns(
      ctx,
      peaks,
      bx1 - bx0,
      hi,
      srcStart + (bx0 - x) * secPerPx,
      srcStart + (bx1 - x) * secPerPx,
      color,
    );
    ctx.restore();
    return;
  }

  let entry = waveCache.get(clipId);
  const stale =
    !entry ||
    entry.w !== wi ||
    entry.h !== hi ||
    entry.srcStart !== srcStart ||
    entry.srcEnd !== srcEnd ||
    entry.color !== color;
  if (stale) {
    const canvas = entry?.canvas ?? document.createElement("canvas");
    canvas.width = wi;
    canvas.height = hi;
    const c = canvas.getContext("2d");
    if (!c) return;
    renderWaveformColumns(c, peaks, wi, hi, srcStart, srcEnd, color);
    entry = { w: wi, h: hi, srcStart, srcEnd, color, canvas };
    waveCache.delete(clipId);
    waveCache.set(clipId, entry);
    if (waveCache.size > WAVE_CACHE_MAX) {
      const oldest = waveCache.keys().next().value;
      if (oldest !== undefined) waveCache.delete(oldest);
    }
  } else {
    // LRU touch.
    waveCache.delete(clipId);
    waveCache.set(clipId, entry!);
  }
  const e = entry!;
  const sx = bx0 - Math.round(x);
  ctx.drawImage(e.canvas, sx, 0, bx1 - bx0, hi, bx0, Math.round(y), bx1 - bx0, hi);
}
