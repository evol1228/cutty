// The export dialog: preset + quality + destination on top of the Rust
// render pipeline. The job runs fully in the background — closing the
// dialog (or the "Hide" button) keeps it running; the TopBar button shows
// live progress and reopens this dialog.

import { useEffect, useState } from "react";
import { save as saveDialog } from "@tauri-apps/plugin-dialog";
import { homeDir, join, videoDir } from "@tauri-apps/api/path";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { exportCancel, exportStart } from "../lib/exportIpc";
import type { ExportQuality, ExportStage } from "../lib/exportIpc";
import { useExportStore } from "../state/exportStore";
import { useProjectStore } from "../state/projectStore";
import { useSessionStore } from "../state/sessionStore";
import { toast } from "../state/toastStore";

export interface ExportPreset {
  id: string;
  label: string;
  width: number;
  height: number;
  fps: number;
}

export const EXPORT_PRESETS: ExportPreset[] = [
  { id: "1080p30", label: "1080p · 30 fps", width: 1920, height: 1080, fps: 30 },
  { id: "1080p60", label: "1080p · 60 fps", width: 1920, height: 1080, fps: 60 },
  { id: "4k30", label: "4K · 30 fps", width: 3840, height: 2160, fps: 30 },
  {
    id: "shorts30",
    label: "TikTok / Shorts (9:16) · 30 fps",
    width: 1080,
    height: 1920,
    fps: 30,
  },
  {
    id: "shorts60",
    label: "TikTok / Shorts (9:16) · 60 fps",
    width: 1080,
    height: 1920,
    fps: 60,
  },
];

const QUALITIES: { id: ExportQuality; label: string; hint: string }[] = [
  { id: "high", label: "High", hint: "best quality, larger file" },
  { id: "medium", label: "Medium", hint: "balanced" },
  { id: "small", label: "Small file", hint: "smallest, visibly compressed" },
];

const STAGE_LABEL: Record<ExportStage, string> = {
  audio: "Rendering audio mix",
  video: "Encoding video",
  finalize: "Finalizing file",
};

/** Dark-themed select: WebKitGTK's native `<select>` chrome ignores our
 * colors (white pill, invisible text), so reset appearance and draw our
 * own chevron. */
function SelectField({
  id,
  value,
  onChange,
  options,
}: {
  id: string;
  value: string;
  onChange: (value: string) => void;
  options: { value: string; label: string }[];
}) {
  return (
    <div className="relative">
      <select
        id={id}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        className="w-full appearance-none rounded-md border border-zinc-700 bg-zinc-800 py-1.5 pl-2 pr-8 text-sm text-zinc-100"
      >
        {options.map((o) => (
          <option key={o.value} value={o.value}>
            {o.label}
          </option>
        ))}
      </select>
      <span className="pointer-events-none absolute inset-y-0 right-2.5 flex items-center text-xs text-zinc-500">
        ▾
      </span>
    </div>
  );
}

function fmtEta(sec: number): string {
  const s = Math.max(0, Math.round(sec));
  if (s < 60) return `${s}s`;
  return `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, "0")}s`;
}

/** Default output file: ~/Videos/<project-name>.mp4. */
async function defaultDstPath(projectName: string): Promise<string> {
  const safe = projectName.replace(/[/\\]/g, "-") || "export";
  try {
    return await join(await videoDir(), `${safe}.mp4`);
  } catch {
    // No XDG videos dir configured — fall back to ~/Videos.
    return join(await homeDir(), "Videos", `${safe}.mp4`);
  }
}

function SetupForm({ onClose }: { onClose: () => void }) {
  const encoder = useExportStore((s) => s.encoder);
  const projectName = useSessionStore((s) => s.meta.name);
  const project = useProjectStore((s) => s.project);
  const timelineEmpty =
    project === null || project.tracks.every((t) => t.clips.length === 0);

  // Default to the Shorts preset for portrait projects.
  const portrait =
    project !== null && project.settings.height > project.settings.width;
  const [presetId, setPresetId] = useState(portrait ? "shorts30" : "1080p30");
  const [quality, setQuality] = useState<ExportQuality>("high");
  const [dstPath, setDstPath] = useState<string | null>(null);

  useEffect(() => {
    if (dstPath === null) {
      void defaultDstPath(projectName).then((p) => setDstPath(p));
    }
  }, [dstPath, projectName]);

  const preset =
    EXPORT_PRESETS.find((p) => p.id === presetId) ?? EXPORT_PRESETS[0];

  const browse = async () => {
    const picked = await saveDialog({
      title: "Export video",
      filters: [{ name: "MP4 video", extensions: ["mp4"] }],
      defaultPath: dstPath ?? undefined,
    });
    if (picked !== null) {
      setDstPath(picked.endsWith(".mp4") ? picked : `${picked}.mp4`);
    }
  };

  const start = async () => {
    if (dstPath === null) return;
    try {
      useExportStore.getState().markStarted();
      await exportStart({
        width: preset.width,
        height: preset.height,
        fps: preset.fps,
        quality,
        dstPath,
      });
    } catch (err) {
      useExportStore.getState().resetOutcome();
      toast(`Could not start the export: ${String(err)}`, "error");
    }
  };

  return (
    <>
      <h2 className="mb-4 font-semibold text-zinc-100">Export</h2>
      <div className="mb-4 grid grid-cols-[auto_1fr] items-center gap-x-3 gap-y-3">
        <label htmlFor="export-preset" className="text-sm text-zinc-400">
          Preset
        </label>
        <SelectField
          id="export-preset"
          value={presetId}
          onChange={setPresetId}
          options={EXPORT_PRESETS.map((p) => ({
            value: p.id,
            label: `${p.label} — ${p.width}×${p.height}`,
          }))}
        />

        <label htmlFor="export-quality" className="text-sm text-zinc-400">
          Quality
        </label>
        <SelectField
          id="export-quality"
          value={quality}
          onChange={(v) => setQuality(v as ExportQuality)}
          options={QUALITIES.map((q) => ({
            value: q.id,
            label: `${q.label} — ${q.hint}`,
          }))}
        />

        <span className="text-sm text-zinc-400">Save to</span>
        <div className="flex min-w-0 items-center gap-2">
          <span
            className="min-w-0 flex-1 truncate rounded-md border border-zinc-700 bg-zinc-800/60 px-2 py-1.5 text-sm text-zinc-300"
            title={dstPath ?? ""}
          >
            {dstPath ?? "…"}
          </span>
          <button
            onClick={() => void browse()}
            className="shrink-0 rounded-md border border-zinc-700 px-2.5 py-1.5 text-sm text-zinc-300 hover:bg-zinc-800"
          >
            Browse…
          </button>
        </div>
      </div>

      <p className="mb-5 text-xs text-zinc-500">
        MP4 (H.264 + AAC) · encoder:{" "}
        <span className={encoder?.hardware ? "text-emerald-400" : ""}>
          {encoder?.label ?? "detecting…"}
        </span>
      </p>

      <div className="flex items-center justify-end gap-2">
        {timelineEmpty && (
          <span className="mr-auto text-xs text-amber-400">
            The timeline is empty — add clips first.
          </span>
        )}
        <button
          onClick={onClose}
          className="rounded-md px-3 py-1.5 text-sm text-zinc-300 hover:bg-zinc-800"
        >
          Close
        </button>
        <button
          autoFocus
          disabled={timelineEmpty || dstPath === null}
          onClick={() => void start()}
          className="rounded-md bg-sky-600 px-4 py-1.5 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-40"
        >
          Export
        </button>
      </div>
    </>
  );
}

function RunningView({ onHide }: { onHide: () => void }) {
  const stage = useExportStore((s) => s.stage);
  const percent = useExportStore((s) => s.percent);
  const etaSec = useExportStore((s) => s.etaSec);
  const speed = useExportStore((s) => s.speed);

  return (
    <>
      <h2 className="mb-4 font-semibold text-zinc-100">Exporting…</h2>
      <div className="mb-2 h-2 overflow-hidden rounded-full bg-zinc-800">
        <div
          className="h-full rounded-full bg-sky-500 transition-[width] duration-300"
          style={{ width: `${percent}%` }}
        />
      </div>
      <div className="mb-5 flex justify-between text-xs text-zinc-400">
        <span>
          {stage ? STAGE_LABEL[stage] : "Starting"} · {percent.toFixed(0)}%
          {speed > 0 && ` · ${speed.toFixed(1)}× realtime`}
        </span>
        <span>{etaSec !== null ? `about ${fmtEta(etaSec)} left` : "…"}</span>
      </div>
      <div className="flex justify-end gap-2">
        <button
          onClick={onHide}
          className="rounded-md px-3 py-1.5 text-sm text-zinc-300 hover:bg-zinc-800"
          title="The export keeps running in the background"
        >
          Hide
        </button>
        <button
          onClick={() => void exportCancel()}
          className="rounded-md px-3 py-1.5 text-sm text-red-400 hover:bg-zinc-800"
        >
          Cancel export
        </button>
      </div>
    </>
  );
}

function DoneView({ onClose }: { onClose: () => void }) {
  const outputPath = useExportStore((s) => s.outputPath);
  const encoderUsed = useExportStore((s) => s.encoderUsed);

  return (
    <>
      <h2 className="mb-2 font-semibold text-zinc-100">
        <span className="mr-2 text-emerald-400">✓</span>Export finished
      </h2>
      <p
        className="mb-1 truncate text-sm text-zinc-300"
        title={outputPath ?? ""}
      >
        {outputPath}
      </p>
      <p className="mb-5 text-xs text-zinc-500">encoded with {encoderUsed}</p>
      <div className="flex justify-end gap-2">
        <button
          onClick={onClose}
          className="rounded-md px-3 py-1.5 text-sm text-zinc-300 hover:bg-zinc-800"
        >
          Close
        </button>
        <button
          autoFocus
          onClick={() => {
            if (outputPath !== null) {
              void revealItemInDir(outputPath).catch((err) =>
                toast(`Could not open the folder: ${String(err)}`, "error"),
              );
            }
          }}
          className="rounded-md bg-sky-600 px-4 py-1.5 text-sm font-medium text-white hover:bg-sky-500"
        >
          Show in folder
        </button>
      </div>
    </>
  );
}

function ErrorView({ onClose }: { onClose: () => void }) {
  const error = useExportStore((s) => s.error);
  return (
    <>
      <h2 className="mb-2 font-semibold text-red-400">Export failed</h2>
      <p className="mb-5 break-words text-sm text-zinc-300">{error}</p>
      <div className="flex justify-end gap-2">
        <button
          autoFocus
          onClick={onClose}
          className="rounded-md px-3 py-1.5 text-sm text-zinc-300 hover:bg-zinc-800"
        >
          Close
        </button>
        <button
          onClick={() => useExportStore.getState().resetOutcome()}
          className="rounded-md bg-sky-600 px-4 py-1.5 text-sm font-medium text-white hover:bg-sky-500"
        >
          Try again
        </button>
      </div>
    </>
  );
}

function ExportDialog() {
  const open = useExportStore((s) => s.dialogOpen);
  const phase = useExportStore((s) => s.phase);
  const close = useExportStore((s) => s.closeDialog);

  // Escape hides the dialog (never cancels a running export implicitly).
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        close();
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [open, close]);

  if (!open) return null;

  const closeAndReset = () => {
    close();
    if (phase === "done" || phase === "error") {
      useExportStore.getState().resetOutcome();
    }
  };

  return (
    <div className="fixed inset-0 z-[80] flex items-center justify-center bg-black/60">
      <div className="w-[30rem] rounded-lg border border-zinc-700 bg-zinc-900 p-5 shadow-2xl shadow-black/60">
        {phase === "running" && <RunningView onHide={close} />}
        {phase === "done" && <DoneView onClose={closeAndReset} />}
        {phase === "error" && <ErrorView onClose={closeAndReset} />}
        {phase === "idle" && <SetupForm onClose={close} />}
      </div>
    </div>
  );
}

export default ExportDialog;
