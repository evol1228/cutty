// Typed wrappers around the Tauri IPC surface. These mirror the Rust types
// in cutty-media (serde camelCase).

import { Channel, invoke } from "@tauri-apps/api/core";

export interface VideoStreamInfo {
  codec: string;
  width: number;
  height: number;
  fps: number;
  /** Display-matrix rotation in degrees; 0 when absent. */
  rotation: number;
  /** The stream carries an alpha channel (PNG/GIF transparency, WebM
   * alpha_mode). Alpha media skips the proxy and decodes originals. */
  hasAlpha: boolean;
}

export interface AudioStreamInfo {
  codec: string;
  sampleRate: number;
  channels: number;
}

export interface StreamSummary {
  index: number;
  kind: string;
  codec: string;
}

/** Timeline semantics of a media file (mirrors cutty-engine's MediaKind). */
export type MediaKind = "video" | "audio" | "image" | "gif";

export interface MediaInfo {
  path: string;
  /** Container duration in seconds; 0 for still images. */
  durationSec: number;
  container: string;
  sizeBytes: number;
  kind: MediaKind;
  video: VideoStreamInfo | null;
  audio: AudioStreamInfo | null;
  streams: StreamSummary[];
}

export interface ProxyProgressEvent {
  srcPath: string;
  percent: number;
  outTimeSec: number;
  speed: number;
}

export const PROXY_PROGRESS_EVENT = "proxy://progress";

export function probeMedia(path: string): Promise<MediaInfo> {
  return invoke<MediaInfo>("probe_media", { path });
}

/** Packed audio peak data for a media file (see cutty_media::peaks for
 * the binary layout). Generates on first call, then serves the cache. */
export function mediaPeaks(path: string): Promise<ArrayBuffer> {
  return invoke<ArrayBuffer>("media_peaks", { path });
}

/** Packed filmstrip sprite for a media file (see cutty_media::filmstrip
 * for the binary layout). Generates on first call, then serves the cache. */
export function mediaFilmstrip(
  path: string,
  durationHint?: number,
): Promise<ArrayBuffer> {
  return invoke<ArrayBuffer>("media_filmstrip", { path, durationHint });
}

export function generateProxy(
  path: string,
  durationHint?: number,
): Promise<string> {
  return invoke<string>("generate_proxy", { path, durationHint });
}

/** Generate (or fetch cached) a media thumbnail; resolves with JPEG bytes. */
export function mediaThumbnail(
  path: string,
  durationHint?: number,
): Promise<ArrayBuffer> {
  return invoke<ArrayBuffer>("media_thumbnail", { path, durationHint });
}

/** Which of the given source paths currently exist on disk. */
export function pathsExist(paths: string[]): Promise<boolean[]> {
  return invoke<boolean[]>("paths_exist", { paths });
}

// --- Timeline playback ---
//
// The Rust playback engine owns the clock and everything that moves: the
// frontend attaches one binary frame channel, sends transport commands,
// and renders position events. Timeline time is the only time.

export interface PositionEvent {
  positionSec: number;
  playing: boolean;
}

export const PLAYER_POSITION_EVENT = "player://position";
export const PLAYER_EOF_EVENT = "player://eof";
export const PLAYER_ERROR_EVENT = "player://error";

export interface FrameMessage {
  /** Timeline presentation time, seconds. */
  ptsSec: number;
  width: number;
  height: number;
  jpeg: Blob;
}

/** Binary frame layout (little-endian): [f64 pts][u32 w][u32 h][jpeg…]. */
function parseFrameMessage(buf: ArrayBuffer): FrameMessage {
  const view = new DataView(buf);
  return {
    ptsSec: view.getFloat64(0, true),
    width: view.getUint32(8, true),
    height: view.getUint32(12, true),
    jpeg: new Blob([buf.slice(16)], { type: "image/jpeg" }),
  };
}

/** Start (or restart) the playback engine, streaming frames to `onFrame`. */
export function attachPlayback(
  onFrame: (frame: FrameMessage) => void,
): Promise<void> {
  const channel = new Channel<ArrayBuffer>();
  channel.onmessage = (buf) => onFrame(parseFrameMessage(buf));
  return invoke("playback_attach", { onFrame: channel });
}

export function playbackToggle(): Promise<void> {
  return invoke("playback_toggle");
}

export function playbackPlay(): Promise<void> {
  return invoke("playback_play");
}

export function playbackPause(): Promise<void> {
  return invoke("playback_pause");
}

/** Seek/scrub to an absolute timeline position (paused: shows the frame). */
export function playbackSeek(positionSec: number): Promise<void> {
  return invoke("playback_seek", { positionSec });
}

/** Step by `delta` project frames (negative = backwards). Pauses. */
export function playbackStep(delta: number): Promise<void> {
  return invoke("playback_step", { delta });
}

// --- Export (Phase 0 spike; the export dialog prompt replaces this) ---

export interface ExportResult {
  path: string;
  /** The keyframe the cut actually starts on (≤ requested in point). */
  actualStartSec: number;
  durationSec: number;
}

export function exportTrim(
  srcPath: string,
  dstPath: string,
  inSec: number,
  outSec: number,
): Promise<ExportResult> {
  return invoke<ExportResult>("export_trim", {
    srcPath,
    dstPath,
    inSec,
    outSec,
  });
}
