const TOOLS = ["Split", "Delete", "Undo", "Redo", "Snap"];

const TRACKS = [
  { id: "V1", kind: "video" },
  { id: "A1", kind: "audio" },
] as const;

function Timeline() {
  return (
    <section className="flex h-56 shrink-0 flex-col border-t border-zinc-800 bg-zinc-900">
      <div className="flex h-9 shrink-0 items-center gap-1 border-b border-zinc-800 px-2">
        {TOOLS.map((t) => (
          <button
            key={t}
            disabled
            className="rounded px-2 py-1 text-xs text-zinc-500 disabled:cursor-default"
          >
            {t}
          </button>
        ))}
        <div className="flex flex-1 items-center justify-end gap-2 text-xs text-zinc-500">
          <span>Zoom</span>
          <input
            type="range"
            disabled
            defaultValue={50}
            className="w-28 accent-sky-500"
          />
        </div>
      </div>
      <div className="flex min-h-0 flex-1">
        <div className="flex w-24 shrink-0 flex-col border-r border-zinc-800">
          {TRACKS.map((t) => (
            <div
              key={t.id}
              className="flex h-14 items-center justify-between border-b border-zinc-800/60 px-2 text-xs text-zinc-500"
            >
              <span>{t.id}</span>
              <span className="flex gap-1 text-zinc-700">🔒 🔇</span>
            </div>
          ))}
        </div>
        <div className="relative flex-1 overflow-hidden bg-zinc-950/60">
          <div className="absolute inset-0 flex items-center justify-center text-zinc-700">
            Timeline (canvas renderer lands in Phase 1)
          </div>
        </div>
      </div>
    </section>
  );
}

export default Timeline;
