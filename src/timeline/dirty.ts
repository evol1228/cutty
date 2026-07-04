// Dirty-flag render scheduling. The timeline only draws inside a
// requestAnimationFrame that something explicitly requested — an idle
// timeline schedules no frames and burns zero CPU. Multiple requests
// before the next frame coalesce into a single draw.

let drawCallback: (() => void) | null = null;
let rafId: number | null = null;

/** Install the draw function (the controller owns it). Pass null to detach. */
export function setDrawCallback(cb: (() => void) | null): void {
  drawCallback = cb;
  if (cb === null && rafId !== null) {
    cancelAnimationFrame(rafId);
    rafId = null;
  }
}

/** Mark the timeline dirty; schedules exactly one animation frame. */
export function requestDraw(): void {
  if (rafId !== null || drawCallback === null) return;
  rafId = requestAnimationFrame(() => {
    rafId = null;
    drawCallback?.();
  });
}
