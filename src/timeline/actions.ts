// Timeline operations shared by the toolbar and keyboard shortcuts. Every
// mutation is an engine command over IPC; multi-clip operations are
// wrapped in an engine transaction so each user action is exactly one
// undo step. No timeline math here — target picking reads engine state,
// the engine validates and clamps.

import type { Clip, Project, Track, TrimEdge } from "../lib/engineIpc";
import {
  engineAddClip,
  engineAddMedia,
  engineAddTrack,
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
import { playbackSeek, playbackStep } from "../lib/ipc";
import { useMediaStore } from "../state/mediaStore";
import { useProjectStore } from "../state/projectStore";
import { toast } from "../state/toastStore";
import { ensureVisible } from "./view";

/** Timeline end (latest clip out point) — display/navigation helper; the
 * engine's `timeline_end` is the authority during playback. */
export function timelineEndSec(project: Project): number {
  let end = 0;
  for (const track of project.tracks) {
    // Clips are sorted and non-overlapping, so the last one ends the track.
    const last = track.clips[track.clips.length - 1];
    if (last && last.timelineOut > end) end = last.timelineOut;
  }
  return end;
}

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

/** Step the playhead by one project frame — the engine decodes and shows
 * the exact frame, across cut boundaries; its position event moves the
 * playhead. The optimistic store update keeps the UI snappy. */
export function stepFrame(direction: -1 | 1): void {
  const { project, playheadSec, setPlayhead } = useProjectStore.getState();
  const fps = project?.settings.fps ?? 30;
  const next = Math.max(0, playheadSec + direction / fps);
  setPlayhead(next);
  ensureVisible(next);
  void playbackStep(direction).catch(() => undefined);
}

export function goToStart(): void {
  useProjectStore.getState().setPlayhead(0);
  ensureVisible(0);
  void playbackSeek(0).catch(() => undefined);
}

/** End key: jump to the last clip's out point (0 on an empty timeline). */
export function goToEnd(): void {
  const { project, setPlayhead } = useProjectStore.getState();
  const end = project ? timelineEndSec(project) : 0;
  setPlayhead(end);
  ensureVisible(end);
  void playbackSeek(end).catch(() => undefined);
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
 * Build the playback-acceptance timeline from real imported media: 13
 * short segments (12 hard cuts) cycling the first three ready video
 * files in the pool, with varied in-points. One undo step. Dev tool for
 * validating multi-cut playback in the running app.
 */
export async function seedCutTimeline(): Promise<void> {
  const { project } = useProjectStore.getState();
  const videoTrack = project?.tracks.find((t) => t.kind === "video");
  if (!videoTrack) return;
  const ready = useMediaStore
    .getState()
    .items.flatMap((i) =>
      i.mediaId !== null &&
      i.hasVideo &&
      !i.missing &&
      i.status !== "error" &&
      i.durationSec !== null &&
      i.durationSec >= 3
        ? [{ mediaId: i.mediaId, durationSec: i.durationSec }]
        : [],
    );
  if (ready.length < 3) {
    toast("Seed cuts needs 3+ imported video files of ≥3s each.");
    return;
  }
  const media = ready.slice(0, 3);
  const rng = mulberry32(0xcafe);
  await engineBeginTransaction();
  try {
    let t = trackEnd(videoTrack) + (videoTrack.clips.length > 0 ? 0.5 : 0);
    for (let i = 0; i < 13; i++) {
      const m = media[i % 3];
      const dur = 0.8 + rng() * 1.4;
      const maxIn = Math.max(0, m.durationSec - dur - 0.2);
      const sourceIn = rng() * maxIn;
      await engineAddClip(videoTrack.id, m.mediaId, t, sourceIn, sourceIn + dur);
      t += dur;
    }
    await engineCommitTransaction();
  } catch (err) {
    console.error("seeding cut timeline failed", err);
    await engineRollbackTransaction().catch(() => undefined);
  }
}

/**
 * Seed the Phase 2 performance-acceptance layout as one undo step: 3
 * video lanes × 20 clips plus 20 audio clips, creating the extra video
 * tracks when needed. Dev tool — real import lands elsewhere.
 */
export async function seedDummyClips(): Promise<void> {
  const { project } = useProjectStore.getState();
  const audioTrack = project?.tracks.find((t) => t.kind === "audio");
  if (!project || !audioTrack) return;

  const rng = mulberry32(0xc0ffee);
  await engineBeginTransaction();
  try {
    const videoTrackIds = project.tracks
      .filter((t) => t.kind === "video" && !t.locked)
      .map((t) => t.id);
    while (videoTrackIds.length < 3) {
      videoTrackIds.push(await engineAddTrack("video", 0));
    }

    const mediaIds: number[] = [];
    for (const def of DUMMY_MEDIA) {
      mediaIds.push(await engineAddMedia(def.path, def.duration, true, true));
    }

    // Existing clip ends per track, so reseeding appends instead of failing.
    const endOf = (id: number): number => {
      const track = useProjectStore
        .getState()
        .project?.tracks.find((t) => t.id === id);
      return track ? trackEnd(track) + (track.clips.length > 0 ? 0.5 : 0) : 0;
    };

    let n = 0;
    for (const trackId of [...videoTrackIds.slice(0, 3), audioTrack.id]) {
      let t = endOf(trackId);
      for (let i = 0; i < 20; i++) {
        const mediaIndex = n % DUMMY_MEDIA.length;
        const duration = 1.5 + rng() * 4;
        const sourceIn = rng() * (DUMMY_MEDIA[mediaIndex].duration - duration);
        await engineAddClip(
          trackId,
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
