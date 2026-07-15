// Typed wrappers around the engine IPC surface. These mirror the Rust
// types in cutty-engine (serde camelCase). The engine owns all timeline
// state — the frontend only sends these commands and renders the
// EngineSnapshot events that come back.

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export interface ProjectSettings {
  width: number;
  height: number;
  fps: number;
}

export interface MediaRef {
  id: number;
  path: string;
  duration: number;
  hasVideo: boolean;
  hasAudio: boolean;
}

export type TrackKind = "video" | "audio";

export interface Transform {
  x: number;
  y: number;
  scale: number;
  rotation: number;
}

export interface Clip {
  id: number;
  mediaId: number;
  timelineIn: number;
  timelineOut: number;
  sourceIn: number;
  sourceOut: number;
  transform: Transform;
  opacity: number;
  speed: number;
  volume: number;
}

export interface Track {
  id: number;
  kind: TrackKind;
  name: string;
  locked: boolean;
  muted: boolean;
  clips: Clip[];
}

export interface Project {
  settings: ProjectSettings;
  media: MediaRef[];
  tracks: Track[];
}

export interface EngineSnapshot {
  project: Project;
  undoDepth: number;
  redoDepth: number;
  transactionActive: boolean;
}

export type TrimEdge = "start" | "end";

export interface SnappedMove {
  timelineIn: number;
  snapPoint: number | null;
}

export const ENGINE_STATE_EVENT = "engine://project";

/** Subscribe to engine state snapshots (fires after every mutation). */
export function onEngineState(
  handler: (snapshot: EngineSnapshot) => void,
): Promise<UnlistenFn> {
  return listen<EngineSnapshot>(ENGINE_STATE_EVENT, (e) => handler(e.payload));
}

export function engineGetState(): Promise<EngineSnapshot> {
  return invoke<EngineSnapshot>("engine_get_state");
}

export function engineAddMedia(
  path: string,
  duration: number,
  hasVideo: boolean,
  hasAudio: boolean,
): Promise<number> {
  return invoke<number>("engine_add_media", {
    path,
    duration,
    hasVideo,
    hasAudio,
  });
}

/** Remove a media file and every clip referencing it (one undo step). */
export function engineRemoveMedia(mediaId: number): Promise<void> {
  return invoke("engine_remove_media", { mediaId });
}

export function engineAddClip(
  trackId: number,
  mediaId: number,
  timelineIn: number,
  sourceIn: number,
  sourceOut: number,
): Promise<number> {
  return invoke<number>("engine_add_clip", {
    trackId,
    mediaId,
    timelineIn,
    sourceIn,
    sourceOut,
  });
}

export function engineMoveClip(
  clipId: number,
  timelineIn: number,
): Promise<void> {
  return invoke("engine_move_clip", { clipId, timelineIn });
}

/** Returns the clamped edge time the engine actually applied. */
export function engineTrimClip(
  clipId: number,
  edge: TrimEdge,
  to: number,
): Promise<number> {
  return invoke<number>("engine_trim_clip", { clipId, edge, to });
}

/** Returns the new right half's clip id. */
export function engineSplitClip(clipId: number, at: number): Promise<number> {
  return invoke<number>("engine_split_clip", { clipId, at });
}

export function engineDeleteClip(clipId: number): Promise<void> {
  return invoke("engine_delete_clip", { clipId });
}

export function engineRippleDelete(clipId: number): Promise<void> {
  return invoke("engine_ripple_delete", { clipId });
}

/** Set a clip's audio gain (linear; 1.0 = unity, 0.0 = silent). */
export function engineSetClipVolume(
  clipId: number,
  volume: number,
): Promise<void> {
  return invoke("engine_set_clip_volume", { clipId, volume });
}

export function engineUndo(): Promise<boolean> {
  return invoke<boolean>("engine_undo");
}

export function engineRedo(): Promise<boolean> {
  return invoke<boolean>("engine_redo");
}

export function engineBeginTransaction(): Promise<void> {
  return invoke("engine_begin_transaction");
}

export function engineCommitTransaction(): Promise<void> {
  return invoke("engine_commit_transaction");
}

export function engineRollbackTransaction(): Promise<void> {
  return invoke("engine_rollback_transaction");
}

/** Snap a time to clip edges / playhead; null when nothing is in range. */
export function engineSnapTime(
  t: number,
  threshold: number,
  playhead: number | null,
  exclude: number[],
): Promise<number | null> {
  return invoke<number | null>("engine_snap_time", {
    t,
    threshold,
    playhead,
    exclude,
  });
}

/** Snap a clip-move gesture (both clip edges compete for candidates). */
export function engineSnapClipMove(
  clipId: number,
  desiredIn: number,
  threshold: number,
  playhead: number | null,
): Promise<SnappedMove> {
  return invoke<SnappedMove>("engine_snap_clip_move", {
    clipId,
    desiredIn,
    threshold,
    playhead,
  });
}
