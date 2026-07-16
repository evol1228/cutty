// Right-hand inspector. Video tab (transform / opacity / blend), Audio
// tab (per-clip volume), and Text tab (content + full style) are live;
// the rest are Phase 3+ placeholders. Every slider/scrub drag is wrapped
// in an engine transaction so a whole gesture is exactly one undo entry
// — and typing in the Text tab debounces a whole burst into one entry.

import { useCallback, useEffect, useRef, useState } from "react";
import {
  cachedFontFamilies,
  engineBeginTransaction,
  engineCommitTransaction,
  engineSetClipBlendMode,
  engineSetClipOpacity,
  engineSetClipText,
  engineSetClipTransform,
  engineSetClipVolume,
  BLEND_MODES,
  type BlendMode,
  type Clip,
  type TextAlign,
  type TextSpec,
  type TextStyle,
  type Track,
  type Transform,
} from "../lib/engineIpc";
import { useProjectStore } from "../state/projectStore";
import { toast } from "../state/toastStore";
import SelectField from "./ui/SelectField";

const TABS = ["Video", "Audio", "Text", "Speed", "Animation", "Adjustment"] as const;
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
// Shared sections
// ---------------------------------------------------------------------

/** Position / scale / rotation grid — video and text clips share the
 * same transform placement. */
function TransformControls({ clip }: { clip: Clip }) {
  const applyTransform = (t: Transform) => {
    void engineSetClipTransform(clip.id, t).catch(() => undefined);
  };
  return (
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

  return (
    <div className="w-full space-y-5 overflow-y-auto p-4">
      <TransformControls clip={clip} />

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

// ---------------------------------------------------------------------
// Text tab
// ---------------------------------------------------------------------

/** Idle time (ms) before a typing/color burst commits its undo entry. */
const TEXT_COMMIT_MS = 700;

/**
 * A debounced engine transaction: `touch()` on every change opens the
 * transaction (first change) and re-arms the idle timer; the commit
 * lands once the burst pauses — so a whole typing burst is exactly one
 * undo entry while every keystroke still previews live. `flush()`
 * commits immediately (blur / unmount / clip switch).
 */
function useDebouncedTransaction(delayMs = TEXT_COMMIT_MS) {
  const open = useRef(false);
  const timer = useRef<number>(0);

  const flush = useCallback(() => {
    window.clearTimeout(timer.current);
    if (!open.current) return;
    open.current = false;
    void engineCommitTransaction().catch(() => undefined);
  }, []);

  const touch = useCallback(() => {
    if (!open.current) {
      open.current = true;
      void engineBeginTransaction().catch(() => {
        open.current = false;
      });
    }
    window.clearTimeout(timer.current);
    timer.current = window.setTimeout(flush, delayMs);
  }, [delayMs, flush]);

  // Unmount (clip switch, tab switch) commits whatever is pending.
  useEffect(() => flush, [flush]);
  return { touch, flush };
}

/** Native color input with the debounced-transaction contract (a picker
 * drag streams many changes; one undo entry per pause). */
function ColorField({
  id,
  label,
  value,
  apply,
}: {
  id: string;
  label: string;
  value: string;
  apply: (hex: string) => void;
}) {
  const burst = useDebouncedTransaction();
  return (
    <label className="block" htmlFor={id}>
      <span className="mb-1 block text-[11px] text-zinc-500">{label}</span>
      <div className="flex items-center gap-2 rounded-md border border-zinc-700 bg-zinc-800 px-1.5 py-1">
        <input
          type="color"
          id={id}
          value={value.slice(0, 7)}
          onChange={(e) => {
            burst.touch();
            apply(e.target.value);
          }}
          onBlur={burst.flush}
          className="h-6 w-8 shrink-0 cursor-pointer rounded border-0 bg-transparent p-0"
        />
        <span className="text-xs uppercase tabular-nums text-zinc-400">
          {value.slice(0, 7)}
        </span>
      </div>
    </label>
  );
}

const WEIGHT_OPTIONS = [
  { value: "400", label: "Regular" },
  { value: "500", label: "Medium" },
  { value: "600", label: "SemiBold" },
  { value: "700", label: "Bold" },
  { value: "800", label: "ExtraBold" },
  { value: "900", label: "Black" },
];

const ALIGN_OPTIONS: Array<{ value: TextAlign; glyph: string; title: string }> = [
  { value: "left", glyph: "⯇", title: "Align left" },
  { value: "center", glyph: "☰", title: "Align center" },
  { value: "right", glyph: "⯈", title: "Align right" },
];

/** All controls for one text clip. Keyed by clip id from TextTab, so a
 * selection switch unmounts (flushing any pending typing burst). */
function TextControls({ clip, text }: { clip: Clip; text: TextSpec }) {
  const [families, setFamilies] = useState<string[]>([]);
  const typing = useDebouncedTransaction();
  // Local echo while typing: the engine snapshot chases keystrokes over
  // IPC; the textarea must not lag or reorder.
  const [draft, setDraft] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    cachedFontFamilies()
      .then((list) => {
        if (alive) setFamilies(list);
      })
      .catch(() => undefined);
    return () => {
      alive = false;
    };
  }, []);

  const send = (next: TextSpec) => {
    void engineSetClipText(clip.id, next).catch(() => undefined);
  };
  const style = text.style;
  const setStyle = (patch: Partial<TextStyle>) => {
    send({ ...text, style: { ...style, ...patch } });
  };

  const familyOptions = [
    { value: "", label: "Default (Sans)" },
    { value: "serif", label: "Serif" },
    { value: "monospace", label: "Monospace" },
    ...families.map((f) => ({ value: f, label: f })),
  ];

  return (
    <div className="w-full space-y-5 overflow-y-auto p-4">
      <section>
        <h3 className="mb-2 text-xs font-medium uppercase tracking-wider text-zinc-400">
          Content
        </h3>
        <textarea
          value={draft ?? text.content}
          rows={3}
          spellCheck={false}
          placeholder="Type your text…"
          onChange={(e) => {
            setDraft(e.target.value);
            typing.touch();
            send({ ...text, content: e.target.value });
          }}
          onBlur={() => {
            typing.flush();
            setDraft(null);
          }}
          className="w-full resize-y rounded-md border border-zinc-700 bg-zinc-800 px-2 py-1.5 text-sm text-zinc-100 outline-none focus:border-zinc-500"
        />
      </section>

      <section>
        <h3 className="mb-2 text-xs font-medium uppercase tracking-wider text-zinc-400">
          Font
        </h3>
        <div className="space-y-3">
          <SelectField
            id="text-family"
            value={style.fontFamily}
            onChange={(fontFamily) => setStyle({ fontFamily })}
            options={familyOptions}
          />
          <div className="grid grid-cols-2 gap-x-3">
            <div>
              <span className="mb-1 block text-[11px] text-zinc-500">Weight</span>
              <SelectField
                id="text-weight"
                value={String(style.weight)}
                onChange={(w) => setStyle({ weight: Number(w) })}
                options={WEIGHT_OPTIONS}
              />
            </div>
            <DragNumber
              label="Size"
              value={style.fontSize}
              sensitivity={0.5}
              decimals={0}
              suffix="px"
              min={8}
              max={800}
              commit={(fontSize) => setStyle({ fontSize })}
            />
          </div>
          <div className="flex items-center gap-1">
            {ALIGN_OPTIONS.map((opt) => (
              <button
                key={opt.value}
                title={opt.title}
                onClick={() => setStyle({ align: opt.value })}
                className={`flex-1 rounded-md border px-2 py-1 text-sm ${
                  style.align === opt.value
                    ? "border-sky-600 bg-sky-600/20 text-sky-300"
                    : "border-zinc-700 text-zinc-400 hover:bg-zinc-800"
                }`}
              >
                {opt.glyph}
              </button>
            ))}
          </div>
        </div>
      </section>

      <section>
        <h3 className="mb-2 text-xs font-medium uppercase tracking-wider text-zinc-400">
          Color &amp; stroke
        </h3>
        <div className="grid grid-cols-2 gap-x-3 gap-y-3">
          <ColorField
            id="text-fill"
            label="Fill"
            value={style.fill}
            apply={(fill) => setStyle({ fill })}
          />
          <ColorField
            id="text-stroke-color"
            label="Stroke"
            value={style.strokeColor}
            apply={(strokeColor) => setStyle({ strokeColor })}
          />
          <DragNumber
            label="Stroke width"
            value={style.strokeWidth}
            sensitivity={0.2}
            decimals={1}
            suffix="px"
            min={0}
            max={60}
            commit={(strokeWidth) => setStyle({ strokeWidth })}
          />
        </div>
      </section>

      <section>
        <h3 className="mb-2 text-xs font-medium uppercase tracking-wider text-zinc-400">
          Shadow
        </h3>
        <div className="space-y-3">
          <GestureSlider
            id="text-shadow-alpha"
            label="Opacity"
            value={style.shadowAlpha}
            min={0}
            max={1}
            step={0.01}
            format={(v) => `${Math.round(v * 100)}%`}
            apply={(shadowAlpha) => setStyle({ shadowAlpha })}
          />
          <div className="grid grid-cols-3 items-end gap-x-3">
            <ColorField
              id="text-shadow-color"
              label="Color"
              value={style.shadowColor}
              apply={(shadowColor) => setStyle({ shadowColor })}
            />
            <DragNumber
              label="Offset X"
              value={style.shadowOffsetX}
              sensitivity={0.2}
              decimals={1}
              suffix="px"
              min={-200}
              max={200}
              commit={(shadowOffsetX) => setStyle({ shadowOffsetX })}
            />
            <DragNumber
              label="Offset Y"
              value={style.shadowOffsetY}
              sensitivity={0.2}
              decimals={1}
              suffix="px"
              min={-200}
              max={200}
              commit={(shadowOffsetY) => setStyle({ shadowOffsetY })}
            />
          </div>
        </div>
      </section>

      <TransformControls clip={clip} />

      <section>
        <h3 className="mb-2 text-xs font-medium uppercase tracking-wider text-zinc-400">
          Blending
        </h3>
        <GestureSlider
          id="text-opacity"
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
      </section>
    </div>
  );
}

function TextTab() {
  const selected = useSelectedClip();
  if (selected === null || !selected.clip.text) {
    return (
      <Placeholder
        text="Select a text clip — or press T to add one at the playhead"
        hint="Text"
      />
    );
  }
  // Keyed by clip id: switching clips remounts and flushes any pending
  // typing burst into its own undo entry.
  return (
    <TextControls
      key={selected.clip.id}
      clip={selected.clip}
      text={selected.clip.text}
    />
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
  const selected = useSelectedClip();
  const isText = selected?.clip.text != null;

  // Context-sensitive tab, derived (no state sync): a selected text clip
  // shows Text where Video would show (Video is meaningless for text),
  // and Text falls back to Video when the selection isn't text. Every
  // other manual tab choice is honored as-is.
  const displayTab: Tab =
    isText && tab === "Video" ? "Text" : !isText && tab === "Text" ? "Video" : tab;

  return (
    <aside className="flex w-80 shrink-0 flex-col border-l border-zinc-800 bg-zinc-900">
      <nav className="flex flex-wrap gap-1 border-b border-zinc-800 px-2 pt-2">
        {TABS.map((t) => (
          <button
            key={t}
            onClick={() => setTab(t)}
            className={`rounded-t px-2.5 py-1.5 text-xs ${
              displayTab === t
                ? "bg-zinc-800 text-zinc-100"
                : "text-zinc-500 hover:text-zinc-300"
            }`}
          >
            {t}
          </button>
        ))}
      </nav>
      {displayTab === "Video" ? (
        <VideoTab />
      ) : displayTab === "Audio" ? (
        <AudioTab />
      ) : displayTab === "Text" ? (
        <TextTab />
      ) : (
        <Placeholder
          text={`${displayTab} controls land in Phase 3+`}
          hint={displayTab}
        />
      )}
    </aside>
  );
}

export default Inspector;
