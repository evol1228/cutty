import { useEffect, useRef } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  PLAYER_EOF_EVENT,
  PLAYER_ERROR_EVENT,
  PLAYER_POSITION_EVENT,
  playerSeek,
  playerStep,
  playerToggle,
  type PositionEvent,
} from "../lib/ipc";
import { setFrameHandler } from "../lib/frameSink";
import { usePlayerStore } from "../state/playerStore";

/** HH:MM:SS:FF timecode. */
function timecode(sec: number, fps: number): string {
  const totalFrames = Math.round(sec * fps);
  const fpsInt = Math.max(1, Math.round(fps));
  const ff = totalFrames % fpsInt;
  const s = Math.floor(totalFrames / fpsInt);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(Math.floor(s / 3600))}:${pad(Math.floor(s / 60) % 60)}:${pad(s % 60)}:${pad(ff)}`;
}

const SEEK_THROTTLE_MS = 80;

function Player() {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const { playerInfo, playing, positionSec, inPointSec, outPointSec } =
    usePlayerStore();
  const setInPoint = usePlayerStore((s) => s.setInPoint);
  const setOutPoint = usePlayerStore((s) => s.setOutPoint);

  // Paint decoded frames. Bypasses React entirely — see frameSink.
  useEffect(() => {
    let dispatched = 0;
    let drawn = 0;
    setFrameHandler((frame) => {
      const seq = ++dispatched;
      createImageBitmap(frame.jpeg).then((bmp) => {
        // JPEG decodes are async; never paint an older frame over a newer.
        if (seq <= drawn) {
          bmp.close();
          return;
        }
        drawn = seq;
        const canvas = canvasRef.current;
        const ctx = canvas?.getContext("2d");
        if (canvas && ctx) {
          if (canvas.width !== bmp.width || canvas.height !== bmp.height) {
            canvas.width = bmp.width;
            canvas.height = bmp.height;
          }
          ctx.drawImage(bmp, 0, 0);
        }
        bmp.close();
        usePlayerStore.getState().setPosition(frame.ptsSec);
      });
    });
    return () => setFrameHandler(null);
  }, []);

  // Engine transport events.
  useEffect(() => {
    const unlistens = [
      listen<PositionEvent>(PLAYER_POSITION_EVENT, (e) => {
        const s = usePlayerStore.getState();
        s.setPlaying(e.payload.playing);
        if (!e.payload.playing) s.setPosition(e.payload.positionSec);
      }),
      listen(PLAYER_EOF_EVENT, () => {
        usePlayerStore.getState().setPlaying(false);
      }),
      listen<string>(PLAYER_ERROR_EVENT, (e) => {
        console.error("player error:", e.payload);
      }),
    ];
    return () => {
      unlistens.forEach((u) => u.then((fn) => fn()));
    };
  }, []);

  // Keyboard transport: Space toggle, ←/→ frame step.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!usePlayerStore.getState().playerInfo) return;
      if (e.code === "Space") {
        e.preventDefault();
        if (!e.repeat) void playerToggle();
      } else if (e.code === "ArrowLeft") {
        e.preventDefault();
        void playerStep(-1);
      } else if (e.code === "ArrowRight") {
        e.preventDefault();
        void playerStep(1);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  // Slider seeks, throttled so scrubbing doesn't spam decoder restarts.
  const lastSeek = useRef(0);
  const pendingSeek = useRef<number | null>(null);
  function onScrub(value: number) {
    usePlayerStore.getState().setPosition(value);
    const now = performance.now();
    if (now - lastSeek.current >= SEEK_THROTTLE_MS) {
      lastSeek.current = now;
      void playerSeek(value);
    } else if (pendingSeek.current === null) {
      const wait = SEEK_THROTTLE_MS - (now - lastSeek.current);
      pendingSeek.current = value;
      setTimeout(() => {
        const v = pendingSeek.current;
        pendingSeek.current = null;
        lastSeek.current = performance.now();
        if (v !== null) void playerSeek(v);
      }, wait);
    } else {
      pendingSeek.current = value;
    }
  }

  const fps = playerInfo?.fps ?? 30;
  const duration = playerInfo?.durationSec ?? 0;

  return (
    <main className="flex min-w-0 flex-1 flex-col bg-zinc-950">
      <div className="flex min-h-0 flex-1 items-center justify-center p-4">
        <canvas
          id="player-canvas"
          ref={canvasRef}
          className="max-h-full max-w-full rounded bg-black"
          width={1280}
          height={720}
        />
      </div>
      <div className="flex h-12 shrink-0 items-center gap-3 border-t border-zinc-800 bg-zinc-900 px-4">
        <span className="font-mono text-zinc-400" title="position">
          {timecode(positionSec, fps)}
        </span>
        <button
          disabled={!playerInfo}
          onClick={() => void playerStep(-1)}
          className="rounded bg-zinc-800 px-2 py-1 text-zinc-300 hover:bg-zinc-700 disabled:opacity-40"
          title="Previous frame (←)"
        >
          ⏮︎
        </button>
        <button
          disabled={!playerInfo}
          onClick={() => void playerToggle()}
          className="rounded-full bg-zinc-800 px-4 py-1.5 text-zinc-200 hover:bg-zinc-700 disabled:opacity-40"
          title="Play/Pause (Space)"
        >
          {playing ? "⏸" : "▶"}
        </button>
        <button
          disabled={!playerInfo}
          onClick={() => void playerStep(1)}
          className="rounded bg-zinc-800 px-2 py-1 text-zinc-300 hover:bg-zinc-700 disabled:opacity-40"
          title="Next frame (→)"
        >
          ⏭︎
        </button>
        <input
          type="range"
          disabled={!playerInfo}
          min={0}
          max={duration}
          step={1 / fps}
          value={positionSec}
          onChange={(e) => onScrub(Number(e.target.value))}
          className="min-w-0 flex-1 accent-sky-500"
        />
        <span className="font-mono text-zinc-600">
          {timecode(duration, fps)}
        </span>
        <div className="flex items-center gap-1 border-l border-zinc-800 pl-3 text-xs">
          <button
            disabled={!playerInfo}
            onClick={() => setInPoint(positionSec)}
            className="rounded bg-zinc-800 px-2 py-1 text-zinc-400 hover:bg-zinc-700 disabled:opacity-40"
            title="Set trim in point"
          >
            In {inPointSec !== null ? timecode(inPointSec, fps) : "—"}
          </button>
          <button
            disabled={!playerInfo}
            onClick={() => setOutPoint(positionSec)}
            className="rounded bg-zinc-800 px-2 py-1 text-zinc-400 hover:bg-zinc-700 disabled:opacity-40"
            title="Set trim out point"
          >
            Out {outPointSec !== null ? timecode(outPointSec, fps) : "—"}
          </button>
        </div>
      </div>
    </main>
  );
}

export default Player;
