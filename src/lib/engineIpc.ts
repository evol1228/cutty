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

/** Per-track toggle switches (mirrors the engine's TrackFlag). */
export type TrackFlag = "locked" | "muted" | "hidden";

export const BLEND_MODES = [
  "normal",
  "multiply",
  "screen",
  "overlay",
  "add",
] as const;
export type BlendMode = (typeof BLEND_MODES)[number];

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
  blendMode: BlendMode;
  speed: number;
  volume: number;
}

export interface Track {
  id: number;
  kind: TrackKind;
  name: string;
  /** Rejects edits (the engine enforces; the UI also pre-checks). */
  locked: boolean;
  /** Audio silenced (audio tracks and video clips' embedded audio). */
  muted: boolean;
  /** Video track excluded from the composite (preview and export). */
  hidden: boolean;
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

/** Set a clip's 2D placement (position/scale/rotation). */
export function engineSetClipTransform(
  clipId: number,
  transform: Transform,
): Promise<void> {
  return invoke("engine_set_clip_transform", { clipId, transform });
}

/** Set a clip's opacity (0.0 transparent .. 1.0 opaque). */
export function engineSetClipOpacity(
  clipId: number,
  opacity: number,
): Promise<void> {
  return invoke("engine_set_clip_opacity", { clipId, opacity });
}

/** Set how a clip blends with the layers below it. */
export function engineSetClipBlendMode(
  clipId: number,
  mode: BlendMode,
): Promise<void> {
  return invoke("engine_set_clip_blend_mode", { clipId, mode });
}

/** Move a clip onto another track (and position) in one step. */
export function engineMoveClipToTrack(
  clipId: number,
  trackId: number,
  timelineIn: number,
): Promise<void> {
  return invoke("engine_move_clip_to_track", { clipId, trackId, timelineIn });
}

/** Insert a new empty track at panel index (0 = top; engine clamps).
 * Returns the new track's id. */
export function engineAddTrack(
  kind: TrackKind,
  index: number,
): Promise<number> {
  return invoke<number>("engine_add_track", { kind, index });
}

/** Remove a track with all its clips (one undo step). */
export function engineRemoveTrack(trackId: number): Promise<void> {
  return invoke("engine_remove_track", { trackId });
}

/** Move a track to another panel position (restacks the composite). */
export function engineMoveTrack(trackId: number, to: number): Promise<void> {
  return invoke("engine_move_track", { trackId, to });
}

/** Flip a per-track flag (lock / mute / hide). */
export function engineSetTrackFlag(
  trackId: number,
  flag: TrackFlag,
  value: boolean,
): Promise<void> {
  return invoke("engine_set_track_flag", { trackId, flag, value });
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
