// Typed wrappers around the export IPC surface (mirrors the Rust types
// in src-tauri/src/export_ipc.rs, serde camelCase). The render pipeline
// lives entirely in Rust — the frontend starts/cancels jobs and renders
// the event stream.

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type ExportQuality = "high" | "medium" | "small";
export type ExportStage = "audio" | "video" | "finalize";

export interface ExportRequest {
  width: number;
  height: number;
  fps: number;
  quality: ExportQuality;
  dstPath: string;
}

export interface EncoderInfo {
  /** ffmpeg encoder name, e.g. "h264_vaapi". */
  encoder: string;
  /** Human-readable label for the dialog. */
  label: string;
  hardware: boolean;
}

export interface ExportProgressEvent {
  stage: ExportStage;
  percent: number;
  etaSec: number | null;
  speed: number;
}

export interface ExportDoneEvent {
  path: string;
  durationSec: number;
  encoder: string;
  hardwareEncode: boolean;
}

export interface ExportErrorEvent {
  message: string;
}

export const EXPORT_PROGRESS_EVENT = "export://progress";
export const EXPORT_DONE_EVENT = "export://done";
export const EXPORT_ERROR_EVENT = "export://error";
export const EXPORT_CANCELLED_EVENT = "export://cancelled";

/** Which H.264 encoder exports will use (cached detection). */
export function exportDetectEncoder(): Promise<EncoderInfo> {
  return invoke<EncoderInfo>("export_detect_encoder");
}

/** Start exporting the current project (fails if one is running). */
export function exportStart(request: ExportRequest): Promise<void> {
  return invoke("export_start", { request });
}

/** Cancel the running export (no-op when idle). */
export function exportCancel(): Promise<void> {
  return invoke("export_cancel");
}

export function onExportProgress(
  handler: (e: ExportProgressEvent) => void,
): Promise<UnlistenFn> {
  return listen<ExportProgressEvent>(EXPORT_PROGRESS_EVENT, (e) =>
    handler(e.payload),
  );
}

export function onExportDone(
  handler: (e: ExportDoneEvent) => void,
): Promise<UnlistenFn> {
  return listen<ExportDoneEvent>(EXPORT_DONE_EVENT, (e) => handler(e.payload));
}

export function onExportError(
  handler: (e: ExportErrorEvent) => void,
): Promise<UnlistenFn> {
  return listen<ExportErrorEvent>(EXPORT_ERROR_EVENT, (e) =>
    handler(e.payload),
  );
}

export function onExportCancelled(handler: () => void): Promise<UnlistenFn> {
  return listen(EXPORT_CANCELLED_EVENT, () => handler());
}
