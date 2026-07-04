// Timeline operations shared by the toolbar and keyboard shortcuts. Every
// mutation is an engine command over IPC; multi-clip operations are
// wrapped in an engine transaction so each user action is exactly one
// undo step. No timeline math here — target picking reads engine state,
// the engine validates and clamps.

import type { Clip, Track, TrimEdge } from "../lib/engineIpc";
import {
  engineAddClip,
  engineAddMedia,
  engineBeginTransaction,
  engineCommitTransaction,
  engineDeleteClip,
  engineRedo,
  engineRippleDelete,
  engineRollbackTransaction,
  engineSplitClip,
  engineTrimClip,
  engineUndo,
} from "../lib/engineIpc";
import { useProjectStore } from "../state/projectStore";
import { ensureVisible } from "./view";

/** Run `run` inside a transaction when it spans several commands, so the
 * whole action lands on the undo stack as a single entry. */
async function grouped(count: number, run: () => Promise<void>): Promise<void> {
  if (count <= 1) {
    await run();
    return;
  }
  await engineBeginTransaction();
  try {
    await run();
    await engineCommitTransaction();
  } catch (err) {
    await engineRollbackTransaction().catch(() => undefined);
    throw err;
  }
}

/** Clips whose timeline range strictly contains the playhead. */
function clipsUnderPlayhead(): Clip[] {
  const { project, playheadSec } = useProjectStore.getState();
  if (!project) return [];
  const under: Clip[] = [];
  for (const track of project.tracks) {
    for (const clip of track.clips) {
      if (clip.timelineIn < playheadSec && playheadSec < clip.timelineOut) {
        under.push(clip);
      }
    }
  }
  return under;
}

/**
 * Split at the playhead: selected clips under the playhead if any are,
 * otherwise every clip under the playhead. One undo step.
 */
export async function splitAtPlayhead(): Promise<void> {
  const { playheadSec, selection } = useProjectStore.getState();
  const under = clipsUnderPlayhead();
  const selectedUnder = under.filter((c) => selection.includes(c.id));
  const targets = selectedUnder.length > 0 ? selectedUnder : under;
  if (targets.length === 0) return;

  const rightIds: number[] = [];
  await grouped(targets.length, async () => {
    for (const clip of targets) {
      try {
        rightIds.push(await engineSplitClip(clip.id, playheadSec));
      } catch (err) {
        // Too close to a clip edge — the engine rejected it; skip.
        console.warn("split rejected", err);
      }
    }
  });
  if (selectedUnder.length > 0 && rightIds.length > 0) {
    const store = useProjectStore.getState();
    store.setSelection([...store.selection, ...rightIds]);
  }
}

/** Delete the selected clips (ripple shifts later clips left). One undo step. */
export async function deleteSelection(ripple: boolean): Promise<void> {
  const { selection } = useProjectStore.getState();
  if (selection.length === 0) return;
  await grouped(selection.length, async () => {
    for (const id of selection) {
      try {
        if (ripple) await engineRippleDelete(id);
        else await engineDeleteClip(id);
      } catch (err) {
        console.warn("delete rejected", err);
      }
    }
  });
  useProjectStore.getState().setSelection([]);
}

/** Q/W: trim the given edge of selected clips under the playhead to it. */
export async function trimSelectedToPlayhead(edge: TrimEdge): Promise<void> {
  const { playheadSec, selection } = useProjectStore.getState();
  const targets = clipsUnderPlayhead().filter((c) => selection.includes(c.id));
  if (targets.length === 0) return;
  await grouped(targets.length, async () => {
    for (const clip of targets) {
      try {
        await engineTrimClip(clip.id, edge, playheadSec);
      } catch (err) {
        console.warn("trim rejected", err);
      }
    }
  });
}

export async function undo(): Promise<void> {
  try {
    await engineUndo();
  } catch (err) {
    console.warn("undo failed", err);
  }
}

export async function redo(): Promise<void> {
  try {
    await engineRedo();
  } catch (err) {
    console.warn("redo failed", err);
  }
}

/** Step the playhead by one project frame. */
export function stepFrame(direction: -1 | 1): void {
  const { project, playheadSec, setPlayhead } = useProjectStore.getState();
  const fps = project?.settings.fps ?? 30;
  const next = Math.max(0, playheadSec + direction / fps);
  setPlayhead(next);
  ensureVisible(next);
}

export function goToStart(): void {
  useProjectStore.getState().setPlayhead(0);
  ensureVisible(0);
}

/** End key: jump to the last clip's out point (0 on an empty timeline). */
export function goToEnd(): void {
  const { project, setPlayhead } = useProjectStore.getState();
  let end = 0;
  for (const track of project?.tracks ?? []) {
    // Clips are sorted and non-overlapping, so the last one ends the track.
    const last = track.clips[track.clips.length - 1];
    if (last && last.timelineOut > end) end = last.timelineOut;
  }
  setPlayhead(end);
  ensureVisible(end);
}

// ---------------------------------------------------------------------
// Dev seeding — Phase 1 acceptance runs against 50 dummy clips.
// ---------------------------------------------------------------------

const DUMMY_MEDIA = [
  { path: "dummy://b-roll_beach.mp4", duration: 90 },
  { path: "dummy://interview_take2.mp4", duration: 120 },
  { path: "dummy://drone_pass.mp4", duration: 60 },
  { path: "dummy://music_bed.mp4", duration: 180 },
];

/** Deterministic PRNG so seeded layouts are reproducible run to run. */
function mulberry32(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function trackEnd(track: Track): number {
  const last = track.clips[track.clips.length - 1];
  return last ? last.timelineOut : 0;
}

/**
 * Add 50 dummy clips (25 video + 25 audio) after any existing content, as
 * a single undo step. Dev tool for the Phase 1 performance acceptance —
 * real import lands in another prompt.
 */
export async function seedDummyClips(): Promise<void> {
  const { project } = useProjectStore.getState();
  const videoTrack = project?.tracks.find((t) => t.kind === "video");
  const audioTrack = project?.tracks.find((t) => t.kind === "audio");
  if (!videoTrack || !audioTrack) return;

  const rng = mulberry32(0xc0ffee);
  await engineBeginTransaction();
  try {
    const mediaIds: number[] = [];
    for (const def of DUMMY_MEDIA) {
      mediaIds.push(await engineAddMedia(def.path, def.duration, true, true));
    }
    let n = 0;
    for (const track of [videoTrack, audioTrack]) {
      let t = trackEnd(track) + (track.clips.length > 0 ? 0.5 : 0);
      for (let i = 0; i < 25; i++) {
        const mediaIndex = n % DUMMY_MEDIA.length;
        const duration = 1.5 + rng() * 4;
        const sourceIn = rng() * (DUMMY_MEDIA[mediaIndex].duration - duration);
        await engineAddClip(
          track.id,
          mediaIds[mediaIndex],
          t,
          sourceIn,
          sourceIn + duration,
        );
        t += duration + rng() * 1.2;
        n++;
      }
    }
    await engineCommitTransaction();
  } catch (err) {
    console.error("seeding dummy clips failed", err);
    await engineRollbackTransaction().catch(() => undefined);
  }
}
