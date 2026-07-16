// Filmstrip sprites for timeline clips: fetch/parse the engine's packed
// strip files, decode the JPEG sprite once, and draw tiled thumbnails
// into clip bodies. Cached by media *path* (the pool identity — media
// ids restart across projects).
//
// The render path NEVER decodes or waits: `filmstripFor` returns null
// until the async fetch + image decode land (the clip keeps its flat
// placeholder fill, or the legacy single thumbnail), then requests a
// redraw — the same contract as thumbnails and peaks.

import { mediaFilmstrip } from "../lib/ipc";
import { requestDraw } from "./dirty";

export interface Filmstrip {
  /** Tile cell size in sprite pixels. */
  tileW: number;
  tileH: number;
  /** Number of tiles in the sprite. */
  count: number;
  /** Fixed sampling interval, seconds. */
  interval: number;
  /** The decoded sprite (count·tileW × tileH). */
  img: HTMLImageElement;
  /** Sprite pre-scaled to a draw height (rebuilt when lane height
   * changes): per-frame tile blits are then integer-aligned 1:1 copies —
   * cairo's fast path — instead of per-tile bilinear resamples. */
  scaled?: { h: number; tileW: number; canvas: HTMLCanvasElement };
}

/** The sprite pre-scaled so tiles are `drawH` px tall. */
function scaledSprite(
  strip: Filmstrip,
  drawH: number,
): { tileW: number; canvas: HTMLCanvasElement } | null {
  if (strip.scaled && strip.scaled.h === drawH) return strip.scaled;
  const tileW = Math.max(Math.round((drawH / strip.tileH) * strip.tileW), 8);
  const canvas = document.createElement("canvas");
  canvas.width = tileW * strip.count;
  canvas.height = drawH;
  const c = canvas.getContext("2d");
  if (!c) return null;
  c.imageSmoothingEnabled = true;
  c.drawImage(strip.img, 0, 0, canvas.width, canvas.height);
  strip.scaled = { h: drawH, tileW, canvas };
  return strip.scaled;
}

type CacheEntry = Filmstrip | "pending" | "failed";
const cache = new Map<string, CacheEntry>();
// Object URLs owned by the cache (revoked on forget).
const urls = new Map<string, string>();

function parseHeader(
  buf: ArrayBuffer,
): { tileW: number; tileH: number; count: number; interval: number; jpegOffset: number } | null {
  const bytes = new Uint8Array(buf);
  if (bytes.length < 24) return null;
  // "CFLM"
  if (bytes[0] !== 0x43 || bytes[1] !== 0x46 || bytes[2] !== 0x4c || bytes[3] !== 0x4d) {
    return null;
  }
  const view = new DataView(buf);
  if (view.getUint32(4, true) !== 1) return null;
  const tileW = view.getUint32(8, true);
  const tileH = view.getUint32(12, true);
  const count = view.getUint32(16, true);
  const interval = view.getUint32(20, true) / 1000;
  if (tileW === 0 || tileH === 0 || count === 0 || interval <= 0) return null;
  return { tileW, tileH, count, interval, jpegOffset: 24 };
}

/** The decoded filmstrip for a media path, or null while loading/failed.
 * First call kicks off fetch + decode; arrival triggers a redraw. */
export function filmstripFor(path: string, durationSec: number | undefined): Filmstrip | null {
  const hit = cache.get(path);
  if (hit === undefined) {
    cache.set(path, "pending");
    mediaFilmstrip(path, durationSec)
      .then((buf) => {
        const header = parseHeader(buf);
        if (!header) {
          cache.set(path, "failed");
          return;
        }
        const blob = new Blob([new Uint8Array(buf, header.jpegOffset)], {
          type: "image/jpeg",
        });
        const url = URL.createObjectURL(blob);
        urls.set(path, url);
        const img = new Image();
        img.onload = () => {
          cache.set(path, {
            tileW: header.tileW,
            tileH: header.tileH,
            count: header.count,
            interval: header.interval,
            img,
          });
          requestDraw();
        };
        img.onerror = () => {
          cache.set(path, "failed");
        };
        img.src = url;
      })
      .catch(() => {
        cache.set(path, "failed");
      });
    return null;
  }
  return typeof hit === "object" ? hit : null;
}

/** Drop a cached strip (media removed / replaced on disk). */
export function forgetFilmstrip(path: string): void {
  cache.delete(path);
  const url = urls.get(path);
  if (url) {
    URL.revokeObjectURL(url);
    urls.delete(path);
  }
}

/**
 * Draw tiled filmstrip thumbnails across a clip body.
 *
 * Tiles are laid out at the body height (aspect preserved) and each slot
 * shows the sprite tile nearest to its **source time** — repeating tiles
 * when zoomed past the strip interval, skipping tiles when zoomed out.
 * `loopSec` folds source time modulo the media duration (GIF loops).
 * Slots outside `[visX0, visX1)` (canvas pixels) are skipped, so a long
 * clip zoomed far in costs only its visible columns.
 *
 * The caller has already set the clip's rounded-rect clip path.
 */
export function drawFilmstrip(
  ctx: CanvasRenderingContext2D,
  strip: Filmstrip,
  x: number,
  y: number,
  w: number,
  h: number,
  srcStart: number,
  srcEnd: number,
  loopSec: number | null,
  visX0: number,
  visX1: number,
): void {
  if (w < 8 || h < 10 || srcEnd <= srcStart) return;
  const drawH = Math.round(h);
  const scaled = scaledSprite(strip, drawH);
  if (!scaled) return;
  const drawW = scaled.tileW;
  const secPerPx = (srcEnd - srcStart) / w;
  const yi = Math.round(y);
  // Blits are self-clamped to the body (no cairo clip region): partial
  // first/last slots copy a sub-rect of their tile.
  const bx0 = Math.round(Math.max(x, visX0));
  const bx1 = Math.round(Math.min(x + w, visX1));
  if (bx1 <= bx0) return;

  const first = Math.max(0, Math.floor((bx0 - x) / drawW));
  const last = Math.min(Math.ceil(w / drawW), Math.ceil((bx1 - x) / drawW));
  for (let slot = first; slot < last; slot++) {
    const dx = Math.round(x + slot * drawW);
    const bx = Math.max(dx, bx0);
    const bw = Math.min(dx + drawW, bx1) - bx;
    if (bw <= 0) continue;
    // Source time at the slot's center picks the representative tile.
    let t = srcStart + (slot + 0.5) * drawW * secPerPx;
    if (loopSec !== null && loopSec > 0) {
      t = ((t % loopSec) + loopSec) % loopSec;
    }
    const idx = Math.min(
      Math.max(Math.round(t / strip.interval - 0.5), 0),
      strip.count - 1,
    );
    ctx.drawImage(
      scaled.canvas,
      idx * drawW + (bx - dx),
      0,
      bw,
      drawH,
      bx,
      yi,
      bw,
      drawH,
    );
  }
}

/** Pre-scaled still tiles, keyed by source image and draw height. */
const stillScaled = new WeakMap<
  HTMLImageElement,
  { h: number; canvas: HTMLCanvasElement }
>();

/**
 * Tile a single still image across a clip body (image clips have one
 * frame — their "filmstrip" is the media-pool thumbnail repeated).
 * Pre-scaled once per lane height, like filmstrip sprites.
 */
export function drawStillTiles(
  ctx: CanvasRenderingContext2D,
  img: HTMLImageElement,
  x: number,
  y: number,
  w: number,
  h: number,
  visX0: number,
  visX1: number,
): void {
  if (w < 8 || h < 10 || img.naturalHeight === 0) return;
  const drawH = Math.round(h);
  let scaled = stillScaled.get(img);
  if (!scaled || scaled.h !== drawH) {
    const canvas = document.createElement("canvas");
    canvas.width = Math.max(
      Math.round((drawH / img.naturalHeight) * img.naturalWidth),
      8,
    );
    canvas.height = drawH;
    const c = canvas.getContext("2d");
    if (!c) return;
    c.drawImage(img, 0, 0, canvas.width, canvas.height);
    scaled = { h: drawH, canvas };
    stillScaled.set(img, scaled);
  }
  const drawW = scaled.canvas.width;
  const yi = Math.round(y);
  const bx0 = Math.round(Math.max(x, visX0));
  const bx1 = Math.round(Math.min(x + w, visX1));
  if (bx1 <= bx0) return;
  const first = Math.max(0, Math.floor((bx0 - x) / drawW));
  const last = Math.min(Math.ceil(w / drawW), Math.ceil((bx1 - x) / drawW));
  for (let slot = first; slot < last; slot++) {
    const dx = Math.round(x + slot * drawW);
    const bx = Math.max(dx, bx0);
    const bw = Math.min(dx + drawW, bx1) - bx;
    if (bw <= 0) continue;
    ctx.drawImage(scaled.canvas, bx - dx, 0, bw, drawH, bx, yi, bw, drawH);
  }
}
