// Pointer-based drag from the media pool onto the timeline canvas.
//
// Deliberately not HTML5 drag-and-drop: WebKitGTK's DnD is historically
// unreliable on Wayland, and pointer events give us full control over the
// ghost and the drop preview. The pool starts a drag here; the timeline
// controller registers itself as the drop target and owns all snapping
// and the AddClip command (via the engine, as always).

export interface DragMedia {
  mediaId: number;
  name: string;
  durationSec: number;
  hasVideo: boolean;
  hasAudio: boolean;
  thumbnailUrl: string | null;
}

export interface TimelineDropTarget {
  /** Pointer moved while dragging pool media (may be outside the canvas —
   * the target hides its preview then). */
  over: (clientX: number, clientY: number, media: DragMedia) => void;
  /** Drag ended over these coordinates; add the clip if they're inside. */
  drop: (clientX: number, clientY: number, media: DragMedia) => void;
  /** Drag cancelled or left entirely. */
  leave: () => void;
}

let dropTarget: TimelineDropTarget | null = null;

/** The timeline controller registers/unregisters itself here. */
export function setTimelineDropTarget(target: TimelineDropTarget | null): void {
  dropTarget = target;
}

/** A transition item dragged from the Transitions tab. */
export interface DragTransition {
  id: string;
  label: string;
  defaultDuration: number;
}

/** Drop target for transition drags: the timeline highlights cut points
 * under the pointer and applies the transition on release. */
export interface TransitionDropTarget {
  over: (clientX: number, clientY: number, item: DragTransition) => void;
  drop: (clientX: number, clientY: number, item: DragTransition) => void;
  leave: () => void;
}

let transitionTarget: TransitionDropTarget | null = null;

export function setTransitionDropTarget(
  target: TransitionDropTarget | null,
): void {
  transitionTarget = target;
}

/** Pointer travel before a press becomes a drag, CSS px. */
const DRAG_THRESHOLD_PX = 4;

function makeGhost(media: DragMedia): HTMLElement {
  const ghost = document.createElement("div");
  ghost.className =
    "pointer-events-none fixed z-50 flex items-center gap-2 rounded-md " +
    "border border-sky-600 bg-zinc-900/90 px-2 py-1.5 shadow-xl";
  if (media.thumbnailUrl) {
    const img = document.createElement("img");
    img.src = media.thumbnailUrl;
    img.className = "h-8 w-14 rounded-sm object-cover";
    ghost.appendChild(img);
  } else {
    const note = document.createElement("span");
    note.textContent = media.hasVideo ? "🎞" : "♪";
    note.className = "w-6 text-center text-zinc-400";
    ghost.appendChild(note);
  }
  const label = document.createElement("span");
  label.textContent = media.name;
  label.className = "max-w-40 truncate text-xs text-zinc-200";
  ghost.appendChild(label);
  document.body.appendChild(ghost);
  return ghost;
}

/**
 * Begin dragging a pool item. Call from the item's pointerdown; the drag
 * activates after a small movement threshold so plain clicks still select.
 */
export function beginPoolDrag(down: PointerEvent, media: DragMedia): void {
  if (down.button !== 0) return;
  const startX = down.clientX;
  const startY = down.clientY;
  let ghost: HTMLElement | null = null;

  function onMove(e: PointerEvent): void {
    if (!ghost) {
      const dx = e.clientX - startX;
      const dy = e.clientY - startY;
      if (dx * dx + dy * dy < DRAG_THRESHOLD_PX * DRAG_THRESHOLD_PX) return;
      ghost = makeGhost(media);
    }
    ghost.style.left = `${e.clientX + 12}px`;
    ghost.style.top = `${e.clientY + 10}px`;
    dropTarget?.over(e.clientX, e.clientY, media);
  }

  function cleanup(): void {
    ghost?.remove();
    ghost = null;
    window.removeEventListener("pointermove", onMove);
    window.removeEventListener("pointerup", onUp);
    window.removeEventListener("keydown", onKey, true);
  }

  function onUp(e: PointerEvent): void {
    const wasDragging = ghost !== null;
    cleanup();
    if (wasDragging) dropTarget?.drop(e.clientX, e.clientY, media);
  }

  function onKey(e: KeyboardEvent): void {
    if (e.key === "Escape") {
      e.stopPropagation();
      cleanup();
      dropTarget?.leave();
    }
  }

  window.addEventListener("pointermove", onMove);
  window.addEventListener("pointerup", onUp);
  window.addEventListener("keydown", onKey, true);
}

function makeTransitionGhost(item: DragTransition): HTMLElement {
  const ghost = document.createElement("div");
  ghost.className =
    "pointer-events-none fixed z-50 flex items-center gap-1.5 rounded-md " +
    "border border-violet-500 bg-zinc-900/90 px-2 py-1 shadow-xl";
  const glyph = document.createElement("span");
  glyph.textContent = "⧗";
  glyph.className = "text-violet-400";
  ghost.appendChild(glyph);
  const label = document.createElement("span");
  label.textContent = item.label;
  label.className = "text-xs text-zinc-200";
  ghost.appendChild(label);
  document.body.appendChild(ghost);
  return ghost;
}

/**
 * Begin dragging a transition from the Transitions tab. Same pointer
 * mechanics as pool drags; the timeline highlights the cut under the
 * pointer and binds the transition on release.
 */
export function beginTransitionDrag(
  down: PointerEvent,
  item: DragTransition,
): void {
  if (down.button !== 0) return;
  const startX = down.clientX;
  const startY = down.clientY;
  let ghost: HTMLElement | null = null;

  function onMove(e: PointerEvent): void {
    if (!ghost) {
      const dx = e.clientX - startX;
      const dy = e.clientY - startY;
      if (dx * dx + dy * dy < DRAG_THRESHOLD_PX * DRAG_THRESHOLD_PX) return;
      ghost = makeTransitionGhost(item);
    }
    ghost.style.left = `${e.clientX + 12}px`;
    ghost.style.top = `${e.clientY + 10}px`;
    transitionTarget?.over(e.clientX, e.clientY, item);
  }

  function cleanup(): void {
    ghost?.remove();
    ghost = null;
    window.removeEventListener("pointermove", onMove);
    window.removeEventListener("pointerup", onUp);
    window.removeEventListener("keydown", onKey, true);
  }

  function onUp(e: PointerEvent): void {
    const wasDragging = ghost !== null;
    cleanup();
    if (wasDragging) transitionTarget?.drop(e.clientX, e.clientY, item);
  }

  function onKey(e: KeyboardEvent): void {
    if (e.key === "Escape") {
      e.stopPropagation();
      cleanup();
      transitionTarget?.leave();
    }
  }

  window.addEventListener("pointermove", onMove);
  window.addEventListener("pointerup", onUp);
  window.addEventListener("keydown", onKey, true);
}
