# Cutty — Development Plan

A fast, lightweight, CapCut-style video editor for Linux. Native performance, no Electron bloat.

---

## 1. Positioning (read this first)

CapCut's magic is not its 500 features. It's a tight core loop: **import → cut → captions → transitions → text → music → export**, wrapped in a UI that never makes you think. Kdenlive and Shotcut are powerful traditional NLEs, but they're clunky for short-form/social work and feel like 2010.

**Cutty's wedge: the fastest way to edit short-form and social video on Linux**, with a full CapCut-style layout so it also works as a general editor. First-class 9:16 support, one-click auto-captions, a real transition library, and export presets for TikTok/Shorts/Reels.

Scope discipline: this is not a 1:1 CapCut clone (that's a multi-year team effort). It's the 20% of CapCut that delivers 80% of the value, built to be genuinely fast. The phases below are gates — nothing from Phase N+1 starts before Phase N acceptance passes.

---

## 2. Tech stack

| Layer | Choice | Why |
|---|---|---|
| App shell | **Tauri 2** | ~15MB base binary, uses system WebKitGTK (Wayland-native, works well on Hyprland), Rust backend built in. Electron would idle at 200MB+ RAM and contradict "not heavy". |
| Frontend | **React 18 + TypeScript (strict) + Tailwind + Zustand** | CapCut's UI is a web-style dark panel layout — trivially faithful in web tech. Zustand mirrors engine state. |
| Timeline rendering | **Custom `<canvas>` renderer** | DOM cannot handle hundreds of clips + filmstrips + waveforms at 60fps. Canvas with virtualization can. |
| Media I/O | **FFmpeg via `ffmpeg-sidecar`** (MVP) → `ffmpeg-next` bindings later | Sidecar spawns the ffmpeg CLI and streams raw frames — avoids C linking pain, ships fast. Upgrade to library bindings when frame-accuracy demands it. |
| GPU compositing | **wgpu + WGSL shaders** | All compositing, transforms, effects, transitions, chroma key, LUTs run on GPU. Vulkan-first on Linux. |
| Audio | **cpal** (output) + **symphonia** (decode) | Sample-accurate mixing graph in Rust. Audio is the master clock. |
| Captions | **whisper-rs** (whisper.cpp bindings) | Native, no Python dependency. Word-level timestamps for karaoke-style captions. Same pipeline concept as podclip, just in-process. |
| Transitions | **gl-transitions ported to WGSL** | MIT-licensed library of 60+ GLSL transitions — an instant CapCut-grade transition catalog for the cost of a port. |
| Project format | **JSON via serde**, `.cutty` extension | Human-readable, diffable, easy migrations. |

**Rejected:** Electron (bloat), Qt/C++ + MLT (the Kdenlive route — mature but slow to iterate, inherits MLT's constraints), pure egui/iced (lightweight but recreating CapCut's polished UI would be painful and look off).

---

## 3. Architecture

### Core rule: single source of truth

The **Rust engine owns all project and timeline state**. The frontend is a view: it sends commands (`SplitClip`, `MoveClip`, `AddTransition`) over IPC and renders state events. Editing logic never lives in TypeScript. This one rule prevents the classic two-sources-of-truth mess that kills editor projects.

### Preview pipeline (the hard part — solved with proxies)

1. **On import:** probe the file → background jobs generate a 720p H.264 proxy, filmstrip thumbnails, and audio peak data. Progress events stream to the UI.
2. **Playback:** engine decodes proxy frames around the playhead into an LRU frame cache → wgpu composites all tracks + effects at preview resolution → readback → JPEG encode (turbojpeg) → binary IPC → frontend paints to the player canvas.
3. **Budget check:** 720p@30 as JPEG ≈ 2–4 MB/s. Comfortable over Tauri 2's raw/binary IPC.
4. **Upgrade path (only if needed):** shared-memory frame transport, or a native wgpu surface embedded under the webview. Do not build this first — proxies + JPEG will carry you through Phase 3.

### A/V sync

Audio is the master clock from day one. The audio thread mixes sample-accurately; video frame presentation chases the audio clock. This is the standard approach and the only one that doesn't drift.

### Export

The render graph re-runs at full resolution against **original media** (not proxies), piping raw frames into an ffmpeg encoder process. Hardware encode detection at startup: VAAPI (Intel/AMD) and NVENC (NVIDIA), with libx264 fallback. **Smart-copy path:** when a clip has no effects/transforms, use lossless stream-copy trim/concat — simple cuts export near-instantly.

### Data model sketch

```
Project  { settings: {width, height, fps}, media: [MediaRef], tracks: [Track] }
Track    { kind: Video | Audio | Text, locked, muted, clips: [Clip] }
Clip     { media_id, timeline_in, timeline_out, source_in, source_out,
           transform {x, y, scale, rotation}, opacity, speed, volume,
           effects: [Effect], keyframes: { prop → [Keyframe {t, value, easing}] },
           transition_out: Option<Transition> }
```

### Undo/redo

Every mutation is a `Command` with `apply` / `invert`. The engine keeps the stack. No exceptions — this is enforced from the first timeline operation.

---

## 4. UI spec (CapCut layout)

Dark theme by default. Panel layout matching CapCut:

- **Top bar:** menu, project name, autosave indicator, **Export** button top-right.
- **Left — Media pool:** tabs (Import / Library), thumbnail grid, drag-to-timeline.
- **Center — Player:** canvas preview, transport controls, timecode, fit/fill toggle, aspect selector, safe-area guides for 9:16.
- **Right — Inspector:** tabs = Video (transform, blend, opacity) · Audio (volume, fade) · Speed · Animation (in/out/loop presets + keyframes) · Adjustment (basic color + LUT with intensity slider).
- **Bottom — Timeline:** toolbar (split, delete, undo/redo, snap toggle, zoom slider), track headers (lock/mute/hide), canvas-rendered clips with filmstrips and waveforms, playhead, drag-trim handles, snapping, multi-select, ripple delete.

**Keyboard shortcuts:** `Space` play/pause · `S` / `Ctrl+B` split · `Del` delete · `Q`/`W` trim to playhead · `←`/`→` frame step · `+`/`-` zoom · `Home`/`End` · `Ctrl+Z`/`Ctrl+Shift+Z`.

---

## 5. Roadmap

### Phase 0 — Pipeline spike (1–2 weeks)
Kill the biggest risk before building any editor UI.

- [x] Tauri 2 app boots with static panel layout shell (no functionality)
- [x] Probe media files (duration, streams, resolution, fps)
- [x] Background 720p proxy generation with progress events
- [x] Play video with synced audio in the player canvas; seek slider; frame stepping
- [x] Export a trim via ffmpeg stream copy

**Acceptance:** a 4K source plays smoothly ≥30fps via proxy · seek responds <100ms · A/V drift <40ms over 60s · trimmed export opens correctly in mpv.

### Phase 1 — MVP editor (3–5 weeks)

- [x] Media pool with import + thumbnails, drag to timeline
- [x] One video track + one audio track
- [x] Split, trim (drag handles), move, delete, ripple delete, snapping
- [x] Undo/redo (command system)
- [x] Player synced to timeline
- [x] Export dialog: resolution/fps/bitrate presets incl. **1080×1920 TikTok/Shorts preset**, hardware encode detection
- [x] Project save/load (`.cutty`), autosave, crash recovery

**Acceptance:** cut a 10-minute screen recording into a 60s short and export it · timeline stays 60fps with 50 clips · no data loss on kill -9 (autosave).

### Phase 2 — The CapCut feel (4–6 weeks)

- [x] Multi-track video with compositing (opacity, blend modes)
- [ ] Transitions: fade/dissolve + ~15 ported gl-transitions; drag onto cut points; duration handle
- [ ] Text clips: font, size, color, stroke, shadow, position; a few text style presets
- [ ] Multiple audio tracks, volume keyframes, fade in/out, extract-audio-from-video
- [ ] Image and GIF/WebM overlay support
- [ ] Filmstrip thumbnails + waveforms rendered on clips

**Acceptance:** replicate a typical CapCut-style short (b-roll, text overlays, transitions, music) end to end without leaving Cutty.

### Phase 3 — Motion & color (4–6 weeks)

- [ ] Keyframes on transform/opacity with easing curves; animation presets (fade/slide/zoom/shake, in/out/loop)
- [ ] Speed: constant 0.1×–10× with pitch-corrected audio (atempo chains); freeze frame
- [ ] Effects: brightness/contrast/saturation/temperature, blur, vignette
- [ ] Chroma key (green screen) — cheap as a WGSL shader, high perceived value
- [ ] LUT support (`.cube`) with intensity slider

**Acceptance:** a keyframed zoom + LUT + green-screen clip previews in realtime and exports pixel-identical to preview.

### Phase 4 — The killer features (ongoing)

- [ ] **Auto-captions:** whisper.cpp with word-level timestamps, styled caption templates, karaoke highlight animation. This is THE CapCut feature and the #1 reason creators use it.
- [ ] Caption editor: fix text, split lines, restyle
- [ ] Beat markers for music sync (onset detection)
- [ ] Noise reduction (RNNoise), audio ducking under voice
- [ ] Sticker/overlay support (local folder + GIF/WebM first; Lottie later)
- [ ] Project templates
- [ ] Stretch: background removal (ONNX Runtime + RVM), TTS voiceover

### Phase 5 — Ship

- [ ] AUR package (`cutty-git`, then `cutty-bin`)
- [ ] Flatpak for the rest of Linux
- [ ] Landing page + demo video (dogfood rule: the demo is edited in Cutty)
- [ ] Later: Windows/macOS builds — Tauri makes this mostly free

---

## 6. Performance budget (enforced from day one)

| Metric | Budget |
|---|---|
| Cold start | < 1.5s |
| Idle RAM with project open | < 300MB |
| Proxy playback | ≥ 30fps @ 720p |
| UI framerate | 60fps, always |
| Seek response | < 100ms |
| Export speed | ≥ 1× realtime 1080p with hw encode |

Any regression on these blocks the merge. This is what "extremely fast and not heavy" means in practice — a number, not a vibe.

---

## 7. Risks & mitigations

- **Webview frame transport too slow** → proxies + JPEG stream first; shared memory or native surface as a known fallback. Don't prematurely optimize.
- **A/V sync drift** → audio is master clock from Phase 0; test with 10+ minute takes early.
- **Seek accuracy with ffmpeg-sidecar** → acceptable for MVP; migrate hot paths to `ffmpeg-next` bindings when frame-exactness matters (Phase 3).
- **FFmpeg licensing** → depend on system ffmpeg on Arch (dynamic). GPL is fine for an open-source project; revisit only if you ever sell proprietary binaries.
- **Wayland quirks on Hyprland** → WebKitGTK is Wayland-native; test drag-and-drop from file managers early (historically the fiddly part on Wayland).
- **Scope creep** → the phases are gates. The screenshot's HSL/Curves/Color-wheel panels are Phase 3+, not Phase 1.

---

## 8. Repo layout

```
cutty/
├── CLAUDE.md                 # Claude Code project rules (permanent)
├── PLAN.md                   # this file
├── src/                      # React frontend
│   ├── components/           # MediaPool/ Player/ Inspector/ ExportDialog/
│   ├── timeline/             # canvas renderer + interaction handlers
│   └── state/                # zustand store mirroring engine state
├── src-tauri/
│   ├── src/main.rs           # IPC surface only — thin
│   └── crates/
│       ├── cutty-engine/     # project model, timeline ops, commands, undo
│       ├── cutty-media/      # ffmpeg: probe, decode, proxy, thumbs, export
│       ├── cutty-gpu/        # wgpu compositor, WGSL effect/transition shaders
│       ├── cutty-audio/      # cpal + symphonia, mixing graph
│       └── cutty-captions/   # whisper-rs
└── assets/shaders/           # WGSL transitions & effects
```

---

## 9. How to build this with Claude Code

1. Create an empty repo, drop in `CLAUDE.md` and `PLAN.md`.
2. Paste `kickoff-prompt.md` → builds Phase 0.
3. After that: **one feature per session**, always referencing the phase and its acceptance criteria ("Implement split + ripple delete from PLAN.md Phase 1, with engine tests").
4. Make Claude write unit tests for every timeline operation — timeline math is where silent bugs live.
5. Commit per working feature. Never let a session end with a broken build.
