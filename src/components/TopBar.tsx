function TopBar() {
  const MENUS = ["File", "Edit", "View", "Help"];

  return (
    <header className="flex h-11 shrink-0 items-center gap-1 border-b border-zinc-800 bg-zinc-900 px-3">
      <span className="mr-2 font-semibold tracking-wide text-zinc-100">
        Cutty
      </span>
      {MENUS.map((m) => (
        <button
          key={m}
          disabled
          className="rounded px-2 py-1 text-zinc-400 hover:bg-zinc-800 disabled:cursor-default"
        >
          {m}
        </button>
      ))}
      <div className="flex flex-1 items-center justify-center gap-2">
        <span className="text-zinc-400">Untitled Project</span>
        <span
          className="h-1.5 w-1.5 rounded-full bg-emerald-500"
          title="Autosaved"
        />
      </div>
      <button
        id="export-button"
        disabled
        title="The export dialog lands later in Phase 1"
        className="rounded-md bg-sky-600 px-4 py-1.5 font-medium text-white hover:bg-sky-500 disabled:opacity-40"
      >
        Export
      </button>
    </header>
  );
}

export default TopBar;
