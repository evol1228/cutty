// Center panel: the preview player. Renders whatever the Rust playback
// engine presents (it owns the clock, decoding, and all timeline logic) —
// this component attaches the binary frame channel, sends transport
// commands, and mirrors position events into the stores.

import { useEffect, useRef } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  attachPlayback,
  PLAYER_EOF_EVENT,
  PLAYER_ERROR_EVENT,
  PLAYER_POSITION_EVENT,
  playbackStep,
  playbackToggle,
  type PositionEvent,
} from "../lib/ipc";
import { dispatchFrame, setFrameHandler } from "../lib/frameSink";
import { usePlayerStore } from "../state/playerStore";
import { useProjectStore } from "../state/projectStore";
import { toast } from "../state/toastStore";
import { timelineEndSec } from "../timeline/actions";
import { ensureVisible } from "../timeline/view";
import PlayerGizmo from "./PlayerGizmo";

/** HH:MM:SS:FF timecode. */
function timecode(sec: number, fps: number): string {
  const totalFrames = Math.round(sec * fps);
  const fpsInt = Math.max(1, Math.round(fps));
  const ff = totalFrames % fpsInt;
  const s = Math.floor(totalFrames / fpsInt);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(Math.floor(s / 3600))}:${pad(Math.floor(s / 60) % 60)}:${pad(s % 60)}:${pad(ff)}`;
}

/** Preview canvas resolution: the project frame fit within 1280×720. */
function previewSize(width: number, height: number): [number, number] {
  const scale = Math.min(1280 / width, 720 / height, 1);
  return [Math.round(width * scale), Math.round(height * scale)];
}

let attachStarted = false;

function Player() {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const stageRef = useRef<HTMLDivElement>(null);
  const attached = usePlayerStore((s) => s.attached);
  const playing = usePlayerStore((s) => s.playing);
  const playheadSec = useProjectStore((s) => s.playheadSec);
  const project = useProjectStore((s) => s.project);

  const fps = project?.settings.fps ?? 30;
  const durationSec = project ? timelineEndSec(project) : 0;
  const [canvasW, canvasH] = previewSize(
    project?.settings.width ?? 1920,
    project?.settings.height ?? 1080,
  );

  // Paint presented frames, letterboxed into the project-shaped canvas.
  // Bypasses React entirely (30–60 events/s) — see frameSink.
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
          ctx.fillStyle = "#000";
          ctx.fillRect(0, 0, canvas.width, canvas.height);
          const scale = Math.min(
            canvas.width / bmp.width,
            canvas.height / bmp.height,
          );
          const dw = bmp.width * scale;
          const dh = bmp.height * scale;
          ctx.drawImage(bmp, (canvas.width - dw) / 2, (canvas.height - dh) / 2, dw, dh);
        }
        bmp.close();
      });
    });
    return () => setFrameHandler(null);
  }, []);

  // Attach the playback engine once (idempotent under StrictMode).
  useEffect(() => {
    if (attachStarted) return;
    attachStarted = true;
    attachPlayback(dispatchFrame)
      .then(() => usePlayerStore.getState().setAttached(true))
      .catch((err: unknown) => {
        attachStarted = false;
        toast(`Playback engine failed to start: ${String(err)}`, "error");
      });
  }, []);

  // Engine transport events: the engine clock drives the playhead.
  useEffect(() => {
    const unlistens = [
      listen<PositionEvent>(PLAYER_POSITION_EVENT, (e) => {
        usePlayerStore.getState().setPlaying(e.payload.playing);
        useProjectStore.getState().setPlayhead(e.payload.positionSec);
        if (e.payload.playing) ensureVisible(e.payload.positionSec);
      }),
      listen(PLAYER_EOF_EVENT, () => {
        usePlayerStore.getState().setPlaying(false);
      }),
      listen<string>(PLAYER_ERROR_EVENT, (e) => {
        console.warn("playback:", e.payload);
        toast(e.payload, "error");
      }),
    ];
    return () => {
      unlistens.forEach((u) => u.then((fn) => fn()));
    };
  }, []);

  const transportReady = attached && durationSec > 0;

  return (
    <main className="flex min-w-0 flex-1 flex-col bg-zinc-950">
      <div
        ref={stageRef}
        className="relative flex min-h-0 flex-1 items-center justify-center p-4"
      >
        <canvas
          id="player-canvas"
          ref={canvasRef}
          className="max-h-full max-w-full rounded bg-black"
          width={canvasW}
          height={canvasH}
        />
        <PlayerGizmo canvasRef={canvasRef} containerRef={stageRef} />
      </div>
      <div className="flex h-12 shrink-0 items-center justify-center gap-3 border-t border-zinc-800 bg-zinc-900 px-4">
        <span className="font-mono text-zinc-400" title="playhead">
          {timecode(playheadSec, fps)}
        </span>
        <button
          disabled={!transportReady}
          onClick={() => void playbackStep(-1)}
          className="rounded bg-zinc-800 px-2 py-1 text-zinc-300 hover:bg-zinc-700 disabled:opacity-40"
          title="Previous frame (←)"
        >
          ⏮︎
        </button>
        <button
          id="play-toggle"
          disabled={!transportReady}
          onClick={() => void playbackToggle()}
          className="rounded-full bg-zinc-800 px-4 py-1.5 text-zinc-200 hover:bg-zinc-700 disabled:opacity-40"
          title="Play/Pause (Space)"
        >
          {playing ? "⏸" : "▶"}
        </button>
        <button
          disabled={!transportReady}
          onClick={() => void playbackStep(1)}
          className="rounded bg-zinc-800 px-2 py-1 text-zinc-300 hover:bg-zinc-700 disabled:opacity-40"
          title="Next frame (→)"
        >
          ⏭︎
        </button>
        <span className="font-mono text-zinc-600" title="timeline duration">
          {timecode(durationSec, fps)}
        </span>
      </div>
    </main>
  );
}

export default Player;
