//! Audio peak (waveform) data: min/max window pairs, generated in the
//! background at import and cached beside proxies/thumbnails.
//!
//! One window is 1/[`PEAKS_PER_SEC`] of a second; each stores the min and
//! max sample across *all channels* in that window, quantized to i8 —
//! 100 pairs/s ≈ 200 bytes per media second, small enough to ship to the
//! timeline in one IPC response. Decode runs through
//! [`crate::audio_source::open_audio_source`], so every codec the mixer
//! plays (ac3/dts included) gets a waveform.
//!
//! ## File format (`$XDG_CACHE_HOME/cutty/peaks/<hash>.pks`)
//!
//! ```text
//! [0..4)   magic  b"CPKS"
//! [4..8)   u32 LE version (1)
//! [8..12)  u32 LE windows per second
//! [12..16) u32 LE window count N
//! [16..)   N × (i8 min, i8 max)
//! ```

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::audio_source::open_audio_source;
use crate::cache::{cache_dir, cache_entry_for};
use crate::error::MediaError;

/// Waveform resolution, windows per second. 100 gives one window per
/// 10 ms — at the timeline's maximum zoom (800 px/s) that is one pair
/// per 8 px, comfortably beyond what a clip lane can show.
pub const PEAKS_PER_SEC: u32 = 100;

const MAGIC: &[u8; 4] = b"CPKS";
const VERSION: u32 = 1;

/// Where the peaks file for `src` lives (or will live) in the cache.
///
/// Returns `(final_path, exists)`.
pub fn peaks_path_for(src: &Path) -> Result<(PathBuf, bool), MediaError> {
    cache_entry_for(src, "peaks", "pks")
}

/// Generate (or fetch from cache) the peak file for `src` and return its
/// bytes. Blocking — run on a background thread. Fails for sources
/// without a decodable audio stream.
pub fn generate_peaks(src: &Path) -> Result<Vec<u8>, MediaError> {
    let (final_path, exists) = peaks_path_for(src)?;
    if exists {
        return Ok(std::fs::read(&final_path)?);
    }

    let mut source = open_audio_source(src).map_err(MediaError::Audio)?;
    let rate = u64::from(source.sample_rate().max(1));
    let channels = source.channels().max(1);
    let per_sec = u64::from(PEAKS_PER_SEC);

    let mut pairs: Vec<i8> = Vec::new();
    let mut window_index: u64 = 0;
    // Exact rational window boundaries: window i ends at frame
    // (i+1)*rate/per_sec — no drift over long files.
    let mut window_end = rate.div_ceil(per_sec).max(1);
    let mut frames_done: u64 = 0;
    let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
    let mut in_window = false;

    let mut buf = vec![0f32; 8192 - (8192 % channels)];
    loop {
        let n = source.read(&mut buf).map_err(MediaError::Audio)?;
        if n == 0 {
            break;
        }
        for frame in buf[..n].chunks(channels) {
            for &s in frame {
                lo = lo.min(s);
                hi = hi.max(s);
            }
            in_window = true;
            frames_done += 1;
            if frames_done >= window_end {
                push_pair(&mut pairs, lo, hi);
                (lo, hi) = (f32::INFINITY, f32::NEG_INFINITY);
                in_window = false;
                window_index += 1;
                window_end = ((window_index + 1) * rate).div_ceil(per_sec);
            }
        }
    }
    if in_window {
        push_pair(&mut pairs, lo, hi);
    }

    let mut bytes = Vec::with_capacity(16 + pairs.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(&PEAKS_PER_SEC.to_le_bytes());
    bytes.extend_from_slice(&((pairs.len() / 2) as u32).to_le_bytes());
    bytes.extend_from_slice(bytemuck_cast_i8(&pairs));

    // Atomic publish (same idiom as thumbnails): unique part file, then
    // rename — concurrent generators never see each other's partials.
    std::fs::create_dir_all(cache_dir("peaks")?)?;
    static PART_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let part_path = final_path.with_extension(format!(
        "part-{}-{}.pks",
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

fn push_pair(pairs: &mut Vec<i8>, lo: f32, hi: f32) {
    let q = |v: f32| (v.clamp(-1.0, 1.0) * 127.0).round() as i8;
    // An empty window (shouldn't happen, but keep the format total).
    if lo.is_finite() && hi.is_finite() {
        pairs.push(q(lo));
        pairs.push(q(hi));
    } else {
        pairs.push(0);
        pairs.push(0);
    }
}

fn bytemuck_cast_i8(v: &[i8]) -> &[u8] {
    // i8 → u8 view; same size/alignment, no unsafe layout assumptions
    // beyond the primitive cast.
    unsafe { std::slice::from_raw_parts(v.as_ptr().cast::<u8>(), v.len()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn test_dir() -> PathBuf {
        let dir = std::env::temp_dir().join("cutty-media-tests");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 1s at 48k: silence for 0.5s, then a full-scale square wave — the
    /// peak pairs must show ~0 then ~±127.
    fn half_silent_wav() -> PathBuf {
        let file = test_dir().join("peaks-halfsilent.wav");
        if file.is_file() {
            return file;
        }
        let status = Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
            .arg("aevalsrc=if(gte(t\\,0.5)\\,if(lt(mod(t\\,0.01)\\,0.005)\\,0.99\\,-0.99)\\,0):s=48000:d=1")
            .args(["-c:a", "pcm_f32le"])
            .arg(&file)
            .status()
            .expect("system ffmpeg required");
        assert!(status.success());
        file
    }

    fn parse(bytes: &[u8]) -> (u32, u32, Vec<(i8, i8)>) {
        assert_eq!(&bytes[0..4], MAGIC);
        let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        let per_sec = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        let count = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let pairs = bytes[16..16 + count * 2]
            .chunks_exact(2)
            .map(|p| (p[0] as i8, p[1] as i8))
            .collect();
        (version, per_sec, pairs)
    }

    #[test]
    fn peaks_capture_silence_and_signal() {
        let bytes = generate_peaks(&half_silent_wav()).unwrap();
        let (version, per_sec, pairs) = parse(&bytes);
        assert_eq!(version, VERSION);
        assert_eq!(per_sec, PEAKS_PER_SEC);
        // 1s at 100/s → 100 windows (±1 for rounding).
        assert!(
            (99..=101).contains(&pairs.len()),
            "expected ≈100 windows, got {}",
            pairs.len()
        );
        // First 0.4s: silence.
        for (i, &(lo, hi)) in pairs[..40].iter().enumerate() {
            assert!(lo.abs() <= 1 && hi.abs() <= 1, "window {i}: ({lo},{hi})");
        }
        // After 0.6s: full-scale square.
        for (i, &(lo, hi)) in pairs[60..95].iter().enumerate() {
            assert!(
                lo <= -120 && hi >= 120,
                "window {}: ({lo},{hi}) must swing full-scale",
                60 + i
            );
        }

        // Second call hits the cache byte-identically.
        let again = generate_peaks(&half_silent_wav()).unwrap();
        assert_eq!(again, bytes);
    }

    #[test]
    fn peaks_work_for_ac3_through_the_fallback() {
        let file = test_dir().join("peaks-ac3.ac3");
        if !file.is_file() {
            let status = Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
                .arg("sine=frequency=440:sample_rate=48000:duration=1")
                .args(["-c:a", "ac3", "-b:a", "192k"])
                .arg(&file)
                .status()
                .unwrap();
            assert!(status.success());
        }
        let bytes = generate_peaks(&file).unwrap();
        let (_, _, pairs) = parse(&bytes);
        assert!(pairs.len() >= 95, "got {} windows", pairs.len());
        // The ffmpeg sine synth runs at ~1/8 scale → peaks ≈ ±16.
        let strong = pairs.iter().filter(|(lo, hi)| *hi >= 10 && *lo <= -10).count();
        assert!(
            strong > pairs.len() / 2,
            "sine energy must show in most windows ({strong}/{})",
            pairs.len()
        );
    }

    #[test]
    fn video_files_get_peaks_from_their_audio_stream() {
        let file = test_dir().join("peaks-video.mp4");
        if !file.is_file() {
            let status = Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
                .arg("testsrc2=size=320x180:rate=30:duration=1")
                .args(["-f", "lavfi", "-i"])
                .arg("sine=frequency=330:sample_rate=48000:duration=1")
                .args(["-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac", "-shortest"])
                .arg(&file)
                .status()
                .unwrap();
            assert!(status.success());
        }
        let bytes = generate_peaks(&file).unwrap();
        let (_, _, pairs) = parse(&bytes);
        assert!(pairs.len() >= 90, "got {} windows", pairs.len());
    }
}
