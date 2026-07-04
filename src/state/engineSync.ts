// One-time wiring of engine state events into the project store.

import type { EngineSnapshot } from "../lib/engineIpc";
import { engineGetState, onEngineState } from "../lib/engineIpc";
import { useProjectStore } from "./projectStore";

let started = false;

/**
 * Subscribe to `engine://project` events and fetch the initial snapshot.
 * Idempotent (safe under React StrictMode double-effects); the
 * subscription lives for the app's lifetime.
 */
export function startEngineSync(): void {
  if (started) return;
  started = true;
  const apply = (s: EngineSnapshot) =>
    useProjectStore.getState().applySnapshot(s);

  void onEngineState(apply).catch((err: unknown) => {
    console.error("engine event subscription failed", err);
  });
  void engineGetState()
    .then(apply)
    .catch((err: unknown) => {
      console.error("initial engine state fetch failed", err);
    });
}
