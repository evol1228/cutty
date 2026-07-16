//! The shared frame renderer: **one** implementation of "the output frame
//! at time `t`", consumed by both render frontends.
//!
//! - Preview (playback/scrub) renders proxies at preview resolution.
//! - Export renders originals at full output resolution.
//!
//! Everything that defines the picture lives here — the frame sampling
//! rule, layer geometry, opacity/blend parameters, and the GPU composite —
//! so preview == export holds *by construction*, enforced by the
//! golden-frame tests (`tests/golden_frames.rs`).
//!
//! # The frame sampling rule
//!
//! The output frame grid drives everything. For an output frame at time
//! `t`, [`cutty_engine::resolve_video_layers`] gives the active clips
//! bottom→top; for each layer, the frame shown is the **latest decoded
//! source frame with pts ≤ the clip's source time at `t`** (floor on the
//! source frame grid). A source that runs dry before its clip ends holds
//! its last frame (matching the fast export path's `tpad` semantics).
//!
//! Decoders advance forward frame-by-frame when the target is near
//! (sequential playback, small hops), and seek when it is not (scrubs,
//! jump cuts). Decode sessions are keyed by **clip**: normally that means
//! one decoder per on-screen source, and a session migrates ("adoption")
//! to the next clip of the same media at a cut, so split points still
//! flow through one decoder. During a transition overlap the two clips
//! hold two sessions — even when they read the same file at different
//! offsets — because both stream simultaneously.
//!
//! # Transitions
//!
//! Inside a transition span ([`cutty_engine::resolve_track_visuals`]) a
//! track plans a *pair*: both sides decode and upload, and the GPU runs
//! the registered transition shader between them
//! ([`cutty_gpu::Visual::Transition`]). Extended source times implement
//! the handle semantics — the outgoing side runs past its `source_out`
//! (decoders hold the last frame at end of file: the freeze case), the
//! incoming side runs negative (clamped to frame 0: the incoming
//! freeze). Clip blend modes are ignored *during* the overlap (the pair
//! composites as one normal layer); opacity and transforms apply.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use cutty_engine::{
    parse_hex_color, resolve_text_layers, resolve_track_visuals, ActiveClip, Clip, Project,
    ProjectSettings, TextAlign, TextSpec, TrackVisual,
};
use cutty_gpu::{BlendMode as GpuBlend, Compositor, Layer, SourceTexture, Target, Visual};
use cutty_text::{RasterSpec, TextRasterizer};

use crate::decode::SourceDecoder;
use crate::error::MediaError;

/// How many frames ahead a decoder will roll forward instead of seeking.
/// A seek costs up to one GOP of catch-up decode (proxies are `-g 30`),
/// so rolling forward is cheaper up to about that.
const FORWARD_WINDOW_FRAMES: i64 = 32;

/// Tolerance when mapping times to frame indices (matches `decode.rs`).
const PTS_EPS: f64 = 1e-6;

/// Renderer counters (logged by the frontends; asserted by perf tests).
#[derive(Debug, Default, Clone, Copy)]
pub struct RenderStats {
    /// Frames composited.
    pub frames: u64,
    /// Decoders opened synchronously inside a render (a potential hitch
    /// during playback; prefetch exists to keep this rare).
    pub cold_opens: u64,
    /// In-stream seeks (scrubs, jump cuts, backward samples).
    pub seeks: u64,
    /// Frames decoded rolling forward.
    pub forward_decodes: u64,
    /// Text blocks rasterized (cache misses). Steady-state playback of a
    /// static text layer must not grow this — the texture cache serves
    /// every following frame.
    pub text_rasterized: u64,
}

/// One open source: a decoder plus its GPU texture and what's in it.
struct SourceState {
    /// The media file this decoder reads — adoption matches on it.
    media_id: u64,
    decoder: SourceDecoder,
    texture: SourceTexture,
    /// Source frame index currently uploaded to `texture`.
    uploaded_idx: Option<i64>,
}

impl SourceState {
    fn new(compositor: &Compositor, media_id: u64, decoder: SourceDecoder) -> Self {
        let texture = compositor.create_source(decoder.width(), decoder.height());
        Self {
            media_id,
            decoder,
            texture,
            uploaded_idx: None,
        }
    }

    fn idx_of(&self, pts: f64) -> i64 {
        (pts * self.decoder.fps() + 0.5).floor() as i64
    }

    /// Make `texture` hold the latest source frame with pts ≤ `src_t`.
    /// Returns `false` when the stream has no frames at all (skip the
    /// layer). Holds the last frame past end of stream.
    fn sample(
        &mut self,
        compositor: &Compositor,
        src_t: f64,
        stats: &mut RenderStats,
    ) -> Result<bool, MediaError> {
        let fps = self.decoder.fps();
        let needed = (src_t.max(0.0) * fps + PTS_EPS).floor() as i64;

        if self.uploaded_idx == Some(needed) {
            return Ok(true);
        }

        // The decoder may already hold the wanted frame un-uploaded (a
        // freshly installed prefetched decoder sits exactly here).
        if let Some(pts) = self.decoder.current_pts_sec() {
            if self.idx_of(pts) == needed {
                return self.upload_current(compositor);
            }
        }

        // Past the end of a dried-up stream: hold the last frame.
        if self.decoder.is_exhausted() {
            if let Some(idx) = self.uploaded_idx {
                if needed > idx {
                    return Ok(true);
                }
            }
        }

        // Near-forward target: roll the stream forward (cheaper than a
        // seek). `next_pts_hint` is exact on CFR sources (all proxies);
        // VFR originals fall back to seeking.
        let forward_gap = self
            .decoder
            .next_pts_hint()
            .map(|next| needed - self.idx_of(next));
        if let Some(gap) = forward_gap {
            if (0..=FORWARD_WINDOW_FRAMES).contains(&gap) {
                loop {
                    let next_within = self
                        .decoder
                        .next_pts_hint()
                        .is_some_and(|next| self.idx_of(next) <= needed);
                    if !next_within {
                        break;
                    }
                    stats.forward_decodes += 1;
                    if self.decoder.next_frame()?.is_none() {
                        break; // dried up: hold what we have
                    }
                }
                return self.upload_current(compositor);
            }
        }

        // Everything else (backward, far forward, fresh or exhausted
        // stream): one seek positions the floor frame.
        stats.seeks += 1;
        let uploaded = {
            let Some(frame) = self.decoder.seek_to(src_t.max(0.0))? else {
                return Ok(self.uploaded_idx.is_some()); // no frames at all
            };
            let idx = (frame.pts_sec * fps + 0.5).floor() as i64;
            compositor.upload_rgba(&self.texture, frame.data, frame.stride);
            idx
        };
        self.uploaded_idx = Some(uploaded);
        Ok(true)
    }

    /// Upload the decoder's current frame (if any) to the texture.
    fn upload_current(&mut self, compositor: &Compositor) -> Result<bool, MediaError> {
        let fps = self.decoder.fps();
        let uploaded = {
            let Some(frame) = self.decoder.current_frame()? else {
                return Ok(self.uploaded_idx.is_some());
            };
            let idx = (frame.pts_sec * fps + 0.5).floor() as i64;
            compositor.upload_rgba(&self.texture, frame.data, frame.stride);
            idx
        };
        self.uploaded_idx = Some(uploaded);
        Ok(true)
    }
}

/// Tear decode sessions down off the caller's thread: destroying a
/// frame-threaded libav context joins its worker pool (multiple
/// milliseconds), which would stall the playback control thread right at
/// a cut. Spawn failure falls back to an inline drop.
fn dispose_sources(dropped: Vec<SourceState>) {
    if dropped.is_empty() {
        return;
    }
    // Spawn failure drops the closure — and with it the sessions —
    // inline, which is the right fallback.
    let _ = std::thread::Builder::new()
        .name("cutty-decoder-drop".into())
        .spawn(move || drop(dropped));
}

// ---------------------------------------------------------------------
// Text layers: rasterized once, cached as GPU textures
// ---------------------------------------------------------------------

/// Text texture cache budget, bytes (a full-screen 4K raster is ~33 MB;
/// preview rasters are a couple MB — this comfortably holds a busy
/// timeline's worth).
const TEXT_CACHE_BYTES: usize = 256 << 20;

/// One cached rasterization: `None` texture = the content rasterized to
/// nothing (empty/whitespace), cached too so empty clips don't re-shape
/// every frame.
struct TextEntry {
    texture: Option<(SourceTexture, (f32, f32))>, // texture + block center
    bytes: usize,
    last_used: u64,
}

/// Rasterized-text textures keyed by content + style + raster scale —
/// invalidation is by key change (an edit or zoom mints a new key; stale
/// entries age out by LRU under the byte budget).
struct TextCache {
    rasterizer: TextRasterizer,
    entries: HashMap<u64, TextEntry>,
    bytes: usize,
    tick: u64,
}

impl TextCache {
    fn new() -> Self {
        Self {
            rasterizer: TextRasterizer::new(),
            entries: HashMap::new(),
            bytes: 0,
            tick: 0,
        }
    }

    /// Evict least-recently-used entries (never ones touched this tick)
    /// until the byte budget holds.
    fn enforce_budget(&mut self) {
        while self.bytes > TEXT_CACHE_BYTES {
            let victim = self
                .entries
                .iter()
                .filter(|(_, e)| e.last_used < self.tick)
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| *k);
            let Some(key) = victim else { break };
            if let Some(entry) = self.entries.remove(&key) {
                self.bytes -= entry.bytes;
            }
        }
    }
}

/// Cache key for one rasterization: content, full style, and the
/// quantized pixel scale (output scale × clip scale — the raster is
/// resolution-exact, never upscaled).
fn text_raster_key(text: &TextSpec, px_scale: f64) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.content.hash(&mut hasher);
    let style = &text.style;
    style.font_family.hash(&mut hasher);
    style.weight.hash(&mut hasher);
    for v in [
        style.font_size,
        style.stroke_width,
        style.shadow_offset_x,
        style.shadow_offset_y,
        style.shadow_alpha,
    ] {
        v.to_bits().hash(&mut hasher);
    }
    style.fill.hash(&mut hasher);
    style.stroke_color.hash(&mut hasher);
    style.shadow_color.hash(&mut hasher);
    (style.align as u8).hash(&mut hasher);
    // ~0.1% steps: fine enough that quantization is invisible, coarse
    // enough that a scale drag reuses entries when it revisits values.
    ((px_scale * 1024.0).round() as u64).hash(&mut hasher);
    hasher.finish()
}

/// Model style → rasterizer request, everything scaled to device pixels.
/// Colors were validated by the engine; a hand-edited file that slipped
/// through renders white/transparent rather than failing the frame.
fn raster_spec(text: &TextSpec, px_scale: f64) -> RasterSpec {
    let style = &text.style;
    let color = |s: &str, fallback: [u8; 4]| parse_hex_color(s).unwrap_or(fallback);
    RasterSpec {
        font_family: style.font_family.clone(),
        weight: style.weight,
        font_size: (style.font_size * px_scale) as f32,
        fill: color(&style.fill, [255, 255, 255, 255]),
        stroke_color: color(&style.stroke_color, [0, 0, 0, 0]),
        stroke_width: (style.stroke_width * px_scale) as f32,
        shadow_color: color(&style.shadow_color, [0, 0, 0, 0]),
        shadow_offset: (
            (style.shadow_offset_x * px_scale) as f32,
            (style.shadow_offset_y * px_scale) as f32,
        ),
        shadow_alpha: style.shadow_alpha as f32,
        align: match style.align {
            TextAlign::Left => cutty_text::TextAlign::Left,
            TextAlign::Center => cutty_text::TextAlign::Center,
            TextAlign::Right => cutty_text::TextAlign::Right,
        },
    }
}

/// Shared rasterizer for UI queries (font list, gizmo block measure).
/// The render paths own per-thread rasterizers; this one exists so the
/// IPC layer never blocks a render thread for a dropdown.
static UI_TEXT: std::sync::OnceLock<std::sync::Mutex<TextRasterizer>> = std::sync::OnceLock::new();

fn ui_text() -> &'static std::sync::Mutex<TextRasterizer> {
    UI_TEXT.get_or_init(|| std::sync::Mutex::new(TextRasterizer::new()))
}

/// Distinct system font family names, sorted (the Inspector dropdown).
pub fn text_font_families() -> Vec<String> {
    ui_text()
        .lock()
        .expect("text rasterizer poisoned")
        .font_families()
}

/// Size of a text block in **project pixels** at transform scale 1 — the
/// box the player gizmo draws. Uses the exact layout the renderer
/// rasterizes, so the box always hugs the pixels.
pub fn measure_text_block(text: &TextSpec) -> (f64, f64) {
    let spec = raster_spec(text, 1.0);
    let (w, h) = ui_text()
        .lock()
        .expect("text rasterizer poisoned")
        .measure(&text.content, &spec);
    (f64::from(w), f64::from(h))
}

/// What a planned layer samples from: a decode session's texture (video)
/// or a cached text raster.
enum PlannedSource {
    /// Clip id — the decode-session key.
    Media(u64),
    /// Text-raster cache key.
    Text(u64),
}

/// A layer resolved and sampled, ready to composite (phase 1 of
/// `begin_frame`; phase 2 turns these into borrowed [`Layer`]s).
struct PlannedLayer {
    source: PlannedSource,
    center: (f32, f32),
    size: (f32, f32),
    rotation_rad: f32,
    opacity: f32,
    blend: GpuBlend,
}

/// One planned visual: a layer, or a sampled transition pair.
enum PlannedVisual {
    Layer(PlannedLayer),
    Transition {
        from: PlannedLayer,
        to: PlannedLayer,
        kind: u32,
        progress: f32,
    },
}

/// A composited output frame, borrowed from the readback buffer.
pub struct FrameSlice<'a> {
    pub width: u32,
    pub height: u32,
    /// Row pitch in bytes (≥ `width * 4`; GPU readbacks pad rows to 256).
    pub stride: usize,
    /// RGBA rows, `height * stride` bytes.
    pub data: &'a [u8],
}

/// Placement of a layer on the output, in output pixels. Pure math —
/// split out for unit tests.
///
/// The clip's source frame is fit inside the project canvas
/// (aspect-preserving contain, centered), then the clip transform is
/// applied in project space (x/y offset in project pixels, scale on the
/// fitted size), and the whole project canvas is uniformly fit into the
/// output. See [`cutty_engine::Transform`].
pub(crate) fn layer_placement(
    src_w: u32,
    src_h: u32,
    clip: &Clip,
    settings: &ProjectSettings,
    out_w: u32,
    out_h: u32,
) -> (f32, f32, f32, f32, f32) {
    let (pw, ph) = (settings.width as f64, settings.height as f64);
    let (ow, oh) = (f64::from(out_w), f64::from(out_h));
    // Project canvas → output mapping (letterboxed if aspects differ).
    let s = (ow / pw).min(oh / ph);
    let off_x = (ow - pw * s) / 2.0;
    let off_y = (oh - ph * s) / 2.0;
    // Source frame fit inside the project canvas.
    let base = (pw / f64::from(src_w)).min(ph / f64::from(src_h));
    let size_w = f64::from(src_w) * base * clip.transform.scale;
    let size_h = f64::from(src_h) * base * clip.transform.scale;
    let center_x = pw / 2.0 + clip.transform.x;
    let center_y = ph / 2.0 + clip.transform.y;
    (
        ((center_x * s) + off_x) as f32,
        ((center_y * s) + off_y) as f32,
        (size_w * s) as f32,
        (size_h * s) as f32,
        clip.transform.rotation.to_radians() as f32,
    )
}

fn gpu_blend(mode: cutty_engine::BlendMode) -> GpuBlend {
    match mode {
        cutty_engine::BlendMode::Normal => GpuBlend::Normal,
        cutty_engine::BlendMode::Multiply => GpuBlend::Multiply,
        cutty_engine::BlendMode::Screen => GpuBlend::Screen,
        cutty_engine::BlendMode::Overlay => GpuBlend::Overlay,
        cutty_engine::BlendMode::Add => GpuBlend::Add,
    }
}

/// Renders output frames for a project: decoders + GPU compositor behind
/// the frame sampling rule. Owned by one thread (playback control or the
/// export worker).
pub struct TimelineRenderer {
    compositor: Compositor,
    target: Target,
    out_w: u32,
    out_h: u32,
    /// `true`: any layer failure fails the frame (export). `false`: the
    /// layer is dropped and the message recorded (preview keeps playing).
    strict: bool,
    /// Decode sessions keyed by clip id (see the module docs on
    /// adoption). A clip name outlives its decoder only until the GC
    /// (`sync_sources`) runs.
    sources: HashMap<u64, SourceState>,
    /// Rasterized-text textures. `None` until the first text layer — a
    /// project without text never loads the font database.
    text: Option<TextCache>,
    /// Transition kinds already reported as unknown (fallback to fade).
    unknown_kinds: HashSet<String>,
    issues: Vec<String>,
    stats: RenderStats,
}

impl TimelineRenderer {
    /// Bring up the GPU and an output target of the given size.
    pub fn new(out_w: u32, out_h: u32, strict: bool) -> Result<Self, MediaError> {
        let compositor = Compositor::new().map_err(MediaError::Gpu)?;
        let target = compositor.create_target(out_w, out_h);
        Ok(Self {
            compositor,
            target,
            out_w,
            out_h,
            strict,
            sources: HashMap::new(),
            text: None,
            unknown_kinds: HashSet::new(),
            issues: Vec::new(),
            stats: RenderStats::default(),
        })
    }

    pub fn adapter_label(&self) -> String {
        self.compositor.adapter_label()
    }

    pub fn output_size(&self) -> (u32, u32) {
        (self.out_w, self.out_h)
    }

    pub fn stats(&self) -> RenderStats {
        self.stats
    }

    /// Non-fatal layer problems recorded since the last call (lenient
    /// mode only): missing sources, decode failures.
    pub fn take_issues(&mut self) -> Vec<String> {
        std::mem::take(&mut self.issues)
    }

    /// Whether a decode session exists for `clip_id` (installed prime or
    /// streaming).
    pub fn has_session(&self, clip_id: u64) -> bool {
        self.sources.contains_key(&clip_id)
    }

    /// Install a prefetched decoder for `clip_id` (reading `media_id`),
    /// positioned near `needed_src_t`. Dropped when the clip already has
    /// a session that reaches the target without seeking.
    pub fn offer_decoder(
        &mut self,
        clip_id: u64,
        media_id: u64,
        decoder: SourceDecoder,
        needed_src_t: f64,
    ) {
        if let Some(existing) = self.sources.get(&clip_id) {
            let fps = existing.decoder.fps();
            let needed = (needed_src_t.max(0.0) * fps + PTS_EPS).floor() as i64;
            let reachable = existing.uploaded_idx == Some(needed)
                || existing
                    .decoder
                    .current_pts_sec()
                    .is_some_and(|p| existing.idx_of(p) == needed)
                || existing.decoder.next_pts_hint().is_some_and(|next| {
                    let gap = needed - existing.idx_of(next);
                    (0..=FORWARD_WINDOW_FRAMES).contains(&gap)
                });
            if reachable {
                return; // the running decoder already covers this clip
            }
        }
        self.sources.insert(
            clip_id,
            SourceState::new(&self.compositor, media_id, decoder),
        );
    }

    /// Reconcile decode sessions with what plays now and soon: sessions
    /// migrate to the next clip of the same media first (a cut's
    /// continuation flows through the running decoder — no reopen, no
    /// seek), then everything not in `needed` closes. `needed` is
    /// `(clip id, media id)` for every active and upcoming clip.
    pub fn sync_sources(&mut self, needed: &[(u64, u64)]) {
        let needed_clips: std::collections::HashSet<u64> =
            needed.iter().map(|&(clip, _)| clip).collect();
        for &(clip, media) in needed {
            if self.sources.contains_key(&clip) {
                continue;
            }
            let orphan = self
                .sources
                .iter()
                .find(|(k, v)| !needed_clips.contains(*k) && v.media_id == media)
                .map(|(k, _)| *k);
            if let Some(k) = orphan {
                let state = self.sources.remove(&k).expect("just found");
                self.sources.insert(clip, state);
            }
        }
        let drop_keys: Vec<u64> = self
            .sources
            .keys()
            .filter(|k| !needed_clips.contains(k))
            .copied()
            .collect();
        let dropped: Vec<SourceState> = drop_keys
            .iter()
            .filter_map(|k| self.sources.remove(k))
            .collect();
        dispose_sources(dropped);
    }

    /// Close every decoder (source files may have changed on disk).
    pub fn clear_sources(&mut self) {
        dispose_sources(self.sources.drain().map(|(_, v)| v).collect());
    }

    /// Ensure a decode session for `clip_id` (adopting a same-media
    /// orphan when one exists, cold-opening otherwise) and sample it at
    /// `source_time`. `Ok(None)` = the layer contributes nothing this
    /// frame (missing file / empty stream, lenient mode).
    #[allow(clippy::too_many_arguments)]
    fn sample_side(
        &mut self,
        project: &Project,
        active: &ActiveClip,
        needed: &HashSet<u64>,
        path_of: &dyn Fn(u64) -> Option<PathBuf>,
        transition_side: bool,
    ) -> Result<Option<PlannedLayer>, MediaError> {
        let Some((_, clip)) = project.find_clip(active.clip_id) else {
            return Ok(None);
        };
        let key = active.clip_id.0;
        let Some(media) = clip.media_id.map(|m| m.0) else {
            return Ok(None); // text clips never reach the decode path
        };

        if !self.sources.contains_key(&key) {
            // Adopt a same-media session no planned clip owns (the
            // continuation flow when the pump's sync didn't run — e.g.
            // scrubbing straight to a cut).
            let orphan = self
                .sources
                .iter()
                .find(|(k, v)| !needed.contains(*k) && v.media_id == media)
                .map(|(k, _)| *k);
            if let Some(k) = orphan {
                let state = self.sources.remove(&k).expect("just found");
                self.sources.insert(key, state);
            } else {
                let Some(path) = path_of(media) else {
                    self.layer_problem(format!(
                        "no renderable file for media {media} (proxy still generating?)"
                    ))?;
                    return Ok(None);
                };
                self.stats.cold_opens += 1;
                match SourceDecoder::open(&path) {
                    Ok(decoder) => {
                        self.sources
                            .insert(key, SourceState::new(&self.compositor, media, decoder));
                    }
                    Err(e) => {
                        self.layer_problem(format!("open {} failed: {e}", path.display()))?;
                        return Ok(None);
                    }
                }
            }
        }

        let state = self.sources.get_mut(&key).expect("just ensured");
        match state.sample(&self.compositor, active.source_time, &mut self.stats) {
            Ok(true) => {
                let (cx, cy, w, h, rot) = layer_placement(
                    state.texture.width(),
                    state.texture.height(),
                    clip,
                    &project.settings,
                    self.out_w,
                    self.out_h,
                );
                Ok(Some(PlannedLayer {
                    source: PlannedSource::Media(key),
                    center: (cx, cy),
                    size: (w, h),
                    rotation_rad: rot,
                    opacity: clip.opacity as f32,
                    // Blend modes need the backdrop, which a transition
                    // intermediate doesn't have: the pair composites as
                    // one normal layer.
                    blend: if transition_side {
                        GpuBlend::Normal
                    } else {
                        gpu_blend(clip.blend_mode)
                    },
                }))
            }
            Ok(false) => Ok(None), // stream with no frames
            Err(e) => {
                // Broken decoder: drop it so the next frame reopens.
                self.sources.remove(&key);
                self.layer_problem(format!("decode failed: {e}"))?;
                Ok(None)
            }
        }
    }

    /// Plan one text layer: rasterize (or reuse) the styled block at the
    /// exact device pixel scale — output scale × clip scale, so 4K
    /// exports get 4K-sharp glyphs, never an upscaled preview raster —
    /// and place the texture 1:1 with output pixels. `None` = nothing
    /// visible (empty content, degenerate scale).
    fn plan_text_layer(&mut self, project: &Project, active: &ActiveClip) -> Option<PlannedLayer> {
        let (_, clip) = project.find_clip(active.clip_id)?;
        let text = clip.text.as_ref()?;
        let settings = &project.settings;
        let (pw, ph) = (settings.width as f64, settings.height as f64);
        let (ow, oh) = (f64::from(self.out_w), f64::from(self.out_h));
        let s = (ow / pw).min(oh / ph);
        let off_x = (ow - pw * s) / 2.0;
        let off_y = (oh - ph * s) / 2.0;
        let px_scale = s * clip.transform.scale;
        if !px_scale.is_finite() || px_scale <= 0.0 {
            return None;
        }

        let key = text_raster_key(text, px_scale);
        let cache = self.text.get_or_insert_with(TextCache::new);
        if !cache.entries.contains_key(&key) {
            self.stats.text_rasterized += 1;
            let raster = cache
                .rasterizer
                .rasterize(&text.content, &raster_spec(text, px_scale));
            let entry = match raster {
                Some(r) => {
                    let texture = self.compositor.create_source(r.width, r.height);
                    self.compositor
                        .upload_rgba(&texture, &r.data, r.width as usize * 4);
                    TextEntry {
                        bytes: r.data.len(),
                        texture: Some((texture, r.block_center)),
                        last_used: cache.tick,
                    }
                }
                // Nothing shaped: cache the negative so empty clips
                // don't re-shape every frame.
                None => TextEntry {
                    bytes: 0,
                    texture: None,
                    last_used: cache.tick,
                },
            };
            cache.bytes += entry.bytes;
            cache.entries.insert(key, entry);
            cache.enforce_budget();
        }
        let entry = cache.entries.get_mut(&key)?;
        entry.last_used = cache.tick;
        let (texture, block_center) = entry.texture.as_ref()?;

        // The raster is 1:1 with output pixels (scale is baked into the
        // rasterization), so the quad is exactly texture-sized; only
        // rotation remains for the GPU. The texture rect is ink-sized
        // and not centered on the text block, so shift the quad center
        // by the (rotated) block→texture center offset to anchor the
        // *block* center at the clip transform.
        let (tw, th) = (texture.width() as f32, texture.height() as f32);
        let anchor_x = ((pw / 2.0 + clip.transform.x) * s + off_x) as f32;
        let anchor_y = ((ph / 2.0 + clip.transform.y) * s + off_y) as f32;
        let rot = clip.transform.rotation.to_radians() as f32;
        let (dx, dy) = (tw / 2.0 - block_center.0, th / 2.0 - block_center.1);
        let (sin, cos) = rot.sin_cos();
        Some(PlannedLayer {
            source: PlannedSource::Text(key),
            center: (
                anchor_x + dx * cos - dy * sin,
                anchor_y + dx * sin + dy * cos,
            ),
            size: (tw, th),
            rotation_rad: rot,
            opacity: clip.opacity as f32,
            // Text rasters are premultiplied; blend modes don't apply.
            blend: GpuBlend::PremultipliedOver,
        })
    }

    /// Dispatch index for a transition kind, falling back to fade (and
    /// reporting once) for ids this build doesn't know.
    fn transition_kind_index(&mut self, kind: &str) -> u32 {
        match cutty_gpu::transition_kind(kind) {
            Some(index) => index,
            None => {
                if self.unknown_kinds.insert(kind.to_string()) {
                    self.issues.push(format!(
                        "unknown transition \"{kind}\" — rendering a crossfade \
                         (project saved by a newer Cutty?)"
                    ));
                }
                0 // fade
            }
        }
    }

    /// Decode, upload, and composite the output frame at time `t` into
    /// readback slot `slot`, without blocking on the readback. Source
    /// files are resolved through `path_of` (proxy for preview, original
    /// for export). Pair with [`TimelineRenderer::read_frame`]; slots
    /// allow one frame's readback to overlap the next frame's work.
    pub fn begin_frame(
        &mut self,
        project: &Project,
        t: f64,
        path_of: &dyn Fn(u64) -> Option<PathBuf>,
        slot: usize,
    ) -> Result<(), MediaError> {
        if let Some(cache) = &mut self.text {
            cache.tick += 1; // LRU clock; entries touched this frame are never evicted
        }
        let track_visuals = resolve_track_visuals(project, t);
        let needed: HashSet<u64> = track_visuals
            .iter()
            .flat_map(|v| match v {
                TrackVisual::Single(c) => vec![c.clip_id.0],
                TrackVisual::Transition { from, to, .. } => vec![from.clip_id.0, to.clip_id.0],
            })
            .collect();

        // Phase 1 (mutable): ensure decoders and sample every side.
        let mut plan: Vec<PlannedVisual> = Vec::new();
        for visual in &track_visuals {
            match visual {
                TrackVisual::Single(active) => {
                    if let Some(layer) =
                        self.sample_side(project, active, &needed, path_of, false)?
                    {
                        plan.push(PlannedVisual::Layer(layer));
                    }
                }
                TrackVisual::Transition {
                    from,
                    to,
                    kind,
                    progress,
                } => {
                    let from_layer = self.sample_side(project, from, &needed, path_of, true)?;
                    let to_layer = self.sample_side(project, to, &needed, path_of, true)?;
                    let kind = self.transition_kind_index(kind);
                    match (from_layer, to_layer) {
                        (Some(from), Some(to)) => plan.push(PlannedVisual::Transition {
                            from,
                            to,
                            kind,
                            progress: *progress as f32,
                        }),
                        // One side missing (lenient mode): degrade to the
                        // side that exists rather than dropping the track.
                        (Some(single), None) | (None, Some(single)) => {
                            plan.push(PlannedVisual::Layer(single));
                        }
                        (None, None) => {}
                    }
                }
            }
        }

        // Text layers stack above every video track, in resolver order.
        for active in resolve_text_layers(project, t) {
            if let Some(layer) = self.plan_text_layer(project, &active) {
                plan.push(PlannedVisual::Layer(layer));
            }
        }

        // Phase 2 (immutable): build texture refs and composite.
        let as_layer = |p: &PlannedLayer| Layer {
            source: match &p.source {
                PlannedSource::Media(key) => &self.sources[key].texture,
                PlannedSource::Text(key) => {
                    let cache = self.text.as_ref().expect("cache exists: text was planned");
                    let (texture, _) = cache.entries[key]
                        .texture
                        .as_ref()
                        .expect("planned text has a texture");
                    texture
                }
            },
            center: p.center,
            size: p.size,
            rotation_rad: p.rotation_rad,
            opacity: p.opacity,
            blend: p.blend,
        };
        let visuals: Vec<Visual> = plan
            .iter()
            .map(|v| match v {
                PlannedVisual::Layer(p) => Visual::Layer(as_layer(p)),
                PlannedVisual::Transition {
                    from,
                    to,
                    kind,
                    progress,
                } => Visual::Transition {
                    from: as_layer(from),
                    to: as_layer(to),
                    kind: *kind,
                    progress: *progress,
                },
            })
            .collect();
        self.compositor
            .composite_visuals(&mut self.target, &visuals, slot);
        self.stats.frames += 1;
        Ok(())
    }

    /// Block until slot `slot`'s readback lands and hand the frame to `f`.
    pub fn read_frame<R>(
        &mut self,
        slot: usize,
        f: impl FnOnce(FrameSlice<'_>) -> R,
    ) -> Result<R, MediaError> {
        let (w, h) = (self.out_w, self.out_h);
        self.compositor
            .read_slot(&mut self.target, slot, |data, stride| {
                f(FrameSlice {
                    width: w,
                    height: h,
                    stride,
                    data,
                })
            })
            .map_err(MediaError::Gpu)
    }

    /// Render the frame at `t` and read it back synchronously (the
    /// preview path; export pipelines `begin_frame`/`read_frame` across
    /// two slots instead).
    pub fn render_with<R>(
        &mut self,
        project: &Project,
        t: f64,
        path_of: &dyn Fn(u64) -> Option<PathBuf>,
        f: impl FnOnce(FrameSlice<'_>) -> R,
    ) -> Result<R, MediaError> {
        self.begin_frame(project, t, path_of, 0)?;
        self.read_frame(0, f)
    }

    /// Record a layer problem: fail the frame in strict mode, log and
    /// drop the layer otherwise.
    fn layer_problem(&mut self, message: String) -> Result<(), MediaError> {
        if self.strict {
            return Err(MediaError::ExportNotReady { message });
        }
        self.issues.push(message);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cutty_engine::{ClipId, MediaId, Transform};

    fn clip(transform: Transform) -> Clip {
        Clip {
            id: ClipId(1),
            media_id: Some(MediaId(1)),
            timeline_in: 0.0,
            timeline_out: 1.0,
            source_in: 0.0,
            source_out: 1.0,
            transform,
            opacity: 1.0,
            blend_mode: cutty_engine::BlendMode::Normal,
            speed: 1.0,
            volume: 1.0,
            transition_out: None,
            text: None,
            keyframes: Default::default(),
        }
    }

    fn settings(w: u32, h: u32) -> ProjectSettings {
        ProjectSettings {
            width: w,
            height: h,
            fps: 30.0,
        }
    }

    #[test]
    fn default_transform_fits_and_centers() {
        // 16:9 source in a 16:9 project rendered at 1280×720: exact cover.
        let (cx, cy, w, h, rot) = layer_placement(
            1920,
            1080,
            &clip(Transform::default()),
            &settings(1920, 1080),
            1280,
            720,
        );
        assert_eq!((cx, cy), (640.0, 360.0));
        assert_eq!((w, h), (1280.0, 720.0));
        assert_eq!(rot, 0.0);

        // Vertical source letterboxed into a 16:9 project: fit by height.
        let (cx, cy, w, h, _) = layer_placement(
            720,
            1280,
            &clip(Transform::default()),
            &settings(1920, 1080),
            1920,
            1080,
        );
        assert_eq!((cx, cy), (960.0, 540.0));
        assert_eq!(h, 1080.0);
        assert!((w - 607.5).abs() < 0.01, "w = {w}");
    }

    #[test]
    fn transform_offsets_scale_with_output_resolution() {
        // +100 px x-offset and 0.5 scale in a 1080p project…
        let t = Transform {
            x: 100.0,
            y: -50.0,
            scale: 0.5,
            rotation: 90.0,
        };
        // …rendered at project size: offset applies 1:1.
        let (cx, cy, w, h, rot) = layer_placement(
            1920,
            1080,
            &clip(t.clone()),
            &settings(1920, 1080),
            1920,
            1080,
        );
        assert_eq!((cx, cy), (1060.0, 490.0));
        assert_eq!((w, h), (960.0, 540.0));
        assert!((rot - std::f32::consts::FRAC_PI_2).abs() < 1e-6);

        // …rendered at half size (preview): everything halves.
        let (cx, cy, w, h, _) =
            layer_placement(1920, 1080, &clip(t), &settings(1920, 1080), 960, 540);
        assert_eq!((cx, cy), (530.0, 245.0));
        assert_eq!((w, h), (480.0, 270.0));
    }

    #[test]
    fn output_aspect_mismatch_letterboxes_the_project_canvas() {
        // 16:9 project exported to a square output: the canvas maps to
        // the middle band, so the fitted clip does too.
        let (cx, cy, w, h, _) = layer_placement(
            1920,
            1080,
            &clip(Transform::default()),
            &settings(1920, 1080),
            1000,
            1000,
        );
        assert_eq!((cx, cy), (500.0, 500.0));
        assert!((w - 1000.0).abs() < 0.01);
        assert!((h - 562.5).abs() < 0.01);
    }
}
