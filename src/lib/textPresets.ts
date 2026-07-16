// Built-in text style presets (the left-panel Text tab). Styles are
// engine TextStyle values; placement is fractional so presets land
// sensibly in any project aspect (16:9 and 9:16 alike). The CSS preview
// hints approximate stroke/shadow for the tile — the engine render is
// the truth.

import type { TextAlign, TextSpec, TextStyle } from "./engineIpc";

export interface TextPreset {
  id: string;
  label: string;
  /** Content the tile shows and the new clip starts with. */
  sampleContent: string;
  style: TextStyle;
  /** Placement as a fraction of project width/height from the center
   * (0,0 = centered; +y down). Omitted = centered. */
  place?: { xFrac: number; yFrac: number };
  /** CSS for the tile preview (approximation of the engine render). */
  css: React.CSSProperties;
}

const base: TextStyle = {
  fontFamily: "",
  weight: 700,
  fontSize: 72,
  fill: "#ffffff",
  strokeColor: "#000000",
  strokeWidth: 6,
  shadowColor: "#000000",
  shadowOffsetX: 0,
  shadowOffsetY: 4,
  shadowAlpha: 0.35,
  align: "center" as TextAlign,
};

/** The default style `T` / "Add text" uses. */
export const DEFAULT_TEXT: TextSpec = {
  content: "Add text",
  style: { ...base },
};

/** Default duration of a freshly-added text clip, seconds. */
export const DEFAULT_TEXT_DURATION = 3;

export const TEXT_PRESETS: TextPreset[] = [
  {
    id: "classic-bold",
    label: "Classic Bold",
    sampleContent: "Add text",
    style: { ...base },
    css: {
      fontWeight: 800,
      color: "#fff",
      WebkitTextStroke: "1.6px #000",
      textShadow: "0 2px 4px rgba(0,0,0,0.55)",
    },
  },
  {
    id: "pop-yellow",
    label: "Pop Yellow",
    sampleContent: "SUBSCRIBE",
    style: {
      ...base,
      fill: "#ffdd00",
      strokeWidth: 5,
      shadowOffsetX: 0,
      shadowOffsetY: 5,
      shadowAlpha: 0.5,
      weight: 900,
    },
    css: {
      fontWeight: 900,
      color: "#ffdd00",
      WebkitTextStroke: "1.5px #000",
      textShadow: "0 2.5px 3px rgba(0,0,0,0.6)",
      letterSpacing: "0.02em",
    },
  },
  {
    id: "lower-third",
    label: "Lower Third",
    sampleContent: "Jane Doe\nProduct Designer",
    style: {
      ...base,
      fontSize: 40,
      weight: 500,
      strokeWidth: 0,
      shadowOffsetY: 2,
      shadowAlpha: 0.55,
      align: "left",
    },
    place: { xFrac: -0.24, yFrac: 0.32 },
    css: {
      fontWeight: 500,
      fontSize: "11px",
      color: "#fff",
      textShadow: "0 1px 2px rgba(0,0,0,0.7)",
      textAlign: "left",
      lineHeight: 1.25,
    },
  },
  {
    id: "chromatic",
    label: "Chromatic",
    sampleContent: "GLOW UP",
    style: {
      ...base,
      fill: "#4df3e8",
      strokeWidth: 0,
      shadowColor: "#ff3d8b",
      shadowOffsetX: 4,
      shadowOffsetY: 4,
      shadowAlpha: 0.9,
      weight: 800,
    },
    css: {
      fontWeight: 800,
      color: "#4df3e8",
      textShadow: "2px 2px 0 rgba(255,61,139,0.9)",
      letterSpacing: "0.03em",
    },
  },
  {
    id: "serif-quote",
    label: "Serif Quote",
    sampleContent: "“Make it feel easy.”",
    style: {
      ...base,
      fontFamily: "serif",
      fontSize: 56,
      weight: 400,
      strokeWidth: 0,
      shadowOffsetY: 3,
      shadowAlpha: 0.5,
    },
    css: {
      fontFamily: "Georgia, 'Times New Roman', serif",
      fontWeight: 400,
      color: "#fff",
      textShadow: "0 1.5px 3px rgba(0,0,0,0.65)",
    },
  },
  {
    id: "mono-stamp",
    label: "Mono Stamp",
    sampleContent: "REC ● 00:00",
    style: {
      ...base,
      fontFamily: "monospace",
      fontSize: 44,
      weight: 700,
      strokeWidth: 0,
      shadowColor: "#000000",
      shadowOffsetX: 3,
      shadowOffsetY: 3,
      shadowAlpha: 1,
    },
    place: { xFrac: -0.22, yFrac: -0.36 },
    css: {
      fontFamily: "ui-monospace, 'JetBrains Mono', monospace",
      fontWeight: 700,
      fontSize: "12px",
      color: "#fff",
      textShadow: "1.5px 1.5px 0 #000",
    },
  },
];
