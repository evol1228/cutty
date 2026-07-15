// UI mirror of playback transport state. The Rust playback engine owns
// the clock and transport; this store only reflects what its events
// report back. The playhead position itself lives in projectStore
// (playheadSec) — there is exactly one playhead.

import { create } from "zustand";

interface PlayerState {
  /** The playback engine is attached and accepting transport commands. */
  attached: boolean;
  /** Engine-reported transport state. */
  playing: boolean;

  setAttached: (attached: boolean) => void;
  setPlaying: (playing: boolean) => void;
}

export const usePlayerStore = create<PlayerState>((set) => ({
  attached: false,
  playing: false,

  setAttached: (attached) => set({ attached }),
  setPlaying: (playing) => set({ playing }),
}));
