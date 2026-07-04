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

export interface MediaInfo {
  path: string;
  durationSec: number;
  container: string;
  sizeBytes: number;
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

export function generateProxy(
  path: string,
  durationHint?: number,
): Promise<string> {
  return invoke<string>("generate_proxy", { path, durationHint });
}

// --- Player ---

export interface PlayerInfo {
  width: number;
  height: number;
  fps: number;
  durationSec: number;
  hasAudio: boolean;
}

export interface PositionEvent {
  positionSec: number;
  playing: boolean;
}

export const PLAYER_POSITION_EVENT = "player://position";
export const PLAYER_EOF_EVENT = "player://eof";
export const PLAYER_ERROR_EVENT = "player://error";

export interface FrameMessage {
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

export function openPlayer(
  path: string,
  onFrame: (frame: FrameMessage) => void,
): Promise<PlayerInfo> {
  const channel = new Channel<ArrayBuffer>();
  channel.onmessage = (buf) => onFrame(parseFrameMessage(buf));
  return invoke<PlayerInfo>("open_player", { path, onFrame: channel });
}

export function closePlayer(): Promise<void> {
  return invoke("close_player");
}

export function playerToggle(): Promise<void> {
  return invoke("player_toggle");
}

export function playerSeek(positionSec: number): Promise<void> {
  return invoke("player_seek", { positionSec });
}

export function playerStep(delta: number): Promise<void> {
  return invoke("player_step", { delta });
}

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
