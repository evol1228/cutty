<h1 align="center">Cutty</h1>

<p align="center">
  <strong>A dead-fast, CapCut-style video editor for Linux — built in Rust for maximum performance.</strong>
</p>

<!-- TODO: Add a screenshot once the Phase 2 UI is presentable -->
<!-- <p align="center"><img src="assets/screenshot.png" alt="Cutty" width="800" /></p> -->

---

> **Very early beta.** Cutty is in very early development. It already does a full import → cut → export loop, but expect missing features, rough edges, and breaking changes to the project format. Don't trust it with your only copy of anything yet.

## What is Cutty?

Cutty is a lightweight video editor for Linux with a CapCut-style layout: media pool on the left, player in the center, inspector on the right, timeline at the bottom. It's aimed at short-form and social video — the import → cut → captions → music → export loop — without the weight of a traditional NLE and without Electron.

The entire editing engine is Rust. The frontend is just a view: every edit is a command sent to the engine, which owns all project and timeline state. Performance isn't a goal, it's a budget — regressions against the numbers below block the merge.

## What works today

- **Import & media pool** — drop files in, and background jobs probe them and generate 720p preview proxies and thumbnails while you keep working
- **Timeline editing** — split, trim with drag handles, move, delete, ripple delete, and snapping on a custom canvas renderer (no DOM clips, stays at 60fps)
- **Full undo/redo** — every mutation is an invertible command; nothing escapes the history
- **Playback** — sample-accurate audio mixing with audio as the master clock; video chases it, so long takes don't drift
- **Fast seeking** — in-process libav decode instead of spawning processes; cold seeks measure ~16 ms median, well under the 100 ms budget
- **Export** — renders from original media (not proxies), with VAAPI/NVENC hardware encode detected at startup and libx264 fallback, including a 1080×1920 TikTok/Shorts preset
- **Projects** — human-readable `.cutty` JSON files with autosave and crash recovery

Next up (in progress): multi-track compositing, transitions, text clips, and further out, keyframes, chroma key, LUTs, and whisper.cpp auto-captions. See [PLAN.md](PLAN.md) for the full roadmap.

## Performance budget

These numbers are enforced from day one — any regression blocks the merge.

| Metric                    | Budget                            |
|---------------------------|-----------------------------------|
| Cold start                | < 1.5s                            |
| Idle RAM with project open| < 300MB                           |
| Proxy playback            | ≥ 30fps @ 720p                    |
| UI framerate              | 60fps, always                     |
| Seek response             | < 100ms                           |
| Export speed              | ≥ 1× realtime 1080p with hw encode|

## Tech Stack

- **App shell:** [Tauri 2](https://tauri.app) — ~15MB base binary, Wayland-native via WebKitGTK
- **Engine:** Rust workspace crates — `cutty-engine` (model, commands, undo), `cutty-media` (ffmpeg I/O), `cutty-gpu` (compositing), `cutty-audio` (mixing)
- **Media I/O:** [ffmpeg-sidecar](https://github.com/nathanbabcock/ffmpeg-sidecar) for transcode jobs + [ffmpeg-the-third](https://github.com/shssoichiro/ffmpeg-the-third) for in-process interactive decode
- **GPU:** [wgpu](https://wgpu.rs) with WGSL shaders — all pixel work on the GPU
- **Audio:** [cpal](https://github.com/rustaudio/cpal) output + [symphonia](https://github.com/pdeljanov/Symphonia) decode
- **Frontend:** [React 19](https://react.dev) + TypeScript (strict) + [Tailwind CSS](https://tailwindcss.com) v4 + [Zustand](https://github.com/pmndrs/zustand)

## Getting Started

Cutty targets Arch Linux (Wayland-first) but should build on any modern distro with the equivalent packages.

```bash
# Prerequisites (Arch)
sudo pacman -S --needed base-devel webkit2gtk-4.1 ffmpeg libjpeg-turbo clang pkgconf nodejs npm rustup
rustup default stable

# Clone and install
git clone https://github.com/evol1228/cutty.git
cd cutty
npm install

# Run the app in dev mode
npm run tauri dev
```

System ffmpeg is required at runtime — Cutty fails with a clear error if it's missing.

## Development

Engine tests and lint run from the Rust workspace:

```bash
# Engine tests (every timeline operation is unit-tested)
cd src-tauri && cargo test --workspace

# Lint (must be clean before commit)
cargo clippy --workspace -- -D warnings && cd .. && npm run lint
```

## License

[Cutty Source-Available License](LICENSE) — creators can use Cutty freely, including for monetized content, with no license or credit needed just for using it. The moment your content shows or mentions Cutty, proper credit is required: the tool's name, [@evol1228](https://github.com/evol1228), and a link to this repo. Forks must stay source-available under this license with the same credit. Reusing parts of the code in open-source projects is fine with credit; closed-source reuse and any commercial use require contacting [@evol1228](https://github.com/evol1228) first (commercial use = paid license, and credit rules still apply when Cutty is shown or mentioned).

---

<p align="center">
  Built by <a href="https://github.com/evol1228">@evol1228</a>
</p>
