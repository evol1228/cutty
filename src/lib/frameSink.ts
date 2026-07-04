// Decoded frames bypass React state (30–60 events/s): the media pool opens
// the player and dispatches frames here; the Player canvas registers the
// single handler.

import type { FrameMessage } from "./ipc";

type FrameHandler = (frame: FrameMessage) => void;

let handler: FrameHandler | null = null;

export function setFrameHandler(h: FrameHandler | null): void {
  handler = h;
}

export function dispatchFrame(frame: FrameMessage): void {
  handler?.(frame);
}
