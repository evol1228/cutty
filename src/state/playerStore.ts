// UI mirror of pipeline state. The Rust side owns all media/playback state;
// this store only reflects what events/commands report back.

import { create } from "zustand";
import type { MediaInfo, PlayerInfo } from "../lib/ipc";

interface PlayerState {
  /** Probed source file, if any. */
  media: MediaInfo | null;
  /** Path of the generated 720p proxy, once ready. */
  proxyPath: string | null;
  /** Proxy generation progress 0–100, or null when not generating. */
  proxyProgress: number | null;

  /** Set once the playback engine is open on the proxy. */
  playerInfo: PlayerInfo | null;
  playing: boolean;
  /** Engine-reported position (frame pts / transport events). */
  positionSec: number;
  /** Trim marks for the Phase 0 export test. */
  inPointSec: number | null;
  outPointSec: number | null;

  setMedia: (media: MediaInfo | null) => void;
  setProxyPath: (path: string | null) => void;
  setProxyProgress: (percent: number | null) => void;
  setPlayerInfo: (info: PlayerInfo | null) => void;
  setPlaying: (playing: boolean) => void;
  setPosition: (sec: number) => void;
  setInPoint: (sec: number | null) => void;
  setOutPoint: (sec: number | null) => void;
}

export const usePlayerStore = create<PlayerState>((set) => ({
  media: null,
  proxyPath: null,
  proxyProgress: null,
  playerInfo: null,
  playing: false,
  positionSec: 0,
  inPointSec: null,
  outPointSec: null,

  setMedia: (media) =>
    set({
      media,
      proxyPath: null,
      proxyProgress: null,
      playerInfo: null,
      playing: false,
      positionSec: 0,
      inPointSec: null,
      outPointSec: null,
    }),
  setProxyPath: (proxyPath) => set({ proxyPath, proxyProgress: null }),
  setProxyProgress: (proxyProgress) => set({ proxyProgress }),
  setPlayerInfo: (playerInfo) =>
    set({ playerInfo, playing: false, positionSec: 0 }),
  setPlaying: (playing) => set({ playing }),
  setPosition: (positionSec) => set({ positionSec }),
  setInPoint: (inPointSec) => set({ inPointSec }),
  setOutPoint: (outPointSec) => set({ outPointSec }),
}));
