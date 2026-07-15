// Right-hand inspector. Phase 1 ships the Audio tab (per-clip volume —
// the mixer applies it in preview and export alike); the other tabs are
// Phase 2+ placeholders.

import { useRef, useState } from "react";
import {
  engineBeginTransaction,
  engineCommitTransaction,
  engineSetClipVolume,
  type Clip,
} from "../lib/engineIpc";
import { useProjectStore } from "../state/projectStore";
import { toast } from "../state/toastStore";

const TABS = ["Video", "Audio", "Speed", "Animation", "Adjustment"] as const;
type Tab = (typeof TABS)[number];

/** The single selected clip, or null (none / multi-select). */
function useSelectedClip(): Clip | null {
  const project = useProjectStore((s) => s.project);
  const selection = useProjectStore((s) => s.selection);
  if (project === null || selection.length !== 1) return null;
  for (const track of project.tracks) {
    const clip = track.clips.find((c) => c.id === selection[0]);
    if (clip) return clip;
  }
  return null;
}

/** Volume slider: transaction-per-drag so a whole gesture is one undo
 * entry, exactly like timeline drags. */
function VolumeControl({ clip }: { clip: Clip }) {
  const inGesture = useRef(false);
  // Live slider position during a drag (the engine echoes state back per
  // change, but driving the input from local state keeps it glitch-free).
  const [dragValue, setDragValue] = useState<number | null>(null);
  const percent = Math.round((dragValue ?? clip.volume) * 100);

  const beginGesture = () => {
    if (inGesture.current) return;
    inGesture.current = true;
    void engineBeginTransaction().catch(() => {
      inGesture.current = false;
    });
  };

  const endGesture = () => {
    if (!inGesture.current) return;
    inGesture.current = false;
    setDragValue(null);
    void engineCommitTransaction().catch((err) =>
      toast(`Volume change failed: ${String(err)}`, "error"),
    );
  };

  const apply = (value: number) => {
    setDragValue(value);
    void engineSetClipVolume(clip.id, value).catch(() => undefined);
  };

  return (
    <div className="w-full">
      <div className="mb-2 flex items-center justify-between">
        <label htmlFor="clip-volume" className="text-sm text-zinc-300">
          Volume
        </label>
        <span className="text-sm tabular-nums text-zinc-400">{percent}%</span>
      </div>
      <input
        id="clip-volume"
        type="range"
        min={0}
        max={2}
        step={0.01}
        value={dragValue ?? clip.volume}
        onPointerDown={beginGesture}
        onPointerUp={endGesture}
        onBlur={endGesture}
        onChange={(e) => apply(Number(e.target.value))}
        className="w-full accent-sky-500"
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
  const clip = useSelectedClip();
  const project = useProjectStore((s) => s.project);
  const media = clip
    ? (project?.media.find((m) => m.id === clip.mediaId) ?? null)
    : null;

  if (clip === null) {
    return (
      <Placeholder text="Select a clip to edit its audio" hint="Audio" />
    );
  }
  if (media === null || !media.hasAudio) {
    return <Placeholder text="This clip has no audio" hint="Audio" />;
  }
  return (
    <div className="w-full p-4">
      <VolumeControl clip={clip} />
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
  const [tab, setTab] = useState<Tab>("Audio");

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
      {tab === "Audio" ? (
        <AudioTab />
      ) : (
        <Placeholder text={`${tab} controls land in Phase 2+`} hint={tab} />
      )}
    </aside>
  );
}

export default Inspector;
