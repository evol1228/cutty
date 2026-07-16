//! # cutty-text
//!
//! CPU text rasterization for the compositor: shape and lay out styled
//! text with cosmic-text (system fonts via fontdb/fontconfig), then
//! rasterize fill, stroke, and shadow into one **premultiplied RGBA**
//! bitmap. The GPU composites that bitmap as an ordinary layer through
//! the premultiplied-over path, which is what makes text follow the
//! exact preview == export contract of every other layer.
//!
//! Like `cutty-gpu`, this crate is free of editor model types: callers
//! pass plain pixel quantities. **Everything here is in device pixels**
//! — the caller scales font size, stroke width, and shadow offsets by
//! its output scale (and the clip's transform scale) *before* calling,
//! so a 4K export rasterizes 4K-sharp glyphs instead of upscaling a
//! preview-sized bitmap.
//!
//! Stroke v1 is 8-direction offset drawing of the glyph coverage mask —
//! visually solid at CapCut-typical widths (a few px at 1080p). The
//! known upgrade for very wide strokes or sharp corners is real outline
//! stroking (offset the glyph outline and fill it, e.g. with `zeno`,
//! which swash already depends on). Shadow v1 is a single offset draw
//! with alpha — no blur yet.

use cosmic_text::{
    Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache, SwashContent, Weight, Wrap,
};

/// Line height as a multiple of font size (cosmic-text lays lines out on
/// this grid; CapCut-ish leading).
const LINE_HEIGHT: f32 = 1.2;

/// Padding around the computed ink bounds, px — guards the bilinear
/// sampling reads at raster edges.
const PAD: i32 = 2;

/// Hard cap on raster dimensions (a 4K frame is 4096 wide; text larger
/// than 2× that is clamped rather than allocating unbounded bitmaps).
const MAX_RASTER_DIM: u32 = 8192;

/// Horizontal alignment of lines within the text block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextAlign {
    Left,
    Center,
    Right,
}

impl TextAlign {
    fn factor(self) -> f32 {
        match self {
            TextAlign::Left => 0.0,
            TextAlign::Center => 0.5,
            TextAlign::Right => 1.0,
        }
    }
}

/// A fully-resolved raster request. All pixel quantities are **device
/// pixels** (pre-scaled by the caller); all colors are straight
/// (non-premultiplied) RGBA.
#[derive(Debug, Clone, PartialEq)]
pub struct RasterSpec {
    /// Font family as fontconfig knows it. Empty → default sans; the
    /// generic names `serif` / `sans-serif` / `monospace` map to the
    /// matching fallback class.
    pub font_family: String,
    /// Weight 100–900.
    pub weight: u16,
    /// Font size, px.
    pub font_size: f32,
    pub fill: [u8; 4],
    pub stroke_color: [u8; 4],
    /// Stroke width, px; 0 disables.
    pub stroke_width: f32,
    pub shadow_color: [u8; 4],
    /// Shadow offset, px, +x right / +y down.
    pub shadow_offset: (f32, f32),
    /// Shadow opacity 0..=1 (multiplies the shadow color's alpha); 0
    /// disables.
    pub shadow_alpha: f32,
    pub align: TextAlign,
}

/// A rasterized text block.
pub struct TextRaster {
    pub width: u32,
    pub height: u32,
    /// Premultiplied RGBA8 rows, `width * 4` bytes apart.
    pub data: Vec<u8>,
    /// Center of the **layout block** (the box alignment works in;
    /// stroke/shadow overhang excluded) within the raster, px from the
    /// top-left. This is the point the compositor anchors to the clip
    /// transform, so toggling a shadow never shifts the text.
    pub block_center: (f32, f32),
}

/// One glyph ready to blit: cache key + absolute position.
struct PlacedGlyph {
    cache_key: cosmic_text::CacheKey,
    x: i32,
    y: i32,
}

/// Shaped-and-aligned layout: glyph placements in **layout space**
/// (origin at the block's top-left) plus the block extents.
struct BlockLayout {
    glyphs: Vec<PlacedGlyph>,
    block_w: f32,
    block_h: f32,
}

/// Shaping, layout, and rasterization context. Creation loads the system
/// font database (fontconfig) — expensive, so hosts keep one per thread
/// and reuse it; the glyph cache inside amortizes repeated content.
pub struct TextRasterizer {
    font_system: FontSystem,
    swash: SwashCache,
}

impl Default for TextRasterizer {
    fn default() -> Self {
        Self::new()
    }
}

impl TextRasterizer {
    /// Load the system font database and set up the glyph cache.
    pub fn new() -> Self {
        Self {
            font_system: FontSystem::new(),
            swash: SwashCache::new(),
        }
    }

    /// Distinct font family names known to the system, sorted — the
    /// Inspector's font dropdown.
    pub fn font_families(&self) -> Vec<String> {
        let mut families: Vec<String> = self
            .font_system
            .db()
            .faces()
            .flat_map(|face| face.families.iter().map(|(name, _)| name.clone()))
            .collect();
        families.sort();
        families.dedup();
        families
    }

    fn attrs<'a>(spec: &'a RasterSpec) -> Attrs<'a> {
        let family = match spec.font_family.as_str() {
            "" | "sans-serif" => Family::SansSerif,
            "serif" => Family::Serif,
            "monospace" => Family::Monospace,
            name => Family::Name(name),
        };
        Attrs::new().family(family).weight(Weight(spec.weight))
    }

    /// Shape + lay out `content`, apply per-line alignment, and return
    /// absolute glyph placements with the block extents. Shared by
    /// [`Self::measure`] and [`Self::rasterize`] so the gizmo box and the
    /// rendered pixels can never disagree.
    fn layout(&mut self, content: &str, spec: &RasterSpec) -> BlockLayout {
        let font_size = spec.font_size.max(1.0);
        let metrics = Metrics::new(font_size, (font_size * LINE_HEIGHT).ceil());
        let mut buffer = Buffer::new(&mut self.font_system, metrics);
        let mut buffer = buffer.borrow_with(&mut self.font_system);
        // Explicit newlines only — text blocks have no wrap box.
        buffer.set_wrap(Wrap::None);
        buffer.set_size(None, None);
        // Alignment is applied manually below (cosmic's own `Align`
        // needs a fixed buffer width; text blocks are unbounded).
        buffer.set_text(content, &Self::attrs(spec), Shaping::Advanced, None);
        buffer.shape_until_scroll(false);

        let mut block_w = 0f32;
        let mut block_h = 0f32;
        for run in buffer.layout_runs() {
            block_w = block_w.max(run.line_w);
            block_h = block_h.max(run.line_top + run.line_height);
        }

        let mut glyphs = Vec::new();
        for run in buffer.layout_runs() {
            let align_x = (block_w - run.line_w) * spec.align.factor();
            for glyph in run.glyphs.iter() {
                let physical = glyph.physical((align_x, run.line_y), 1.0);
                glyphs.push(PlacedGlyph {
                    cache_key: physical.cache_key,
                    x: physical.x,
                    y: physical.y,
                });
            }
        }
        BlockLayout {
            glyphs,
            block_w,
            block_h,
        }
    }

    /// Size of the laid-out text block (the box the gizmo shows), px.
    /// `(0, 0)` when nothing shapes (empty content).
    pub fn measure(&mut self, content: &str, spec: &RasterSpec) -> (f32, f32) {
        let layout = self.layout(content, spec);
        if layout.glyphs.is_empty() {
            return (0.0, 0.0);
        }
        (layout.block_w, layout.block_h)
    }

    /// Rasterize the styled block into premultiplied RGBA. `None` when
    /// nothing would be visible (empty/whitespace content, or no font
    /// could shape it).
    pub fn rasterize(&mut self, content: &str, spec: &RasterSpec) -> Option<TextRaster> {
        let layout = self.layout(content, spec);
        if layout.glyphs.is_empty() {
            return None;
        }

        // Pass 1 — ink bounds across every glyph image (mask and color).
        let mut ink: Option<(i32, i32, i32, i32)> = None; // x0, y0, x1, y1 (exclusive)
        for glyph in &layout.glyphs {
            let Some(image) = self.swash.get_image(&mut self.font_system, glyph.cache_key) else {
                continue;
            };
            if image.placement.width == 0 || image.placement.height == 0 {
                continue;
            }
            let x0 = glyph.x + image.placement.left;
            let y0 = glyph.y - image.placement.top;
            let x1 = x0 + image.placement.width as i32;
            let y1 = y0 + image.placement.height as i32;
            ink = Some(match ink {
                None => (x0, y0, x1, y1),
                Some((a, b, c, d)) => (a.min(x0), b.min(y0), c.max(x1), d.max(y1)),
            });
        }
        let (ink_x0, ink_y0, ink_x1, ink_y1) = ink?;

        // Canvas: ink bounds grown by the stroke ring and the shadow
        // offset, then padded. The layout block is *not* forced into the
        // canvas — only visible pixels cost memory — but the block
        // center is still reported relative to it.
        let stroke = if spec.stroke_color[3] > 0 {
            spec.stroke_width.max(0.0).ceil() as i32
        } else {
            0
        };
        let (shadow_dx, shadow_dy) = spec.shadow_offset;
        let shadow_on = spec.shadow_alpha > 0.0 && spec.shadow_color[3] > 0;
        let grow_x0 = stroke
            + if shadow_on {
                (-shadow_dx).max(0.0).ceil() as i32
            } else {
                0
            };
        let grow_y0 = stroke
            + if shadow_on {
                (-shadow_dy).max(0.0).ceil() as i32
            } else {
                0
            };
        let grow_x1 = stroke
            + if shadow_on {
                shadow_dx.max(0.0).ceil() as i32
            } else {
                0
            };
        let grow_y1 = stroke
            + if shadow_on {
                shadow_dy.max(0.0).ceil() as i32
            } else {
                0
            };
        let canvas_x0 = ink_x0 - grow_x0 - PAD;
        let canvas_y0 = ink_y0 - grow_y0 - PAD;
        let canvas_x1 = ink_x1 + grow_x1 + PAD;
        let canvas_y1 = ink_y1 + grow_y1 + PAD;
        let width = ((canvas_x1 - canvas_x0) as u32).min(MAX_RASTER_DIM);
        let height = ((canvas_y1 - canvas_y0) as u32).min(MAX_RASTER_DIM);
        let (w, h) = (width as usize, height as usize);

        // Pass 2 — blit coverage (and color glyphs) in canvas space.
        let mut mask = vec![0u8; w * h]; // fill+stroke source (monochrome glyphs)
        let mut shadow_mask = vec![0u8; w * h]; // adds color-glyph alpha
        let mut color_layer: Option<Vec<u8>> = None; // premultiplied RGBA emoji layer
        for glyph in &layout.glyphs {
            let Some(image) = self.swash.get_image(&mut self.font_system, glyph.cache_key) else {
                continue;
            };
            if image.placement.width == 0 || image.placement.height == 0 {
                continue;
            }
            let gx = glyph.x + image.placement.left - canvas_x0;
            let gy = glyph.y - image.placement.top - canvas_y0;
            let gw = image.placement.width as usize;
            let gh = image.placement.height as usize;
            match image.content {
                SwashContent::Mask => {
                    blit_max(&mut mask, w, h, &image.data, gw, gh, (gx, gy));
                    blit_max(&mut shadow_mask, w, h, &image.data, gw, gh, (gx, gy));
                }
                SwashContent::Color => {
                    // Color glyphs (emoji) paint as-is above the text
                    // layers; their alpha still casts the shadow so
                    // mixed lines look consistent. No stroke for them.
                    let layer = color_layer.get_or_insert_with(|| vec![0u8; w * h * 4]);
                    blit_color_premul(layer, &mut shadow_mask, w, h, &image.data, gw, gh, (gx, gy));
                }
                SwashContent::SubpixelMask => {
                    // Not produced for our render mode; treat the red
                    // channel as coverage if it ever appears.
                    let coverage: Vec<u8> = image.data.chunks(4).map(|px| px[0]).collect();
                    blit_max(&mut mask, w, h, &coverage, gw, gh, (gx, gy));
                    blit_max(&mut shadow_mask, w, h, &coverage, gw, gh, (gx, gy));
                }
            }
        }

        // Pass 3 — compose premultiplied layers bottom-up:
        // shadow, then stroke, then fill, then color glyphs.
        let mut out = vec![0u8; w * h * 4];
        if shadow_on {
            let a = spec.shadow_alpha.clamp(0.0, 1.0);
            let tint = [
                spec.shadow_color[0],
                spec.shadow_color[1],
                spec.shadow_color[2],
                (f32::from(spec.shadow_color[3]) * a).round() as u8,
            ];
            over_shifted_mask(&mut out, &shadow_mask, w, h, (shadow_dx, shadow_dy), tint);
        }
        if stroke > 0 {
            let stroke_mask = dilate_8dir(&mask, w, h, spec.stroke_width);
            over_shifted_mask(&mut out, &stroke_mask, w, h, (0.0, 0.0), spec.stroke_color);
        }
        over_shifted_mask(&mut out, &mask, w, h, (0.0, 0.0), spec.fill);
        if let Some(layer) = color_layer {
            for (dst, src) in out.chunks_exact_mut(4).zip(layer.chunks_exact(4)) {
                over_premul(dst, [src[0], src[1], src[2], src[3]]);
            }
        }

        Some(TextRaster {
            width,
            height,
            data: out,
            block_center: (
                layout.block_w / 2.0 - canvas_x0 as f32,
                layout.block_h / 2.0 - canvas_y0 as f32,
            ),
        })
    }
}

/// Blit a coverage tile into `dst` (single channel), keeping the max
/// where glyphs overlap.
fn blit_max(
    dst: &mut [u8],
    dst_w: usize,
    dst_h: usize,
    src: &[u8],
    src_w: usize,
    src_h: usize,
    (at_x, at_y): (i32, i32),
) {
    for sy in 0..src_h {
        let dy = at_y + sy as i32;
        if dy < 0 || dy >= dst_h as i32 {
            continue;
        }
        for sx in 0..src_w {
            let dx = at_x + sx as i32;
            if dx < 0 || dx >= dst_w as i32 {
                continue;
            }
            let d = &mut dst[dy as usize * dst_w + dx as usize];
            *d = (*d).max(src[sy * src_w + sx]);
        }
    }
}

/// Blit a straight-alpha RGBA tile as premultiplied "over" into `layer`,
/// and its alpha into `shadow_mask`.
#[allow(clippy::too_many_arguments)]
fn blit_color_premul(
    layer: &mut [u8],
    shadow_mask: &mut [u8],
    dst_w: usize,
    dst_h: usize,
    src: &[u8],
    src_w: usize,
    src_h: usize,
    (at_x, at_y): (i32, i32),
) {
    for sy in 0..src_h {
        let dy = at_y + sy as i32;
        if dy < 0 || dy >= dst_h as i32 {
            continue;
        }
        for sx in 0..src_w {
            let dx = at_x + sx as i32;
            if dx < 0 || dx >= dst_w as i32 {
                continue;
            }
            let s = &src[(sy * src_w + sx) * 4..(sy * src_w + sx) * 4 + 4];
            if s[3] == 0 {
                continue;
            }
            let a = f32::from(s[3]) / 255.0;
            let premul = [
                (f32::from(s[0]) * a).round() as u8,
                (f32::from(s[1]) * a).round() as u8,
                (f32::from(s[2]) * a).round() as u8,
                s[3],
            ];
            let di = (dy as usize * dst_w + dx as usize) * 4;
            over_premul(&mut layer[di..di + 4], premul);
            let m = &mut shadow_mask[dy as usize * dst_w + dx as usize];
            *m = (*m).max(s[3]);
        }
    }
}

/// Premultiplied source-over: `dst = src + dst * (1 - src.a)`.
fn over_premul(dst: &mut [u8], src: [u8; 4]) {
    let inv = 255 - u16::from(src[3]);
    for c in 0..4 {
        let d = u16::from(dst[c]);
        dst[c] = (u16::from(src[c]) + (d * inv + 127) / 255).min(255) as u8;
    }
}

/// Bilinear sample of a single-channel mask at a fractional position;
/// 0 outside.
fn sample_mask(mask: &[u8], w: usize, h: usize, x: f32, y: f32) -> f32 {
    let x0 = x.floor();
    let y0 = y.floor();
    let fx = x - x0;
    let fy = y - y0;
    let at = |ix: i32, iy: i32| -> f32 {
        if ix < 0 || iy < 0 || ix >= w as i32 || iy >= h as i32 {
            0.0
        } else {
            f32::from(mask[iy as usize * w + ix as usize])
        }
    };
    let (x0, y0) = (x0 as i32, y0 as i32);
    let top = at(x0, y0) * (1.0 - fx) + at(x0 + 1, y0) * fx;
    let bottom = at(x0, y0 + 1) * (1.0 - fx) + at(x0 + 1, y0 + 1) * fx;
    top * (1.0 - fy) + bottom * fy
}

/// Stroke v1: dilate the coverage mask by drawing it offset in 8
/// directions at `radius` (axis-aligned and diagonal), keeping the max.
/// Reads well at CapCut-typical widths; the upgrade path is true outline
/// stroking (see the module docs).
fn dilate_8dir(mask: &[u8], w: usize, h: usize, radius: f32) -> Vec<u8> {
    const DIAG: f32 = std::f32::consts::FRAC_1_SQRT_2;
    let dirs = [
        (1.0, 0.0),
        (-1.0, 0.0),
        (0.0, 1.0),
        (0.0, -1.0),
        (DIAG, DIAG),
        (DIAG, -DIAG),
        (-DIAG, DIAG),
        (-DIAG, -DIAG),
    ];
    let mut out = mask.to_vec();
    for y in 0..h {
        for x in 0..w {
            let mut best = f32::from(out[y * w + x]);
            for (dx, dy) in dirs {
                if best >= 255.0 {
                    break;
                }
                let v = sample_mask(mask, w, h, x as f32 - dx * radius, y as f32 - dy * radius);
                best = best.max(v);
            }
            out[y * w + x] = best.round().min(255.0) as u8;
        }
    }
    out
}

/// Composite `mask` (optionally shifted by a fractional offset) tinted
/// with straight-alpha `color` over premultiplied `out`.
fn over_shifted_mask(
    out: &mut [u8],
    mask: &[u8],
    w: usize,
    h: usize,
    shift: (f32, f32),
    color: [u8; 4],
) {
    if color[3] == 0 {
        return;
    }
    let shifted = shift != (0.0, 0.0);
    for y in 0..h {
        for x in 0..w {
            let coverage = if shifted {
                sample_mask(mask, w, h, x as f32 - shift.0, y as f32 - shift.1) / 255.0
            } else {
                f32::from(mask[y * w + x]) / 255.0
            };
            if coverage <= 0.0 {
                continue;
            }
            let a = coverage * f32::from(color[3]) / 255.0;
            let src = [
                (f32::from(color[0]) * a).round() as u8,
                (f32::from(color[1]) * a).round() as u8,
                (f32::from(color[2]) * a).round() as u8,
                (a * 255.0).round() as u8,
            ];
            let i = (y * w + x) * 4;
            over_premul(&mut out[i..i + 4], src);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> RasterSpec {
        RasterSpec {
            font_family: String::new(),
            weight: 700,
            font_size: 48.0,
            fill: [255, 255, 255, 255],
            stroke_color: [0, 0, 0, 255],
            stroke_width: 0.0,
            shadow_color: [0, 0, 0, 255],
            shadow_offset: (0.0, 0.0),
            shadow_alpha: 0.0,
            align: TextAlign::Center,
        }
    }

    /// System fonts are an environment dependency; skip (visibly) on
    /// fontless machines so the suite stays green in minimal containers.
    fn rasterizer() -> Option<TextRasterizer> {
        let r = TextRasterizer::new();
        if r.font_families().is_empty() {
            eprintln!("cutty-text tests: skipping, no system fonts");
            return None;
        }
        Some(r)
    }

    fn coverage(raster: &TextRaster) -> usize {
        raster.data.chunks_exact(4).filter(|px| px[3] > 0).count()
    }

    #[test]
    fn rasterizes_visible_premultiplied_pixels() {
        let Some(mut r) = rasterizer() else { return };
        let raster = r.rasterize("Cutty", &spec()).expect("visible raster");
        assert!(raster.width > 0 && raster.height > 0);
        assert!(coverage(&raster) > 100, "glyphs cover real area");
        // Premultiplied invariant: no channel exceeds alpha.
        for px in raster.data.chunks_exact(4) {
            for c in 0..3 {
                assert!(px[c] <= px[3], "premultiplied channel {c} > alpha: {px:?}");
            }
        }
        // The block center sits inside the raster.
        assert!(raster.block_center.0 > 0.0 && raster.block_center.0 < raster.width as f32);
        assert!(raster.block_center.1 > 0.0 && raster.block_center.1 < raster.height as f32);
    }

    #[test]
    fn empty_and_whitespace_content_rasterize_to_none() {
        let Some(mut r) = rasterizer() else { return };
        assert!(r.rasterize("", &spec()).is_none());
        assert!(r.rasterize("   ", &spec()).is_none());
        assert_eq!(r.measure("", &spec()), (0.0, 0.0));
    }

    #[test]
    fn stroke_and_shadow_grow_the_ink() {
        let Some(mut r) = rasterizer() else { return };
        let plain = r.rasterize("O", &spec()).expect("plain");

        let mut stroked_spec = spec();
        stroked_spec.stroke_width = 6.0;
        let stroked = r.rasterize("O", &stroked_spec).expect("stroked");
        assert!(
            coverage(&stroked) > coverage(&plain),
            "stroke adds covered area: {} vs {}",
            coverage(&stroked),
            coverage(&plain)
        );
        assert!(stroked.width >= plain.width + 8, "canvas grew for stroke");

        let mut shadow_spec = spec();
        shadow_spec.shadow_alpha = 0.8;
        shadow_spec.shadow_offset = (8.0, 8.0);
        let shadowed = r.rasterize("O", &shadow_spec).expect("shadowed");
        assert!(coverage(&shadowed) > coverage(&plain));
        // Stroke pixels are the stroke color where the fill doesn't
        // cover: sample the outermost covered pixel row.
        let first_covered = stroked
            .data
            .chunks_exact(4)
            .find(|px| px[3] > 8)
            .expect("covered pixel");
        assert!(
            first_covered[0] < 64,
            "outer ring is dark (stroke), got {first_covered:?}"
        );
    }

    #[test]
    fn shadow_only_pixels_carry_the_shadow_tint() {
        let Some(mut r) = rasterizer() else { return };
        let mut s = spec();
        s.shadow_alpha = 1.0;
        s.shadow_offset = (12.0, 12.0);
        s.shadow_color = [255, 0, 0, 255];
        let raster = r.rasterize("I", &s).expect("raster");
        // Bottom-right of the ink is shadow-only: red, half-ish alpha.
        let mut found_pure_shadow = false;
        for px in raster.data.chunks_exact(4) {
            if px[3] > 128 && px[0] > 100 && px[1] == 0 && px[2] == 0 {
                found_pure_shadow = true;
                break;
            }
        }
        assert!(found_pure_shadow, "expected red shadow-only pixels");
    }

    #[test]
    fn alignment_shifts_short_lines() {
        let Some(mut r) = rasterizer() else { return };
        let content = "wide wide wide\ni";
        let mut left = spec();
        left.align = TextAlign::Left;
        let mut right = spec();
        right.align = TextAlign::Right;
        let l = r.rasterize(content, &left).expect("left");
        let rr = r.rasterize(content, &right).expect("right");
        // Same block, different pixel distribution.
        assert_ne!(l.data, rr.data, "alignment must move the short line");
        let (lw, lh) = r.measure(content, &left);
        let (rw, rh) = r.measure(content, &right);
        assert!((lw - rw).abs() < 0.5 && (lh - rh).abs() < 0.5);
    }

    #[test]
    fn measure_matches_raster_block_and_scales() {
        let Some(mut r) = rasterizer() else { return };
        let s = spec();
        let (w1, h1) = r.measure("Scale", &s);
        assert!(w1 > 0.0 && h1 > 0.0);
        let mut doubled = spec();
        doubled.font_size = s.font_size * 2.0;
        let (w2, h2) = r.measure("Scale", &doubled);
        assert!(
            (w2 / w1 - 2.0).abs() < 0.15 && (h2 / h1 - 2.0).abs() < 0.15,
            "double size ≈ double block: {w1}x{h1} → {w2}x{h2}"
        );

        // The raster is ink-sized (line-height leading excluded), so it
        // may be *smaller* than the block box — but the reported block
        // center must stay in the raster's neighborhood so anchoring is
        // meaningful.
        let raster = r.rasterize("Scale", &s).expect("raster");
        assert!(raster.width as f32 >= w1 * 0.8, "ink ≈ block width");
        assert!(raster.block_center.0 > 0.0 && raster.block_center.0 < raster.width as f32);
    }

    #[test]
    fn multiline_stacks_lines() {
        let Some(mut r) = rasterizer() else { return };
        let (_, h1) = r.measure("one", &spec());
        let (_, h3) = r.measure("one\ntwo\nthree", &spec());
        assert!(
            (h3 / h1 - 3.0).abs() < 0.2,
            "three lines ≈ 3× one line: {h1} vs {h3}"
        );
    }
}
