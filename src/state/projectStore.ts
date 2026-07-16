// UI mirror of engine state plus timeline UI state (selection, playhead,
// snap toggle, zoom). The Rust engine owns the project — this store only
// reflects snapshots it emits, never derives or mutates timeline data.

import { create } from "zustand";
import type {
  EngineSnapshot,
  Project,
  TransitionSpan,
} from "../lib/engineIpc";

/** The transition-picker popover (double-click on a chip). */
export interface TransitionPicker {
  /** Outgoing clip id (the transition's owner). */
  clipId: number;
  /** Current kind, preselected in the list. */
  kind: string;
  /** Anchor position, viewport CSS px. */
  x: number;
  y: number;
}

/** A selected volume-keyframe dot, addressed by its clip-relative time
 * (the engine's keyframe reference scheme). */
export interface SelectedKeyframe {
  clipId: number;
  /** Clip-relative seconds. Kept in lockstep with engine clamping —
   * every applied move updates it. */
  t: number;
}

interface ProjectState {
  /** Latest engine snapshot; null until the initial fetch lands. */
  project: Project | null;
  /** Resolved transition spans (engine-computed, same snapshot). */
  transitions: TransitionSpan[];
  undoDepth: number;
  redoDepth: number;

  /** Selected clip ids. */
  selection: number[];
  /** Selected transition, by its outgoing clip id (chips and clips have
   * mutually exclusive selection, like CapCut). */
  selectedTransition: number | null;
  /** Selected volume-keyframe dot (Del removes it). Lives alongside the
   * clip selection — its clip is always selected too. */
  selectedKeyframe: SelectedKeyframe | null;
  /** Open transition picker, if any. */
  transitionPicker: TransitionPicker | null;
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
  setSelectedTransition: (fromClipId: number | null) => void;
  setSelectedKeyframe: (kf: SelectedKeyframe | null) => void;
  setTransitionPicker: (picker: TransitionPicker | null) => void;
  setPlayhead: (sec: number) => void;
  setSnapEnabled: (enabled: boolean) => void;
  setPxPerSec: (pxPerSec: number) => void;
  setTrackScrollPx: (px: number) => void;
}

export const useProjectStore = create<ProjectState>((set) => ({
  project: null,
  transitions: [],
  undoDepth: 0,
  redoDepth: 0,
  selection: [],
  selectedTransition: null,
  selectedKeyframe: null,
  transitionPicker: null,
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
      // A selected/being-picked transition survives only while its span
      // still resolves.
      const spans = new Set(snapshot.transitions.map((t) => t.fromClipId));
      const selectedTransition =
        state.selectedTransition !== null && spans.has(state.selectedTransition)
          ? state.selectedTransition
          : null;
      const transitionPicker =
        state.transitionPicker !== null && spans.has(state.transitionPicker.clipId)
          ? state.transitionPicker
          : null;
      // A selected keyframe survives only while its dot still exists
      // (undo/redo/trim may have removed or re-anchored it).
      let selectedKeyframe = state.selectedKeyframe;
      if (selectedKeyframe !== null) {
        const target = selectedKeyframe;
        const lane = snapshot.project.tracks
          .flatMap((t) => t.clips)
          .find((c) => c.id === target.clipId)?.keyframes?.volume;
        if (!lane?.some((k) => Math.abs(k.t - target.t) < 5e-4)) {
          selectedKeyframe = null;
        }
      }
      return {
        project: snapshot.project,
        transitions: snapshot.transitions,
        undoDepth: snapshot.undoDepth,
        redoDepth: snapshot.redoDepth,
        selection:
          selection.length === state.selection.length
            ? state.selection
            : selection,
        selectedTransition,
        selectedKeyframe,
        transitionPicker,
      };
    }),
  setSelection: (selection) =>
    set((state) => ({
      selection,
      selectedTransition: selection.length > 0 ? null : state.selectedTransition,
      selectedKeyframe:
        state.selectedKeyframe !== null &&
        selection.includes(state.selectedKeyframe.clipId)
          ? state.selectedKeyframe
          : null,
    })),
  toggleSelected: (id) =>
    set((state) => {
      const selection = state.selection.includes(id)
        ? state.selection.filter((s) => s !== id)
        : [...state.selection, id];
      return {
        selection,
        selectedTransition: null,
        selectedKeyframe:
          state.selectedKeyframe !== null &&
          selection.includes(state.selectedKeyframe.clipId)
            ? state.selectedKeyframe
            : null,
      };
    }),
  setSelectedTransition: (selectedTransition) =>
    set((state) => ({
      selectedTransition,
      selection: selectedTransition !== null ? [] : state.selection,
      selectedKeyframe:
        selectedTransition !== null ? null : state.selectedKeyframe,
    })),
  setSelectedKeyframe: (selectedKeyframe) =>
    set((state) => ({
      selectedKeyframe,
      selectedTransition:
        selectedKeyframe !== null ? null : state.selectedTransition,
    })),
  setTransitionPicker: (transitionPicker) => set({ transitionPicker }),
  setPlayhead: (playheadSec) =>
    set({ playheadSec: Math.max(0, playheadSec) }),
  setSnapEnabled: (snapEnabled) => set({ snapEnabled }),
  setPxPerSec: (pxPerSec) => set({ pxPerSec }),
  setTrackScrollPx: (trackScrollPx) => set({ trackScrollPx }),
}));
