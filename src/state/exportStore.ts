// Export job state: dialog visibility, live progress mirrored from
// export:// events, and the finished/failed outcome. The Rust side owns
// the job — this store only reflects it.

import { create } from "zustand";
import {
  isPermissionGranted,
  requestPermission,
  sendNotification,
} from "@tauri-apps/plugin-notification";
import * as ipc from "../lib/exportIpc";
import type { EncoderInfo, ExportStage } from "../lib/exportIpc";
import { toast } from "./toastStore";

export type ExportPhase = "idle" | "running" | "done" | "error";

interface ExportState {
  dialogOpen: boolean;
  phase: ExportPhase;
  stage: ExportStage | null;
  percent: number;
  etaSec: number | null;
  /** Encode speed as a multiple of realtime (0 = unknown). */
  speed: number;
  /** Detection result; null until the first fetch resolves. */
  encoder: EncoderInfo | null;
  /** Finished output path (done phase). */
  outputPath: string | null;
  /** Encoder the finished export actually used. */
  encoderUsed: string | null;
  error: string | null;

  openDialog: () => void;
  closeDialog: () => void;
  /** Reset a done/error outcome back to the setup form. */
  resetOutcome: () => void;
  markStarted: () => void;
}

export const useExportStore = create<ExportState>((set) => ({
  dialogOpen: false,
  phase: "idle",
  stage: null,
  percent: 0,
  etaSec: null,
  speed: 0,
  encoder: null,
  outputPath: null,
  encoderUsed: null,
  error: null,

  openDialog: () => {
    set({ dialogOpen: true });
    // (Re-)fetch the encoder lazily; detection is warmed at startup so
    // this resolves instantly in practice.
    void ipc
      .exportDetectEncoder()
      .then((encoder) => useExportStore.setState({ encoder }))
      .catch(() => undefined);
  },
  closeDialog: () => set({ dialogOpen: false }),
  resetOutcome: () =>
    set({ phase: "idle", error: null, outputPath: null, encoderUsed: null }),
  markStarted: () =>
    set({
      phase: "running",
      stage: "audio",
      percent: 0,
      etaSec: null,
      speed: 0,
      error: null,
      outputPath: null,
      encoderUsed: null,
    }),
}));

function basename(path: string): string {
  return path.split("/").pop() ?? path;
}

/** Desktop notification for exports finishing while Cutty is unfocused. */
async function notify(title: string, body: string): Promise<void> {
  if (document.hasFocus()) return;
  try {
    let granted = await isPermissionGranted();
    if (!granted) {
      granted = (await requestPermission()) === "granted";
    }
    if (granted) sendNotification({ title, body });
  } catch {
    // Notifications are best-effort (no daemon on this desktop, etc.).
  }
}

let started = false;

/** Wire export events into the store. Idempotent; call once at startup. */
export function startExportSync(): void {
  if (started) return;
  started = true;

  void ipc.onExportProgress((p) => {
    useExportStore.setState({
      phase: "running",
      stage: p.stage,
      percent: p.percent,
      etaSec: p.etaSec,
      speed: p.speed,
    });
  });

  void ipc.onExportDone((e) => {
    useExportStore.setState({
      phase: "done",
      percent: 100,
      etaSec: null,
      outputPath: e.path,
      encoderUsed: e.encoder,
    });
    toast(`Export finished: ${basename(e.path)}`);
    void notify("Export finished", basename(e.path));
  });

  void ipc.onExportError((e) => {
    useExportStore.setState({ phase: "error", error: e.message });
    toast(`Export failed: ${e.message}`, "error");
    void notify("Export failed", e.message);
  });

  void ipc.onExportCancelled(() => {
    useExportStore.setState({
      phase: "idle",
      stage: null,
      percent: 0,
      etaSec: null,
      speed: 0,
    });
    toast("Export cancelled");
  });
}
