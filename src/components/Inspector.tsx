import { useState } from "react";

const TABS = ["Video", "Audio", "Speed", "Animation", "Adjustment"] as const;
type Tab = (typeof TABS)[number];

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
      <div className="flex flex-1 items-center justify-center p-4 text-center text-zinc-600">
        <p>
          {tab} inspector
          <br />
          <span className="text-xs">Select a clip to edit (Phase 1+)</span>
        </p>
      </div>
    </aside>
  );
}

export default Inspector;
