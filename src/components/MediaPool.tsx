import { useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { listen } from "@tauri-apps/api/event";
import {
  closePlayer,
  generateProxy,
  openPlayer,
  probeMedia,
  PROXY_PROGRESS_EVENT,
  type ProxyProgressEvent,
} from "../lib/ipc";
import { dispatchFrame } from "../lib/frameSink";
import { usePlayerStore } from "../state/playerStore";

const TABS = ["Import", "Library"] as const;
type Tab = (typeof TABS)[number];

const VIDEO_EXTENSIONS = ["mp4", "mkv", "mov", "webm", "avi", "m4v"];

function formatBytes(bytes: number): string {
  if (bytes >= 1e9) return `${(bytes / 1e9).toFixed(2)} GB`;
  if (bytes >= 1e6) return `${(bytes / 1e6).toFixed(1)} MB`;
  return `${(bytes / 1e3).toFixed(0)} KB`;
}

// Temporary Phase 0 pipeline test surface: opens a file, shows the probe
// result, generates the proxy. Replaced by the real import flow in Phase 1.
function ProbeTester() {
  const { media: info, proxyPath, proxyProgress } = usePlayerStore();
  const { setMedia, setProxyPath, setProxyProgress } = usePlayerStore();
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    const unlisten = listen<ProxyProgressEvent>(PROXY_PROGRESS_EVENT, (e) => {
      // Progress for some other (stale/concurrent) source must not move
      // this file's bar.
      const state = usePlayerStore.getState();
      if (e.payload.srcPath !== state.media?.path) return;
      state.setProxyProgress(e.payload.percent);
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, []);

  async function openAndProbe() {
    const path = await open({
      multiple: false,
      filters: [{ name: "Video", extensions: VIDEO_EXTENSIONS }],
    });
    if (typeof path !== "string") return;
    setBusy(true);
    setError(null);
    try {
      // Tear down any running player first — otherwise its audio keeps
      // playing with every control gone from the UI.
      await closePlayer().catch(() => {});
      setMedia(await probeMedia(path));
    } catch (e) {
      setMedia(null);
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  async function makeProxy() {
    if (!info) return;
    const forPath = info.path;
    setError(null);
    setProxyProgress(0);
    try {
      const proxyPath = await generateProxy(info.path, info.durationSec);
      // The user may have switched files while this encoded — don't
      // attach file A's proxy to file B.
      if (usePlayerStore.getState().media?.path !== forPath) return;
      setProxyPath(proxyPath);
    } catch (e) {
      if (usePlayerStore.getState().media?.path !== forPath) return;
      setProxyProgress(null);
      setError(String(e));
    }
  }

  async function loadIntoPlayer() {
    if (!proxyPath) return;
    setError(null);
    try {
      const playerInfo = await openPlayer(proxyPath, dispatchFrame);
      usePlayerStore.getState().setPlayerInfo(playerInfo);
    } catch (e) {
      setError(String(e));
    }
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-3">
      <button
        onClick={openAndProbe}
        disabled={busy}
        className="rounded-md border border-zinc-700 bg-zinc-800 px-3 py-2 text-zinc-200 hover:bg-zinc-700 disabled:opacity-50"
      >
        {busy ? "Probing…" : "Open file (probe test)"}
      </button>
      {error && (
        <p className="rounded bg-red-950/60 p-2 text-xs text-red-400">
          {error}
        </p>
      )}
      {info && (
        <dl className="min-h-0 flex-1 space-y-1 overflow-y-auto rounded bg-zinc-950/60 p-2 font-mono text-xs">
          <Row k="File" v={info.path.split("/").pop() ?? info.path} />
          <Row k="Container" v={info.container.split(",")[0]} />
          <Row k="Duration" v={`${info.durationSec.toFixed(2)} s`} />
          <Row k="Size" v={formatBytes(info.sizeBytes)} />
          {info.video && (
            <>
              <Row
                k="Video"
                v={
                  `${info.video.codec} ${info.video.width}×${info.video.height}` +
                  (info.video.rotation !== 0
                    ? ` (rotated ${info.video.rotation}°)`
                    : "")
                }
              />
              <Row k="FPS" v={info.video.fps.toFixed(3)} />
            </>
          )}
          {info.audio && (
            <Row
              k="Audio"
              v={`${info.audio.codec} ${info.audio.sampleRate} Hz ${info.audio.channels}ch`}
            />
          )}
          <Row
            k="Streams"
            v={info.streams.map((s) => `${s.kind}:${s.codec}`).join(", ")}
          />
        </dl>
      )}
      {info && proxyProgress === null && !proxyPath && (
        <button
          onClick={makeProxy}
          className="rounded-md border border-zinc-700 bg-zinc-800 px-3 py-2 text-zinc-200 hover:bg-zinc-700"
        >
          Generate 720p proxy
        </button>
      )}
      {proxyProgress !== null && (
        <div>
          <div className="mb-1 flex justify-between text-xs text-zinc-400">
            <span>Generating proxy…</span>
            <span>{proxyProgress.toFixed(0)}%</span>
          </div>
          <div className="h-1.5 overflow-hidden rounded bg-zinc-800">
            <div
              className="h-full bg-sky-500 transition-[width]"
              style={{ width: `${proxyProgress}%` }}
            />
          </div>
        </div>
      )}
      {proxyPath && (
        <>
          <p
            className="truncate rounded bg-emerald-950/50 p-2 text-xs text-emerald-400"
            title={proxyPath}
          >
            Proxy ready: {proxyPath.split("/").pop()}
          </p>
          <button
            onClick={() => void loadIntoPlayer()}
            className="rounded-md border border-zinc-700 bg-zinc-800 px-3 py-2 text-zinc-200 hover:bg-zinc-700"
          >
            Open in player
          </button>
        </>
      )}
    </div>
  );
}

function Row({ k, v }: { k: string; v: string }) {
  return (
    <div className="flex justify-between gap-2">
      <dt className="shrink-0 text-zinc-500">{k}</dt>
      <dd className="truncate text-zinc-300" title={v}>
        {v}
      </dd>
    </div>
  );
}

function MediaPool() {
  const [tab, setTab] = useState<Tab>("Import");

  return (
    <aside className="flex w-72 shrink-0 flex-col border-r border-zinc-800 bg-zinc-900">
      <nav className="flex gap-1 border-b border-zinc-800 px-2 pt-2">
        {TABS.map((t) => (
          <button
            key={t}
            onClick={() => setTab(t)}
            className={`rounded-t px-3 py-1.5 ${
              tab === t
                ? "bg-zinc-800 text-zinc-100"
                : "text-zinc-500 hover:text-zinc-300"
            }`}
          >
            {t}
          </button>
        ))}
      </nav>
      <div className="flex min-h-0 flex-1 flex-col p-3">
        {tab === "Import" ? (
          <ProbeTester />
        ) : (
          <div className="flex flex-1 items-center justify-center text-zinc-600">
            Library is empty
          </div>
        )}
      </div>
    </aside>
  );
}

export default MediaPool;
