// Transient notification toasts (import rejections, engine errors, …).

import { create } from "zustand";

export type ToastKind = "info" | "error";

export interface Toast {
  id: number;
  kind: ToastKind;
  message: string;
}

const TOAST_LIFETIME_MS = 4500;

interface ToastState {
  toasts: Toast[];
  push: (message: string, kind?: ToastKind) => void;
  dismiss: (id: number) => void;
}

let nextId = 1;

export const useToastStore = create<ToastState>((set) => ({
  toasts: [],
  push: (message, kind = "info") => {
    const id = nextId++;
    set((state) => ({ toasts: [...state.toasts, { id, kind, message }] }));
    setTimeout(() => {
      useToastStore.getState().dismiss(id);
    }, TOAST_LIFETIME_MS);
  },
  dismiss: (id) =>
    set((state) => ({ toasts: state.toasts.filter((t) => t.id !== id) })),
}));

/** Convenience for non-React callers (stores, controllers). */
export function toast(message: string, kind: ToastKind = "info"): void {
  useToastStore.getState().push(message, kind);
}
