//! Filmstrip thumbnail strips for timeline clips: a fixed-interval row
//! of small frames per media file, generated in the background at import
//! and cached beside proxies. The timeline draws tiles straight from the
//! decoded sprite — never a decode on the render path.
//!
//! One sprite per media: `count` cells of [`TILE_W`]×[`TILE_H`] px laid
//! out horizontally, aspect-fill cropped (CapCut-style), JPEG-encoded.
//! The interval is fixed per media at generation time —
//! `duration / MAX_TILES` clamped to [[`MIN_INTERVAL_SEC`],
//! [`MAX_INTERVAL_SEC`]] — so zooming never regenerates anything; the
//! renderer repeats or skips tiles.
//!
//! ## File format (`$XDG_CACHE_HOME/cutty/strips/<hash>.strip`)
//!
//! ```text
//! [0..4)   magic  b"CFLM"
//! [4..8)   u32 LE version (1)
//! [8..12)  u32 LE tile width, px
//! [12..16) u32 LE tile height, px
//! [16..20) u32 LE tile count
//! [20..24) u32 LE tile interval, milliseconds
//! [24..)   JPEG sprite (count·tile_w × tile_h)
//! ```

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::cache::{cache_dir, cache_entry_for};
use crate::decode::SourceDecoder;
use crate::error::MediaError;
use crate::jpeg::JpegEncoder;

/// Tile cell size, px. 96×54 (16:9) reads clearly at the timeline's
/// video-lane height and keeps even a 120-tile sprite near ~200 KB.
pub const TILE_W: u32 = 96;
pub const TILE_H: u32 = 54;

/// Longest strip generated per media.
const MAX_TILES: u32 = 120;
const MIN_INTERVAL_SEC: f64 = 0.25;
const MAX_INTERVAL_SEC: f64 = 10.0;

const MAGIC: &[u8; 4] = b"CFLM";
const VERSION: u32 = 1;

/// Where the filmstrip for `src` lives (or will live) in the cache.
///
/// Returns `(final_path, exists)`.
pub fn filmstrip_path_for(src: &Path) -> Result<(PathBuf, bool), MediaError> {
    cache_entry_for(src, "strips", "strip")
}

/// The fixed tile interval for a media duration.
fn interval_for(duration: f64) -> f64 {
    (duration / f64::from(MAX_TILES)).clamp(MIN_INTERVAL_SEC, MAX_INTERVAL_SEC)
}

/// Generate (or fetch from cache) the filmstrip for `src` and return its
/// bytes. Blocking and decode-heavy (one seek per tile) — run on a
/// background thread. Fails for sources without a decodable picture.
pub fn generate_filmstrip(src: &Path, duration_hint: Option<f64>) -> Result<Vec<u8>, MediaError> {
    let (final_path, exists) = filmstrip_path_for(src)?;
    if exists {
        return Ok(std::fs::read(&final_path)?);
    }

    let mut decoder = SourceDecoder::open(src)?;
    let duration = duration_hint
        .filter(|d| d.is_finite() && *d > 0.0)
        .unwrap_or(0.0);
    let interval = interval_for(duration);
    let count = if duration > 0.0 {
        ((duration / interval).ceil() as u32).clamp(1, MAX_TILES)
    } else {
        1 // stills / unknown duration: a single representative tile
    };

    let sprite_w = TILE_W * count;
    let mut sprite = vec![0u8; (sprite_w * TILE_H * 4) as usize];
    let mut produced_any = false;
    for i in 0..count {
        // Sample mid-window so the tile represents its span (and tile 0
        // skips a black first frame when there is room).
        let t = (f64::from(i) + 0.5) * interval;
        let frame = match decoder.seek_to(t.min(duration.max(0.0)))? {
            Some(f) => f,
            None => break, // no frames at all
        };
        produced_any = true;
        blit_cover(
            frame.data,
            frame.stride,
            frame.width,
            frame.height,
            &mut sprite,
            sprite_w,
            i * TILE_W,
        );
    }
    if !produced_any {
        return Err(MediaError::NoStreams {
            path: src.display().to_string(),
        });
    }

    let jpeg = JpegEncoder::new()?.encode_rgba_strided(
        sprite_w,
        TILE_H,
        sprite_w as usize * 4,
        &sprite,
    )?;

    let mut bytes = Vec::with_capacity(24 + jpeg.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(&TILE_W.to_le_bytes());
    bytes.extend_from_slice(&TILE_H.to_le_bytes());
    bytes.extend_from_slice(&count.to_le_bytes());
    bytes.extend_from_slice(&((interval * 1000.0).round() as u32).to_le_bytes());
    bytes.extend_from_slice(&jpeg);

    std::fs::create_dir_all(cache_dir("strips")?)?;
    static PART_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let part_path = final_path.with_extension(format!(
        "part-{}-{}.strip",
        std::process::id(),
        PART_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let mut file = std::fs::File::create(&part_path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&part_path, &final_path)?;
    Ok(bytes)
}

/// Box-sample `src` (RGBA, `stride` bytes/row) into the sprite cell at
/// `cell_x`, aspect-fill (cover): the largest centered source rect with
/// the cell's aspect, averaged per destination pixel. Background
/// derivation work (import-time), not a render-path pixel loop.
fn blit_cover(
    src: &[u8],
    stride: usize,
    src_w: u32,
    src_h: u32,
    sprite: &mut [u8],
    sprite_w: u32,
    cell_x: u32,
) {
    let cell_aspect = f64::from(TILE_W) / f64::from(TILE_H);
    let src_aspect = f64::from(src_w) / f64::from(src_h);
    // Cover crop rect in source pixels.
    let (crop_w, crop_h) = if src_aspect > cell_aspect {
        (f64::from(src_h) * cell_aspect, f64::from(src_h))
    } else {
        (f64::from(src_w), f64::from(src_w) / cell_aspect)
    };
    let crop_x = (f64::from(src_w) - crop_w) / 2.0;
    let crop_y = (f64::from(src_h) - crop_h) / 2.0;

    for dy in 0..TILE_H {
        let sy0 = (crop_y + f64::from(dy) / f64::from(TILE_H) * crop_h) as usize;
        let sy1 = ((crop_y + f64::from(dy + 1) / f64::from(TILE_H) * crop_h) as usize)
            .clamp(sy0 + 1, src_h as usize);
        for dx in 0..TILE_W {
            let sx0 = (crop_x + f64::from(dx) / f64::from(TILE_W) * crop_w) as usize;
            let sx1 = ((crop_x + f64::from(dx + 1) / f64::from(TILE_W) * crop_w) as usize)
                .clamp(sx0 + 1, src_w as usize);
            let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
            let mut n = 0u32;
            for sy in sy0..sy1 {
                let row = sy * stride;
                for sx in sx0..sx1 {
                    let p = row + sx * 4;
                    r += u32::from(src[p]);
                    g += u32::from(src[p + 1]);
                    b += u32::from(src[p + 2]);
                    n += 1;
                }
            }
            let d = ((cell_x + dx) + dy * sprite_w) as usize * 4;
            sprite[d] = (r / n) as u8;
            sprite[d + 1] = (g / n) as u8;
            sprite[d + 2] = (b / n) as u8;
            sprite[d + 3] = 255;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::generate_test_clip;

    #[test]
    fn interval_scales_with_duration() {
        assert_eq!(interval_for(10.0), MIN_INTERVAL_SEC);
        assert_eq!(interval_for(60.0), 0.5);
        assert_eq!(interval_for(600.0), 5.0);
        assert_eq!(interval_for(7200.0), MAX_INTERVAL_SEC);
    }

    #[test]
    fn generates_a_strip_with_header_and_jpeg() {
        let clip = generate_test_clip("filmstrip-src", 640, 360, 30, 3);
        let bytes = generate_filmstrip(&clip, Some(3.0)).unwrap();

        assert_eq!(&bytes[0..4], MAGIC);
        let u32_at = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
        assert_eq!(u32_at(4), VERSION);
        assert_eq!(u32_at(8), TILE_W);
        assert_eq!(u32_at(12), TILE_H);
        let count = u32_at(16);
        let interval_ms = u32_at(20);
        assert_eq!(interval_ms, 250, "3s clip → min interval");
        assert_eq!(count, 12, "3s / 0.25s");
        // The payload is a JPEG.
        assert_eq!(&bytes[24..26], &[0xFF, 0xD8]);
        assert_eq!(&bytes[bytes.len() - 2..], &[0xFF, 0xD9]);

        // Cache hit returns identical bytes.
        let again = generate_filmstrip(&clip, Some(3.0)).unwrap();
        assert_eq!(again, bytes);
    }

    #[test]
    fn cover_blit_center_crops() {
        // A 200×100 source, left half red, right half blue → a 16:9 cell
        // covers the middle: left of cell red, right blue.
        let (w, h) = (200u32, 100u32);
        let mut src = vec![0u8; (w * h * 4) as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let p = (y * w as usize + x) * 4;
                src[p] = if x < 100 { 255 } else { 0 };
                src[p + 2] = if x >= 100 { 255 } else { 0 };
                src[p + 3] = 255;
            }
        }
        let mut sprite = vec![0u8; (TILE_W * TILE_H * 4) as usize];
        blit_cover(&src, w as usize * 4, w, h, &mut sprite, TILE_W, 0);
        let px = |x: u32, y: u32| {
            let p = ((y * TILE_W + x) * 4) as usize;
            (sprite[p], sprite[p + 2])
        };
        let (r, b) = px(4, TILE_H / 2);
        assert!(r > 200 && b < 50, "left of cell is red, got ({r},{b})");
        let (r, b) = px(TILE_W - 4, TILE_H / 2);
        assert!(b > 200 && r < 50, "right of cell is blue, got ({r},{b})");
    }
}
