// Self-driving timeline perf benchmark (dev acceptance tool).
//
// Activated only when the Rust side reports `CUTTY_BENCH=1`: imports the
// given media, seeds the full-visuals layout (3 video + 2 audio lanes,
// 62 clips with filmstrips + waveforms), then measures the timeline
// renderer under a continuous pan — the same redraw stream a user drag
// produces — and reports draw-time/fps stats back to Rust. Runs with no
// synthetic input, so it is safe on a shared desktop.

import { invoke } from "@tauri-apps/api/core";
import { useMediaStore } from "../state/mediaStore";
import { seedVisualLanes } from "../timeline/actions";
import { requestDraw } from "../timeline/dirty";
import { drawStats, resetStats, setHudForSession } from "../timeline/perf";
import { view, setZoom } from "../timeline/view";

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

async function waitForPool(deadlineMs: number): Promise<void> {
  const t0 = performance.now();
  for (;;) {
    const items = useMediaStore.getState().items;
    const pending = items.filter(
      (i) => i.status === "probing" || i.status === "processing",
    );
    if (items.length > 0 && pending.length === 0) return;
    if (performance.now() - t0 > deadlineMs) return; // measure with what's ready
    await sleep(250);
  }
}

/** One pan pass: scroll `pxPerFrame` per rAF for `ms`, forcing a redraw
 * every frame (exactly what a drag/pan interaction produces). */
function panPhase(ms: number, pxPerFrame: number): Promise<number> {
  return new Promise((resolve) => {
    const t0 = performance.now();
    let frames = 0;
    const tick = (): void => {
      view.scrollPx = Math.max(0, view.scrollPx + pxPerFrame);
      requestDraw();
      frames++;
      if (performance.now() - t0 < ms) {
        requestAnimationFrame(tick);
      } else {
        resolve(frames);
      }
    };
    requestAnimationFrame(tick);
  });
}

let benchStarted = false;

export async function maybeRunBench(): Promise<void> {
  // Idempotent under React StrictMode's double-effects — two racing
  // bench instances tear each other's transactions down.
  if (benchStarted) return;
  benchStarted = true;
  let media: string[] | null;
  try {
    media = await invoke<string[] | null>("bench_config");
  } catch {
    return; // command missing (release build without it, etc.)
  }
  if (!media || media.length === 0) return;

  const log = (msg: string): void => console.log(`[bench] ${msg}`);
  try {
    // Let startup session-restore settle before taking over, then start
    // clean: fresh project, no stray transaction (a restore interrupted
    // by the project swap can leave one open).
    await sleep(2_000);
    log("starting a fresh project");
    await (await import("./projectActions")).newProject();
    await sleep(1_000);
    const engineIpc = await import("./engineIpc");
    await engineIpc.engineRollbackTransaction().catch(() => undefined);
    log(`importing ${media.length} files`);
    await useMediaStore.getState().importFiles(media);
    await waitForPool(180_000);
    await engineIpc.engineRollbackTransaction().catch(() => undefined);
    const pool = useMediaStore.getState().items.map((i) => ({
      name: i.name,
      status: i.status,
      kind: i.kind,
      hasVideo: i.hasVideo,
      hasAudio: i.hasAudio,
      durationSec: i.durationSec,
      error: i.error,
    }));
    log("pool ready; seeding visual lanes");
    const seedError = await seedVisualLanes();
    // The store applies engine snapshots asynchronously — wait until the
    // clip count stops moving before trusting it.
    const projectStore = (await import("../state/projectStore")).useProjectStore;
    const countClips = (): number =>
      projectStore.getState().project?.tracks.reduce((n, t) => n + t.clips.length, 0) ?? 0;
    let seededClips = countClips();
    for (let i = 0; i < 20; i++) {
      await sleep(400);
      const now = countClips();
      if (now === seededClips && now > 0) break;
      seededClips = now;
    }
    if (seededClips < 60) {
      await invoke("bench_report", {
        report: JSON.stringify(
          { error: `seed produced ${seededClips} clips`, seedError, pool },
          null,
          2,
        ),
      });
      return;
    }

    // Let filmstrips/peaks land (fetches kick off on first draw) and the
    // decoded sprites upload; force a few draws to trigger the fetches.
    setHudForSession(true);
    for (let i = 0; i < 20; i++) {
      requestDraw();
      await sleep(150);
    }

    // Frame the view like a real session: zoomed to show a dense stretch.
    setZoom(24, 0);
    view.scrollPx = 0;
    requestDraw();
    await sleep(300);

    // Warm-up pan (JIT, image uploads), then the measured pan.
    await panPhase(1_500, 3);
    resetStats();
    const frames = await panPhase(6_000, 3);
    const stats = drawStats();

    // Visual record: dump the timeline canvas as PNG (filmstrips +
    // waveforms + HUD, pixel-exact, no screen capture involved) —
    // rewound to 0 so the content is in frame.
    try {
      view.scrollPx = 0;
      requestDraw();
      await sleep(200);
      const canvases = Array.from(document.querySelectorAll("canvas"));
      const timelineCanvas = canvases[canvases.length - 1];
      const blob = await new Promise<Blob | null>((r) =>
        timelineCanvas.toBlob((b) => r(b), "image/png"),
      );
      if (blob) {
        const bytes = Array.from(new Uint8Array(await blob.arrayBuffer()));
        await invoke("bench_snapshot", { png: bytes });
      }
    } catch (err) {
      log(`snapshot failed: ${String(err)}`);
    }

    // Second scenario: zoomed-in fast pan (filmstrip tiles repeat).
    setZoom(160, 0);
    view.scrollPx = 0;
    await panPhase(1_000, 8);
    resetStats();
    await panPhase(4_000, 8);
    const statsZoomed = drawStats();

    const project = (await import("../state/projectStore")).useProjectStore.getState()
      .project;
    const clipCount =
      project?.tracks.reduce((n, t) => n + t.clips.length, 0) ?? 0;
    const report = {
      clipCount,
      tracks: project?.tracks.map((t) => ({ kind: t.kind, clips: t.clips.length })),
      devicePixelRatio: window.devicePixelRatio,
      canvas: { w: window.innerWidth, h: window.innerHeight },
      panFrames: frames,
      overview: stats,
      zoomedIn: statsZoomed,
    };
    log(`done: ${JSON.stringify(report)}`);
    await invoke("bench_report", { report: JSON.stringify(report, null, 2) });
  } catch (err) {
    try {
      await invoke("bench_report", {
        report: JSON.stringify({ error: String(err) }),
      });
    } catch {
      console.error("[bench] failed", err);
    }
  }
}
