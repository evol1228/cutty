// UI mirror of engine state plus timeline UI state (selection, playhead,
// snap toggle, zoom). The Rust engine owns the project — this store only
// reflects snapshots it emits, never derives or mutates timeline data.

import { create } from "zustand";
import type { EngineSnapshot, Project } from "../lib/engineIpc";

interface ProjectState {
  /** Latest engine snapshot; null until the initial fetch lands. */
  project: Project | null;
  undoDepth: number;
  redoDepth: number;

  /** Selected clip ids. */
  selection: number[];
  /** Playhead position, seconds (playback lands in a later prompt). */
  playheadSec: number;
  snapEnabled: boolean;
  /** Timeline zoom, CSS pixels per second (mirrored by the canvas view). */
  pxPerSec: number;
  /** Vertical track scroll, CSS pixels (mirrored by the canvas view; the
   * React header column translates by this). */
  trackScrollPx: number;

  applySnapshot: (snapshot: EngineSnapshot) => void;
  setSelection: (ids: number[]) => void;
  toggleSelected: (id: number) => void;
  setPlayhead: (sec: number) => void;
  setSnapEnabled: (enabled: boolean) => void;
  setPxPerSec: (pxPerSec: number) => void;
  setTrackScrollPx: (px: number) => void;
}

export const useProjectStore = create<ProjectState>((set) => ({
  project: null,
  undoDepth: 0,
  redoDepth: 0,
  selection: [],
  playheadSec: 0,
  snapEnabled: true,
  pxPerSec: 60,
  trackScrollPx: 0,

  applySnapshot: (snapshot) =>
    set((state) => {
      // Prune selection to clips that still exist (deletes, undo, …).
      const alive = new Set<number>();
      for (const track of snapshot.project.tracks) {
        for (const clip of track.clips) alive.add(clip.id);
      }
      const selection = state.selection.filter((id) => alive.has(id));
      return {
        project: snapshot.project,
        undoDepth: snapshot.undoDepth,
        redoDepth: snapshot.redoDepth,
        selection:
          selection.length === state.selection.length
            ? state.selection
            : selection,
      };
    }),
  setSelection: (selection) => set({ selection }),
  toggleSelected: (id) =>
    set((state) => ({
      selection: state.selection.includes(id)
        ? state.selection.filter((s) => s !== id)
        : [...state.selection, id],
    })),
  setPlayhead: (playheadSec) =>
    set({ playheadSec: Math.max(0, playheadSec) }),
  setSnapEnabled: (snapEnabled) => set({ snapEnabled }),
  setPxPerSec: (pxPerSec) => set({ pxPerSec }),
  setTrackScrollPx: (trackScrollPx) => set({ trackScrollPx }),
}));
