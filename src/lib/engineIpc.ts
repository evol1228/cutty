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

export type MediaKind = "video" | "audio" | "image" | "gif";

export interface MediaRef {
  id: number;
  path: string;
  /** Seconds; 0 exactly on still images. */
  duration: number;
  hasVideo: boolean;
  hasAudio: boolean;
  hasAlpha: boolean;
  kind: MediaKind;
}

export type TrackKind = "video" | "audio" | "text";

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

/** Transition bound to a clip's out cut (kind = shader registry id). */
export interface Transition {
  kind: string;
  duration: number;
}

export type TextAlign = "left" | "center" | "right";

/** Visual style of a text clip. Pixel quantities are project pixels at
 * transform scale 1; colors are #RRGGBB or #RRGGBBAA. */
export interface TextStyle {
  fontFamily: string;
  /** 100–900 (400 regular, 700 bold). */
  weight: number;
  fontSize: number;
  fill: string;
  strokeColor: string;
  strokeWidth: number;
  shadowColor: string;
  shadowOffsetX: number;
  shadowOffsetY: number;
  /** 0..1; 0 disables the shadow. */
  shadowAlpha: number;
  align: TextAlign;
}

/** Styled text payload of a clip on a text track. */
export interface TextSpec {
  content: string;
  style: TextStyle;
}

/** Easing of the segment from a keyframe to the next. */
export type Easing = "linear" | "easeIn" | "easeOut" | "easeInOut";

/** One keyframe on a clip property lane. `t` is clip-relative seconds
 * (from the clip's timelineIn) — automation moves with the clip. */
export interface Keyframe {
  t: number;
  value: number;
  easing: Easing;
}

/** Keyframable clip property (volume now; transform/opacity in Phase 3).
 * Volume keyframes are a gain *multiplier* on top of the clip's static
 * `volume`. */
export type KeyframeProp = "volume";

/** Which clip edge a fade handle drags. */
export type FadeSide = "in" | "out";

export interface Clip {
  id: number;
  /** Absent exactly on text clips (which carry `text` instead). */
  mediaId?: number;
  timelineIn: number;
  timelineOut: number;
  sourceIn: number;
  sourceOut: number;
  transform: Transform;
  opacity: number;
  blendMode: BlendMode;
  speed: number;
  volume: number;
  /** Transition into the next clip on the same track (absent = none). */
  transitionOut?: Transition | null;
  /** Present exactly on text-track clips. */
  text?: TextSpec | null;
  /** Keyframe lanes by property; absent = no animation. Lanes are
   * sorted by `t` and never empty (engine invariants). */
  keyframes?: Partial<Record<KeyframeProp, Keyframe[]>>;
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

/** A transition resolved to its effective span (engine-computed — the
 * chip renders exactly here; all clamping is engine-side). */
export interface TransitionSpan {
  trackId: number;
  fromClipId: number;
  toClipId: number;
  kind: string;
  /** The cut instant the transition is centered on. */
  cut: number;
  /** Effective span, seconds. */
  start: number;
  end: number;
  /** Stored (requested) duration, seconds. */
  requested: number;
  /** Longest duration this cut currently supports (drag clamp). */
  maxDuration: number;
}

export interface EngineSnapshot {
  project: Project;
  transitions: TransitionSpan[];
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
  hasAlpha: boolean,
  kind: MediaKind,
): Promise<number> {
  return invoke<number>("engine_add_media", {
    path,
    duration,
    hasVideo,
    hasAudio,
    hasAlpha,
    kind,
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

/** Place a new text clip. `trackId: null` = CapCut placement: topmost
 * text lane with room, else a new lane on top (one undo step). Returns
 * the new clip's id. */
export function engineAddTextClip(
  timelineIn: number,
  duration: number,
  text: TextSpec,
  transform?: Transform,
  trackId?: number,
): Promise<number> {
  return invoke<number>("engine_add_text_clip", {
    timelineIn,
    duration,
    text,
    transform: transform ?? null,
    trackId: trackId ?? null,
  });
}

/** Replace a text clip's content and/or style (equal payloads no-op). */
export function engineSetClipText(
  clipId: number,
  text: TextSpec,
): Promise<void> {
  return invoke("engine_set_clip_text", { clipId, text });
}

/** Distinct system font families, sorted. First call loads the font
 * database backend-side; cache the promise. */
export function textFontFamilies(): Promise<string[]> {
  return invoke<string[]>("text_font_families");
}

let fontFamiliesCache: Promise<string[]> | null = null;

/** [`textFontFamilies`], fetched once and shared. */
export function cachedFontFamilies(): Promise<string[]> {
  fontFamiliesCache ??= textFontFamilies();
  return fontFamiliesCache;
}

/** Measure a text block in project pixels at scale 1 (gizmo box size). */
export function textMeasure(text: TextSpec): Promise<[number, number]> {
  return invoke<[number, number]>("text_measure", { text });
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

/** Add a keyframe (or replace the one at that time). `t` is
 * clip-relative seconds; returns where it landed after clamping. */
export function engineAddKeyframe(
  clipId: number,
  prop: KeyframeProp,
  t: number,
  value: number,
  easing?: Easing,
): Promise<number> {
  return invoke<number>("engine_add_keyframe", {
    clipId,
    prop,
    t,
    value,
    easing: easing ?? null,
  });
}

/** Move the keyframe at `fromT` to `toT` with a new value (a dot drag
 * moves both axes). Returns the applied time after neighbor clamping —
 * feed it back as the next step's `fromT`. */
export function engineMoveKeyframe(
  clipId: number,
  prop: KeyframeProp,
  fromT: number,
  toT: number,
  value: number,
): Promise<number> {
  return invoke<number>("engine_move_keyframe", {
    clipId,
    prop,
    fromT,
    toT,
    value,
  });
}

/** Remove the keyframe at clip-relative time `t`. */
export function engineRemoveKeyframe(
  clipId: number,
  prop: KeyframeProp,
  t: number,
): Promise<void> {
  return invoke("engine_remove_keyframe", { clipId, prop, t });
}

/** Set a fade-in/out duration in seconds (0 removes it) — sugar over
 * the volume keyframe lane. Returns the applied (clamped) duration. */
export function engineSetClipFade(
  clipId: number,
  side: FadeSide,
  duration: number,
): Promise<number> {
  return invoke<number>("engine_set_clip_fade", { clipId, side, duration });
}

/** Extract a video clip's audio onto an audio track (one undo step);
 * the video clip's own volume drops to 0. Returns the new clip's id. */
export function engineExtractAudio(clipId: number): Promise<number> {
  return invoke<number>("engine_extract_audio", { clipId });
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

/** One transition shader available in the library panel. */
export interface TransitionDef {
  id: string;
  label: string;
  defaultDuration: number;
}

/** The transition catalog (static per build; cache the result). */
export function transitionList(): Promise<TransitionDef[]> {
  return invoke<TransitionDef[]>("transition_list");
}

let transitionCatalogCache: Promise<TransitionDef[]> | null = null;

/** [`transitionList`], fetched once and shared. */
export function cachedTransitionList(): Promise<TransitionDef[]> {
  transitionCatalogCache ??= transitionList();
  return transitionCatalogCache;
}

/** Set, replace, or remove (null) the transition at a clip's out cut.
 * Returns the stored duration after engine-side clamping. */
export function engineSetTransition(
  clipId: number,
  transition: Transition | null,
): Promise<number | null> {
  return invoke<number | null>("engine_set_transition", {
    clipId,
    transition,
  });
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
