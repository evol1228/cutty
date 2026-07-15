// Right-hand inspector. Video tab (transform / opacity / blend) and Audio
// tab (per-clip volume) are live; the rest are Phase 3+ placeholders.
// Every slider/scrub drag is wrapped in an engine transaction so a whole
// gesture is exactly one undo entry — the same contract as timeline drags.

import { useRef, useState } from "react";
import {
  engineBeginTransaction,
  engineCommitTransaction,
  engineSetClipBlendMode,
  engineSetClipOpacity,
  engineSetClipTransform,
  engineSetClipVolume,
  BLEND_MODES,
  type BlendMode,
  type Clip,
  type Track,
  type Transform,
} from "../lib/engineIpc";
import { useProjectStore } from "../state/projectStore";
import { toast } from "../state/toastStore";
import SelectField from "./ui/SelectField";

const TABS = ["Video", "Audio", "Speed", "Animation", "Adjustment"] as const;
type Tab = (typeof TABS)[number];

const BLEND_LABELS: Record<BlendMode, string> = {
  normal: "Normal",
  multiply: "Multiply",
  screen: "Screen",
  overlay: "Overlay",
  add: "Add",
};

/** The single selected clip with its track, or null (none / multi-select). */
function useSelectedClip(): { clip: Clip; track: Track } | null {
  const project = useProjectStore((s) => s.project);
  const selection = useProjectStore((s) => s.selection);
  if (project === null || selection.length !== 1) return null;
  for (const track of project.tracks) {
    const clip = track.clips.find((c) => c.id === selection[0]);
    if (clip) return { clip, track };
  }
  return null;
}

/** One engine transaction per pointer gesture: begin on the first
 * pointerdown, commit on release. Shared by every Inspector control. */
function useDragTransaction() {
  const inGesture = useRef(false);
  const begin = () => {
    if (inGesture.current) return;
    inGesture.current = true;
    void engineBeginTransaction().catch(() => {
      inGesture.current = false;
    });
  };
  const end = () => {
    if (!inGesture.current) return;
    inGesture.current = false;
    void engineCommitTransaction().catch((err) =>
      toast(`Change failed: ${String(err)}`, "error"),
    );
  };
  return { begin, end };
}

/**
 * A CapCut-style scrubbable number: drag horizontally to adjust (one
 * transaction = one undo entry), or type an exact value (a single
 * command = its own undo entry).
 */
function DragNumber({
  label,
  value,
  sensitivity,
  decimals,
  suffix,
  min,
  max,
  commit,
}: {
  label: string;
  value: number;
  /** Value change per horizontal pixel dragged. */
  sensitivity: number;
  decimals: number;
  suffix: string;
  min?: number;
  max?: number;
  /** Send the value to the engine (transient inside a drag). */
  commit: (value: number) => void;
}) {
  const gesture = useDragTransaction();
  const drag = useRef<{ pointerId: number; startX: number; start: number } | null>(
    null,
  );
  const moved = useRef(false);
  const [dragValue, setDragValue] = useState<number | null>(null);
  const [text, setText] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  const clamp = (v: number) =>
    Math.min(max ?? Infinity, Math.max(min ?? -Infinity, v));
  const shown = dragValue ?? value;

  const commitTyped = () => {
    if (text === null) return;
    const parsed = Number(text);
    setText(null);
    if (Number.isFinite(parsed) && parsed !== value) {
      commit(clamp(parsed));
    }
  };

  return (
    <label className="block">
      <span
        className="mb-1 block cursor-ew-resize select-none text-[11px] text-zinc-500"
        onPointerDown={(e) => {
          e.preventDefault();
          drag.current = { pointerId: e.pointerId, startX: e.clientX, start: value };
          moved.current = false;
          e.currentTarget.setPointerCapture(e.pointerId);
        }}
        onPointerMove={(e) => {
          if (!drag.current || e.pointerId !== drag.current.pointerId) return;
          if (!moved.current) {
            moved.current = true;
            gesture.begin();
          }
          const v = clamp(
            drag.current.start + (e.clientX - drag.current.startX) * sensitivity,
          );
          setDragValue(v);
          commit(v);
        }}
        onPointerUp={() => {
          if (moved.current) gesture.end();
          drag.current = null;
          setDragValue(null);
        }}
      >
        {label}
      </span>
      <div className="flex items-center rounded-md border border-zinc-700 bg-zinc-800 focus-within:border-zinc-500">
        <input
          ref={inputRef}
          className="w-full min-w-0 bg-transparent px-2 py-1 text-right text-sm tabular-nums text-zinc-100 outline-none"
          value={text ?? shown.toFixed(decimals)}
          onFocus={(e) => {
            setText(shown.toFixed(decimals));
            e.currentTarget.select();
          }}
          onChange={(e) => setText(e.target.value)}
          onBlur={commitTyped}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              commitTyped();
              inputRef.current?.blur();
            } else if (e.key === "Escape") {
              setText(null);
              inputRef.current?.blur();
            }
            e.stopPropagation();
          }}
        />
        <span className="pr-2 text-xs text-zinc-500">{suffix}</span>
      </div>
    </label>
  );
}

/** Slider with the transaction-per-drag contract (the Audio tab pattern). */
function GestureSlider({
  id,
  label,
  value,
  min,
  max,
  step,
  format,
  apply,
}: {
  id: string;
  label: string;
  value: number;
  min: number;
  max: number;
  step: number;
  format: (v: number) => string;
  apply: (v: number) => void;
}) {
  const gesture = useDragTransaction();
  const [dragValue, setDragValue] = useState<number | null>(null);

  return (
    <div className="w-full">
      <div className="mb-1.5 flex items-center justify-between">
        <label htmlFor={id} className="text-[11px] text-zinc-500">
          {label}
        </label>
        <span className="text-xs tabular-nums text-zinc-400">
          {format(dragValue ?? value)}
        </span>
      </div>
      <input
        id={id}
        type="range"
        min={min}
        max={max}
        step={step}
        value={dragValue ?? value}
        onPointerDown={gesture.begin}
        onPointerUp={() => {
          gesture.end();
          setDragValue(null);
        }}
        onBlur={() => {
          gesture.end();
          setDragValue(null);
        }}
        onChange={(e) => {
          const v = Number(e.target.value);
          setDragValue(v);
          apply(v);
        }}
        className="w-full accent-sky-500"
      />
    </div>
  );
}

// ---------------------------------------------------------------------
// Video tab
// ---------------------------------------------------------------------

function VideoTab() {
  const selected = useSelectedClip();
  const project = useProjectStore((s) => s.project);

  if (selected === null) {
    return <Placeholder text="Select a clip to edit its video" hint="Video" />;
  }
  const { clip, track } = selected;
  const media = project?.media.find((m) => m.id === clip.mediaId) ?? null;
  if (track.kind !== "video" || media === null || !media.hasVideo) {
    return <Placeholder text="This clip has no video" hint="Video" />;
  }

  const applyTransform = (t: Transform) => {
    void engineSetClipTransform(clip.id, t).catch(() => undefined);
  };

  return (
    <div className="w-full space-y-5 overflow-y-auto p-4">
      <section>
        <div className="mb-2 flex items-center justify-between">
          <h3 className="text-xs font-medium uppercase tracking-wider text-zinc-400">
            Transform
          </h3>
          <button
            onClick={() =>
              // A single command — its own undo entry.
              applyTransform({ x: 0, y: 0, scale: 1, rotation: 0 })
            }
            className="rounded-md border border-zinc-700 px-2 py-0.5 text-[11px] text-zinc-400 hover:bg-zinc-800"
          >
            Reset
          </button>
        </div>
        <div className="grid grid-cols-2 gap-x-3 gap-y-3">
          <DragNumber
            label="Position X"
            value={clip.transform.x}
            sensitivity={1}
            decimals={0}
            suffix="px"
            commit={(x) => applyTransform({ ...clip.transform, x })}
          />
          <DragNumber
            label="Position Y"
            value={clip.transform.y}
            sensitivity={1}
            decimals={0}
            suffix="px"
            commit={(y) => applyTransform({ ...clip.transform, y })}
          />
          <DragNumber
            label="Scale"
            value={clip.transform.scale * 100}
            sensitivity={0.5}
            decimals={0}
            suffix="%"
            min={1}
            max={1000}
            commit={(pct) =>
              applyTransform({ ...clip.transform, scale: pct / 100 })
            }
          />
          <DragNumber
            label="Rotation"
            value={clip.transform.rotation}
            sensitivity={0.5}
            decimals={1}
            suffix="°"
            commit={(rotation) => applyTransform({ ...clip.transform, rotation })}
          />
        </div>
      </section>

      <section>
        <h3 className="mb-2 text-xs font-medium uppercase tracking-wider text-zinc-400">
          Blending
        </h3>
        <GestureSlider
          id="clip-opacity"
          label="Opacity"
          value={clip.opacity}
          min={0}
          max={1}
          step={0.01}
          format={(v) => `${Math.round(v * 100)}%`}
          apply={(v) => {
            void engineSetClipOpacity(clip.id, v).catch(() => undefined);
          }}
        />
        <div className="mt-3">
          <label
            htmlFor="clip-blend"
            className="mb-1 block text-[11px] text-zinc-500"
          >
            Blend mode
          </label>
          <SelectField
            id="clip-blend"
            value={clip.blendMode}
            onChange={(v) => {
              // A dropdown pick is a single command — its own undo entry.
              void engineSetClipBlendMode(clip.id, v as BlendMode).catch(
                (err) => toast(String(err), "error"),
              );
            }}
            options={BLEND_MODES.map((m) => ({
              value: m,
              label: BLEND_LABELS[m],
            }))}
          />
        </div>
      </section>
    </div>
  );
}

// ---------------------------------------------------------------------
// Audio tab
// ---------------------------------------------------------------------

function VolumeControl({ clip }: { clip: Clip }) {
  return (
    <div className="w-full">
      <GestureSlider
        id="clip-volume"
        label="Volume"
        value={clip.volume}
        min={0}
        max={2}
        step={0.01}
        format={(v) => `${Math.round(v * 100)}%`}
        apply={(v) => {
          void engineSetClipVolume(clip.id, v).catch(() => undefined);
        }}
      />
      <div className="mt-1 flex justify-between text-[10px] text-zinc-600">
        <span>0%</span>
        <span>100%</span>
        <span>200%</span>
      </div>
      <button
        onClick={() => {
          // Keyboard/reset path: a single command is its own undo entry.
          void engineSetClipVolume(clip.id, 1.0).catch(() => undefined);
        }}
        className="mt-3 rounded-md border border-zinc-700 px-2.5 py-1 text-xs text-zinc-300 hover:bg-zinc-800"
      >
        Reset to 100%
      </button>
    </div>
  );
}

function AudioTab() {
  const selected = useSelectedClip();
  const project = useProjectStore((s) => s.project);
  const media = selected
    ? (project?.media.find((m) => m.id === selected.clip.mediaId) ?? null)
    : null;

  if (selected === null) {
    return <Placeholder text="Select a clip to edit its audio" hint="Audio" />;
  }
  if (media === null || !media.hasAudio) {
    return <Placeholder text="This clip has no audio" hint="Audio" />;
  }
  return (
    <div className="w-full p-4">
      <VolumeControl clip={selected.clip} />
    </div>
  );
}

function Placeholder({ text, hint }: { text: string; hint: string }) {
  return (
    <div className="flex flex-1 items-center justify-center p-4 text-center text-zinc-600">
      <p>
        {hint} inspector
        <br />
        <span className="text-xs">{text}</span>
      </p>
    </div>
  );
}

function Inspector() {
  const [tab, setTab] = useState<Tab>("Video");

  return (
    <aside className="flex w-80 shrink-0 flex-col border-l border-zinc-800 bg-zinc-900">
      <nav className="flex flex-wrap gap-1 border-b border-zinc-800 px-2 pt-2">
        {TABS.map((t) => (
          <button
            key={t}
            onClick={() => setTab(t)}
            className={`rounded-t px-2.5 py-1.5 text-xs ${
              tab === t
                ? "bg-zinc-800 text-zinc-100"
                : "text-zinc-500 hover:text-zinc-300"
            }`}
          >
            {t}
          </button>
        ))}
      </nav>
      {tab === "Video" ? (
        <VideoTab />
      ) : tab === "Audio" ? (
        <AudioTab />
      ) : (
        <Placeholder text={`${tab} controls land in Phase 3+`} hint={tab} />
      )}
    </aside>
  );
}

export default Inspector;
