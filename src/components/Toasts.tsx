// Toast stack, bottom-right above the timeline.

import { useToastStore } from "../state/toastStore";

function Toasts() {
  const toasts = useToastStore((s) => s.toasts);
  const dismiss = useToastStore((s) => s.dismiss);
  if (toasts.length === 0) return null;

  return (
    <div className="pointer-events-none fixed bottom-72 right-4 z-50 flex w-80 flex-col gap-2">
      {toasts.map((t) => (
        <button
          key={t.id}
          onClick={() => dismiss(t.id)}
          className={`pointer-events-auto rounded-md border px-3 py-2 text-left text-xs shadow-lg ${
            t.kind === "error"
              ? "border-red-800 bg-red-950/95 text-red-200"
              : "border-zinc-700 bg-zinc-800/95 text-zinc-200"
          }`}
          title="Dismiss"
        >
          {t.message}
        </button>
      ))}
    </div>
  );
}

export default Toasts;
