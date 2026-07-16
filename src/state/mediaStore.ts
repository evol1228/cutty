// Media pool state: the engine's project.media is the source of truth for
// what belongs to the project; this store adds the UI-side job state around
// it (probe/proxy/thumbnail progress, error text, thumbnail blob URLs,
// missing-file flags). Items are keyed by source path.
//
// Import pipeline per file: probe → engine_add_media → [proxy ‖ thumbnail]
// → ready. Everything runs in the background; the pool renders live status.

import { create } from "zustand";
import { listen } from "@tauri-apps/api/event";
import type { MediaInfo, MediaKind, ProxyProgressEvent } from "../lib/ipc";
import {
  generateProxy,
  mediaFilmstrip,
  mediaPeaks,
  mediaThumbnail,
  pathsExist,
  probeMedia,
  PROXY_PROGRESS_EVENT,
} from "../lib/ipc";
import { engineAddMedia } from "../lib/engineIpc";
import type { Project } from "../lib/engineIpc";
import { forgetFilmstrip } from "../timeline/filmstrip";
import { clearWaveformRenders, forgetPeaks } from "../timeline/waveform";
import { toast } from "./toastStore";

export type PoolStatus = "probing" | "processing" | "ready" | "error";

export interface PoolItem {
  /** Absolute source path — the item's identity. */
  path: string;
  /** Basename, for display. */
  name: string;
  /** Engine media id, once registered (null while probing). */
  mediaId: number | null;
  status: PoolStatus;
  /** Error message when status is "error". */
  error: string | null;
  durationSec: number | null;
  hasVideo: boolean;
  hasAudio: boolean;
  /** Timeline semantics ("image"/"gif" media never gets a proxy). */
  kind: MediaKind;
  /** Full probe result (null on rehydrated items until re-probed). */
  info: MediaInfo | null;
  proxyPath: string | null;
  /** Proxy encode progress 0–100, or null when not encoding. */
  proxyProgress: number | null;
  /** Blob URL of the thumbnail JPEG (video media only). */
  thumbnailUrl: string | null;
  /** Source file has vanished from disk. */
  missing: boolean;
}

/** Extensions offered by the import dialog. */
export const VIDEO_EXTENSIONS = ["mp4", "mkv", "mov", "webm", "avi", "m4v"];
export const AUDIO_EXTENSIONS = ["mp3", "wav", "flac", "ogg", "opus", "m4a", "aac"];
export const IMAGE_EXTENSIONS = ["png", "jpg", "jpeg", "webp", "gif", "bmp"];

/** Default timeline length of a dropped still, seconds (mirrors the
 * engine's DEFAULT_STILL_CLIP_DURATION — the engine is authoritative). */
export const DEFAULT_STILL_CLIP_DURATION = 5;

/** The timeline span a fresh clip of this item covers (drag ghosts and
 * drops): stills default to 5s, everything else to its duration. */
export function defaultClipDuration(item: {
  kind: MediaKind;
  durationSec: number | null;
}): number {
  if (item.kind === "image") return DEFAULT_STILL_CLIP_DURATION;
  return item.durationSec ?? 0;
}

function basename(path: string): string {
  return path.split("/").pop() ?? path;
}

/** Media that never gets a proxy: preview decodes the original directly
 * (stills/GIFs are cheap; a proxy transcode would flatten alpha). */
function decodesDirect(kind: MediaKind, hasAlpha: boolean): boolean {
  return kind === "image" || kind === "gif" || hasAlpha;
}

// Cap concurrent proxy encodes — five parallel ffmpeg x264 runs would
// starve the UI-facing decoder of cores.
const MAX_CONCURRENT_PROXIES = 2;
let proxySlots = 0;
const proxyWaiters: Array<() => void> = [];

async function withProxySlot<T>(run: () => Promise<T>): Promise<T> {
  if (proxySlots >= MAX_CONCURRENT_PROXIES) {
    await new Promise<void>((resolve) => proxyWaiters.push(resolve));
  }
  proxySlots++;
  try {
    return await run();
  } finally {
    proxySlots--;
    proxyWaiters.shift()?.();
  }
}

interface MediaState {
  items: PoolItem[];
  /** Engine media ids whose source file is currently missing on disk. */
  missingMediaIds: Set<number>;

  /** Import files (dialog or OS drop). Skips duplicates, rejects images. */
  importFiles: (paths: string[]) => Promise<void>;
  /** Drop an item's local state (blob URL etc.). Engine removal is the
   * caller's job — this runs when the snapshot no longer has the media. */
  forgetItem: (path: string) => void;
  /** Drop items whose import is still in flight (no engine id yet) —
   * called right before the engine swaps projects so a late probe can't
   * register media into the wrong project. */
  dropPendingImports: () => void;
  /** Reconcile pool items against an engine snapshot. */
  syncFromProject: (project: Project) => void;
  /** Re-check every item's source file on disk. */
  checkMissing: () => Promise<void>;
}

function patchItem(
  set: (fn: (state: MediaState) => Partial<MediaState>) => void,
  path: string,
  patch: Partial<PoolItem>,
): void {
  set((state) => ({
    items: state.items.map((i) => (i.path === path ? { ...i, ...patch } : i)),
  }));
}

/** Signature of the engine media list, to skip no-op reconciles. */
let lastSyncSignature = "";

export const useMediaStore = create<MediaState>((set, get) => {
  /** Background jobs after a media is registered: thumbnail always (for
   * anything with a picture), proxy only for plain bounded video —
   * stills/GIFs/alpha media decode originals directly. */
  async function runDerivedJobs(
    path: string,
    durationSec: number,
    hasVideo: boolean,
    kind: MediaKind,
    hasAlpha: boolean,
  ): Promise<void> {
    // Waveform peaks: any media with audio (video's audio included) —
    // best-effort warm-up; the timeline fetches lazily and tolerates
    // absence, so failures only cost the waveform.
    const item = get().items.find((i) => i.path === path);
    if (item?.hasAudio) {
      void mediaPeaks(path).catch(() => undefined);
    }
    if (!hasVideo) {
      // Audio-only media plays from the original via the audio stack; no
      // 720p proxy or thumbnail to make.
      patchItem(set, path, { status: "ready" });
      return;
    }
    patchItem(set, path, { status: "processing" });
    const jobs: Array<Promise<void>> = [
      mediaThumbnail(path, durationSec).then((buf) => {
        const url = URL.createObjectURL(new Blob([buf], { type: "image/jpeg" }));
        if (get().items.some((i) => i.path === path)) {
          patchItem(set, path, { thumbnailUrl: url });
        } else {
          URL.revokeObjectURL(url); // item was removed mid-generation
        }
      }),
    ];
    // Filmstrip strips: every video-lane media except stills (a still's
    // strip is its thumbnail repeated). Warm-up only, like peaks.
    if (kind !== "image") {
      void mediaFilmstrip(path, durationSec).catch(() => undefined);
    }
    if (!decodesDirect(kind, hasAlpha)) {
      jobs.push(
        withProxySlot(() => generateProxy(path, durationSec)).then((proxyPath) => {
          patchItem(set, path, { proxyPath, proxyProgress: null });
        }),
      );
    }
    const results = await Promise.allSettled(jobs);
    const failed = results.find(
      (r): r is PromiseRejectedResult => r.status === "rejected",
    );
    if (get().items.some((i) => i.path === path)) {
      if (failed) {
        patchItem(set, path, {
          status: "error",
          error: String(failed.reason),
          proxyProgress: null,
        });
        toast(`${basename(path)}: ${String(failed.reason)}`, "error");
      } else {
        patchItem(set, path, { status: "ready" });
      }
    }
  }

  /** Full import pipeline for one new file. */
  async function runImport(path: string): Promise<void> {
    try {
      const info = await probeMedia(path);
      if (!info.video && !info.audio) {
        throw new Error("no decodable video or audio streams");
      }
      // The item may have been dropped mid-probe (project switched) —
      // don't register media into a project it wasn't imported for.
      if (!get().items.some((i) => i.path === path)) return;
      const hasAlpha = info.video?.hasAlpha ?? false;
      const mediaId = await engineAddMedia(
        path,
        info.durationSec,
        info.video !== null,
        info.audio !== null,
        hasAlpha,
        info.kind,
      );
      patchItem(set, path, {
        mediaId,
        info,
        durationSec: info.durationSec,
        hasVideo: info.video !== null,
        hasAudio: info.audio !== null,
        kind: info.kind,
      });
      await runDerivedJobs(path, info.durationSec, info.video !== null, info.kind, hasAlpha);
    } catch (err) {
      if (!get().items.some((i) => i.path === path)) return;
      patchItem(set, path, { status: "error", error: String(err) });
      toast(`Could not import ${basename(path)}: ${String(err)}`, "error");
    }
  }

  // Live proxy progress (emitted by the Rust side during encodes).
  void listen<ProxyProgressEvent>(PROXY_PROGRESS_EVENT, (e) => {
    const { srcPath, percent } = e.payload;
    if (get().items.some((i) => i.path === srcPath && i.status === "processing")) {
      patchItem(set, srcPath, { proxyProgress: percent });
    }
  });

  // Files can vanish while Cutty is in the background; re-check on focus.
  window.addEventListener("focus", () => {
    void get().checkMissing();
  });

  return {
    items: [],
    missingMediaIds: new Set<number>(),

    importFiles: async (paths) => {
      const fresh: string[] = [];
      for (const path of paths) {
        if (get().items.some((i) => i.path === path)) {
          toast(`${basename(path)} is already in the media pool.`);
          continue;
        }
        fresh.push(path);
      }
      if (fresh.length === 0) return;
      set((state) => ({
        items: [
          ...state.items,
          ...fresh.map(
            (path): PoolItem => ({
              path,
              name: basename(path),
              mediaId: null,
              status: "probing",
              error: null,
              durationSec: null,
              hasVideo: false,
              hasAudio: false,
              kind: "video",
              info: null,
              proxyPath: null,
              proxyProgress: null,
              thumbnailUrl: null,
              missing: false,
            }),
          ),
        ],
      }));
      await Promise.all(fresh.map(runImport));
      void get().checkMissing();
    },

    forgetItem: (path) => {
      const item = get().items.find((i) => i.path === path);
      if (item?.thumbnailUrl) URL.revokeObjectURL(item.thumbnailUrl);
      forgetFilmstrip(path);
      forgetPeaks(path);
      set((state) => ({
        items: state.items.filter((i) => i.path !== path),
      }));
    },

    dropPendingImports: () => {
      for (const item of get().items) {
        if (item.mediaId === null) get().forgetItem(item.path);
      }
    },

    syncFromProject: (project) => {
      // Real files only — the Seed-50 dev tool registers dummy:// media
      // that has no file behind it and doesn't belong in the pool.
      const engineMedia = project.media.filter((m) => m.path.startsWith("/"));
      const signature = engineMedia.map((m) => `${m.id}:${m.path}`).join("|");
      if (signature === lastSyncSignature) return;
      lastSyncSignature = signature;
      // The media set changed (import, removal, project switch): clip-id
      // keyed render caches may now point at other content.
      clearWaveformRenders();

      const state = get();
      const enginePaths = new Set(engineMedia.map((m) => m.path));

      // Items whose engine registration vanished (RemoveMedia, or an undo
      // of nothing-we-know): drop the local state too.
      for (const item of state.items) {
        if (item.mediaId !== null && !enginePaths.has(item.path)) {
          state.forgetItem(item.path);
        }
      }

      // Engine media without a pool item (undo of RemoveMedia, project
      // load, crash recovery): recreate the item and rebuild its derived
      // state — proxy/thumbnail hit their caches, so this is near-instant.
      // Missing files are flagged red without derived jobs (and without
      // per-file error toasts: the project must open quietly).
      for (const m of engineMedia) {
        const existing = get().items.find((i) => i.path === m.path);
        if (existing) {
          if (existing.mediaId !== m.id) patchItem(set, m.path, { mediaId: m.id });
          continue;
        }
        set((s) => ({
          items: [
            ...s.items,
            {
              path: m.path,
              name: basename(m.path),
              mediaId: m.id,
              status: "processing",
              error: null,
              durationSec: m.duration,
              hasVideo: m.hasVideo,
              hasAudio: m.hasAudio,
              kind: m.kind,
              info: null,
              proxyPath: null,
              proxyProgress: null,
              thumbnailUrl: null,
              missing: false,
            },
          ],
        }));
        void (async () => {
          const [exists] = await pathsExist([m.path]);
          if (!get().items.some((i) => i.path === m.path)) return;
          if (!exists) {
            patchItem(set, m.path, {
              missing: true,
              status: "error",
              error: "File not found",
            });
            set((s) => {
              const ids = new Set(s.missingMediaIds);
              ids.add(m.id);
              return { missingMediaIds: ids };
            });
            return;
          }
          // Re-probe for the full stream info (source dimensions drive
          // the player gizmo's box) — engine registration already stands.
          void probeMedia(m.path)
            .then((info) => {
              if (get().items.some((i) => i.path === m.path)) {
                patchItem(set, m.path, { info });
              }
            })
            .catch(() => undefined);
          await runDerivedJobs(m.path, m.duration, m.hasVideo, m.kind, m.hasAlpha);
          void get().checkMissing();
        })();
      }
    },

    checkMissing: async () => {
      const items = get().items;
      if (items.length === 0) {
        if (get().missingMediaIds.size > 0) {
          set({ missingMediaIds: new Set<number>() });
        }
        return;
      }
      const paths = items.map((i) => i.path);
      const exists = await pathsExist(paths);
      const missingIds = new Set<number>();
      const revived: PoolItem[] = [];
      const current = get().items;
      const updated = current.map((item) => {
        const idx = paths.indexOf(item.path);
        if (idx < 0) return item;
        const missing = !exists[idx];
        if (missing && item.mediaId !== null) missingIds.add(item.mediaId);
        if (!missing && item.missing && item.status === "error") {
          revived.push(item);
        }
        return missing === item.missing ? item : { ...item, missing };
      });
      set({ items: updated, missingMediaIds: missingIds });
      // Files that came back (drive remounted, file restored): rebuild
      // their derived state so they become usable again.
      for (const item of revived) {
        patchItem(set, item.path, { status: "processing", error: null });
        void runDerivedJobs(
          item.path,
          item.durationSec ?? 0,
          item.hasVideo,
          item.kind,
          item.info?.video?.hasAlpha ?? false,
        );
      }
    },
  };
});
