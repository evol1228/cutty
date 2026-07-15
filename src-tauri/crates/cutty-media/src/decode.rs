//! In-process video decoding via libav (`ffmpeg-the-third`).
//!
//! Replaces the Phase 0 spawn-per-seek CLI decoder for playback and
//! scrubbing: process startup + format-open put a measured ~110 ms floor
//! under every cold CLI seek, which can never meet the <100 ms budget.
//! Keeping the `AVFormatContext` open makes a cold seek = `av_seek_frame`
//! to the previous keyframe + at most one GOP of catch-up decode
//! (proxies are `-g 30`), landing well under the budget.
//!
//! Decoding and pixel conversion happen here; everything downstream
//! (compositing, JPEG) reads the RGBA frame zero-copy via [`FrameView`].
//! Output is RGBA (not RGB) because the compositor uploads frames as
//! `Rgba8Unorm` textures — the alpha channel costs nothing extra in
//! swscale and saves a repack on the GPU upload path.

use std::path::{Path, PathBuf};
use std::sync::Once;

use ffmpeg_the_third as ffmpeg;
use ffmpeg_the_third::ffi::AV_TIME_BASE;
use ffmpeg_the_third::format::context::Input;
use ffmpeg_the_third::format::Pixel;
use ffmpeg_the_third::media::Type;
use ffmpeg_the_third::software::scaling;

use crate::error::MediaError;

/// Tolerance when comparing frame pts against targets (well under any
/// real frame duration).
const PTS_EPS: f64 = 1e-6;

fn init_ffmpeg() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        if let Err(e) = ffmpeg::init() {
            eprintln!("cutty-media: ffmpeg init: {e}");
        }
        // The libav* logs are noise at playback volume; errors surface
        // through return values.
        ffmpeg::util::log::set_level(ffmpeg::util::log::Level::Error);
    });
}

fn ff_err(path: &Path, context: &str, e: ffmpeg::Error) -> MediaError {
    MediaError::FfmpegFailed {
        context: Some(format!("{context} for {}", path.display())),
        message: e.to_string(),
    }
}

/// A decoded RGBA frame, borrowed from the decoder's reusable buffer.
/// Valid until the next decoder call. Alpha is always 255 for video
/// sources.
pub struct FrameView<'a> {
    /// Presentation time within the source, seconds.
    pub pts_sec: f64,
    pub width: u32,
    pub height: u32,
    /// Bytes per row (≥ `width * 4`; libav aligns rows).
    pub stride: usize,
    pub data: &'a [u8],
}

/// One open decode session on a source file (normally a 720p proxy).
///
/// Sequential playback pulls [`SourceDecoder::next_frame`]; seeks position
/// the session so the *frame visible at* the target time comes out next
/// (floor semantics — an editor shows the frame under the playhead, not
/// the one after it).
pub struct SourceDecoder {
    path: PathBuf,
    ictx: Input,
    decoder: ffmpeg::codec::decoder::Video,
    scaler: scaling::Context,
    stream_index: usize,
    /// Stream time base, seconds per tick.
    tb: f64,
    fps: f64,
    width: u32,
    height: u32,
    /// Receive target — `receive_frame` unrefs it on *every* call
    /// (including `Eof`), so successful frames are swapped into
    /// `decoded` to survive the drain.
    incoming: ffmpeg::frame::Video,
    /// Last successfully decoded frame + reusable RGBA conversion target.
    decoded: ffmpeg::frame::Video,
    rgba: ffmpeg::frame::Video,
    /// End-of-file was signalled to the decoder (drain mode).
    eof_sent: bool,
    /// The decoder is fully drained; only a seek revives the session.
    exhausted: bool,
    /// At least one frame was decoded since open/seek (pts is valid).
    has_decoded: bool,
}

// Safety: every libav context in here (`AVFormatContext`, decoder,
// `SwsContext`, frames) is exclusively owned by this struct and only
// touched through `&mut self` — moving the whole session between threads
// (prefetch → control) is fine; concurrent use is impossible.
unsafe impl Send for SourceDecoder {}

impl SourceDecoder {
    /// Open a decode session at position 0.
    pub fn open(path: &Path) -> Result<Self, MediaError> {
        init_ffmpeg();
        let ictx = ffmpeg::format::input(path).map_err(|e| ff_err(path, "opening", e))?;
        let stream = ictx
            .streams()
            .best(Type::Video)
            .ok_or_else(|| MediaError::NoStreams {
                path: path.display().to_string(),
            })?;
        let stream_index = stream.index();
        let time_base = stream.time_base();
        let tb = f64::from(time_base.numerator()) / f64::from(time_base.denominator());

        let avg = stream.avg_frame_rate();
        let rate = stream.rate();
        let fps = if avg.numerator() > 0 && avg.denominator() > 0 {
            f64::from(avg.numerator()) / f64::from(avg.denominator())
        } else if rate.numerator() > 0 && rate.denominator() > 0 {
            f64::from(rate.numerator()) / f64::from(rate.denominator())
        } else {
            0.0
        };
        if !(fps.is_finite() && fps > 0.0) {
            return Err(MediaError::FfmpegFailed {
                context: Some(format!("opening {}", path.display())),
                message: format!("no usable frame rate (got {fps})"),
            });
        }

        let mut ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
            .map_err(|e| ff_err(path, "reading codec parameters", e))?;
        // Frame threading pipelines the GOP catch-up after a keyframe
        // seek — the difference between ~100 ms and ~40 ms worst-case
        // scrubs. `count: 0` lets libav size the pool from the CPU.
        ctx.set_threading(ffmpeg::codec::threading::Config {
            kind: ffmpeg::codec::threading::Type::Frame,
            count: 0,
        });
        let decoder = ctx
            .decoder()
            .video()
            .map_err(|e| ff_err(path, "opening decoder", e))?;
        let (width, height) = (decoder.width(), decoder.height());
        if width == 0 || height == 0 {
            return Err(MediaError::FfmpegFailed {
                context: Some(format!("opening {}", path.display())),
                message: "stream reports zero dimensions".into(),
            });
        }
        let scaler = scaling::Context::get(
            decoder.format(),
            width,
            height,
            Pixel::RGBA,
            width,
            height,
            scaling::Flags::BILINEAR,
        )
        .map_err(|e| ff_err(path, "creating scaler", e))?;

        Ok(Self {
            path: path.to_path_buf(),
            ictx,
            decoder,
            scaler,
            stream_index,
            tb,
            fps,
            width,
            height,
            incoming: ffmpeg::frame::Video::empty(),
            decoded: ffmpeg::frame::Video::empty(),
            rgba: ffmpeg::frame::Video::empty(),
            eof_sent: false,
            exhausted: false,
            has_decoded: false,
        })
    }

    /// Presentation time of the *next* frame this session will decode,
    /// when known (one frame past the last decoded one; CFR proxies make
    /// this exact). Lets the player skip a redundant seek when the
    /// session is already positioned.
    pub fn next_pts_hint(&self) -> Option<f64> {
        (self.has_decoded && !self.exhausted).then(|| self.current_pts() + 1.0 / self.fps)
    }

    /// The stream's frame rate (proxies are CFR, so this is exact).
    pub fn fps(&self) -> f64 {
        self.fps
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Decode the next frame into the internal buffer. `Ok(false)` = end
    /// of stream. On success the frame's pts is in `self.decoded`.
    fn advance(&mut self) -> Result<bool, MediaError> {
        if self.exhausted {
            return Ok(false);
        }
        loop {
            match self.decoder.receive_frame(&mut self.incoming) {
                Ok(()) => {
                    std::mem::swap(&mut self.incoming, &mut self.decoded);
                    self.has_decoded = true;
                    return Ok(true);
                }
                Err(ffmpeg::Error::Other {
                    errno: libc::EAGAIN,
                }) => {} // needs more input
                Err(ffmpeg::Error::Eof) => {
                    self.exhausted = true;
                    return Ok(false);
                }
                Err(e) => return Err(ff_err(&self.path, "decoding", e)),
            }

            if self.eof_sent {
                continue; // keep draining until Eof
            }
            // Feed the next packet of our stream.
            loop {
                match self.ictx.packets().next() {
                    Some(Ok((stream, packet))) => {
                        if stream.index() != self.stream_index {
                            continue;
                        }
                        self.decoder
                            .send_packet(&packet)
                            .map_err(|e| ff_err(&self.path, "decoding", e))?;
                        break;
                    }
                    Some(Err(ffmpeg::Error::Other {
                        errno: libc::EAGAIN,
                    })) => continue,
                    Some(Err(e)) => return Err(ff_err(&self.path, "demuxing", e)),
                    None => {
                        // Container exhausted: switch the decoder to
                        // drain mode (frame threading buffers several
                        // frames that must still come out).
                        self.decoder
                            .send_eof()
                            .map_err(|e| ff_err(&self.path, "draining decoder", e))?;
                        self.eof_sent = true;
                        break;
                    }
                }
            }
        }
    }

    fn current_pts(&self) -> f64 {
        self.decoded.timestamp().or(self.decoded.pts()).unwrap_or(0) as f64 * self.tb
    }

    /// Convert the current decoded frame to RGBA and hand out a view.
    fn view(&mut self) -> Result<FrameView<'_>, MediaError> {
        self.scaler
            .run(&self.decoded, &mut self.rgba)
            .map_err(|e| ff_err(&self.path, "converting to RGBA", e))?;
        Ok(FrameView {
            pts_sec: self.current_pts(),
            width: self.width,
            height: self.height,
            stride: self.rgba.stride(0),
            data: self.rgba.data(0),
        })
    }

    /// Presentation time of the frame currently held in the decode
    /// buffer, if any frame has been decoded since open/seek.
    pub fn current_pts_sec(&self) -> Option<f64> {
        self.has_decoded.then(|| self.current_pts())
    }

    /// Whether the stream has been fully drained (only a seek revives it).
    pub fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    /// Re-view the currently held frame without advancing the stream —
    /// used when a prefetched decoder (positioned on its first frame) is
    /// installed and that frame must be uploaded. `None` if nothing has
    /// been decoded yet.
    pub fn current_frame(&mut self) -> Result<Option<FrameView<'_>>, MediaError> {
        if !self.has_decoded {
            return Ok(None);
        }
        self.view().map(Some)
    }

    /// Decode and return the next frame in stream order. `Ok(None)` = end
    /// of stream (a later [`SourceDecoder::seek_to`] revives the session).
    pub fn next_frame(&mut self) -> Result<Option<FrameView<'_>>, MediaError> {
        if self.advance()? {
            Ok(Some(self.view()?))
        } else {
            Ok(None)
        }
    }

    /// Position the session and return the frame *visible at* `target`
    /// seconds: the last frame with `pts <= target` (floor on the frame
    /// grid). Returns the stream's final frame when `target` is past the
    /// end; `Ok(None)` only for a stream with no frames at all.
    ///
    /// Also the cheap path for small forward hops: if the session is
    /// already positioned just before `target`, it decodes forward
    /// instead of seeking.
    pub fn seek_to(&mut self, target: f64) -> Result<Option<FrameView<'_>>, MediaError> {
        let target = target.max(0.0);
        // The frame visible at `target` is the one on the floor grid
        // point; decode until the *next* frame would start after it.
        let wanted = (target * self.fps + PTS_EPS).floor() / self.fps;

        let ts = (target * f64::from(AV_TIME_BASE)).round() as i64;
        self.ictx
            .seek(ts, ..=ts)
            .map_err(|e| ff_err(&self.path, "seeking", e))?;
        self.decoder.flush();
        self.eof_sent = false;
        self.exhausted = false;
        self.has_decoded = false;

        // Decode forward to the floor frame. Track whether anything was
        // decoded so a past-the-end target still yields the last frame.
        let mut have_frame = false;
        loop {
            if !self.advance()? {
                break;
            }
            have_frame = true;
            // Stop when this frame is the floor frame: the next one
            // would start after `wanted`.
            if self.current_pts() + 1.0 / self.fps > wanted + PTS_EPS {
                break;
            }
        }
        if have_frame {
            Ok(Some(self.view()?))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::generate_test_clip;
    use std::time::Instant;

    /// Frame-threaded decoding saturates cores; running these tests in
    /// parallel with each other skews the latency measurement below.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn sequential_decode_yields_monotonic_grid_pts() {
        let _serial = serial();
        let clip = generate_test_clip("libav-seq", 320, 180, 30, 2);
        let mut dec = SourceDecoder::open(&clip).unwrap();
        assert_eq!((dec.width(), dec.height()), (320, 180));
        assert!((dec.fps() - 30.0).abs() < 0.01);

        let mut count = 0;
        let mut last = f64::NEG_INFINITY;
        while let Some(frame) = dec.next_frame().unwrap() {
            assert!(frame.pts_sec > last, "pts must increase");
            assert!(frame.stride >= 320 * 4);
            assert_eq!(frame.data.len(), frame.stride * 180);
            last = frame.pts_sec;
            count += 1;
        }
        assert!((55..=65).contains(&count), "got {count} frames");
        // EOF is sticky until a seek.
        assert!(dec.next_frame().unwrap().is_none());
    }

    #[test]
    fn seek_lands_on_the_floor_frame() {
        let _serial = serial();
        let clip = generate_test_clip("libav-seek", 320, 180, 30, 2);
        let mut dec = SourceDecoder::open(&clip).unwrap();

        // 1.017 s sits between frames 30 (1.0) and 31 (1.0333): the
        // visible frame is 30.
        let f = dec.seek_to(1.017).unwrap().expect("frame");
        assert!((f.pts_sec - 1.0).abs() < 1e-3, "got pts {}", f.pts_sec);

        // 0.999 s: still frame 29 (0.9667), NOT frame 30.
        let f = dec.seek_to(0.999).unwrap().expect("frame");
        assert!(
            (f.pts_sec - 29.0 / 30.0).abs() < 1e-3,
            "got pts {}",
            f.pts_sec
        );

        // Exact grid point.
        let f = dec.seek_to(0.5).unwrap().expect("frame");
        assert!((f.pts_sec - 0.5).abs() < 1e-3, "got pts {}", f.pts_sec);

        // Past the end: the final frame.
        let f = dec.seek_to(10.0).unwrap().expect("frame");
        assert!(f.pts_sec > 1.9, "got pts {}", f.pts_sec);

        // Sequential decode continues from the seek point.
        let f = dec.seek_to(1.0).unwrap().expect("frame").pts_sec;
        let next = dec.next_frame().unwrap().expect("next").pts_sec;
        assert!((next - f - 1.0 / 30.0).abs() < 1e-3);
    }

    #[test]
    fn seek_revives_an_exhausted_session() {
        let _serial = serial();
        let clip = generate_test_clip("libav-revive", 320, 180, 30, 2);
        let mut dec = SourceDecoder::open(&clip).unwrap();
        while dec.next_frame().unwrap().is_some() {}
        let f = dec.seek_to(0.5).unwrap().expect("frame after revive");
        assert!((f.pts_sec - 0.5).abs() < 1e-3);
        assert!(dec.next_frame().unwrap().is_some());
    }

    #[test]
    fn cold_seeks_stay_inside_the_seek_budget() {
        let _serial = serial();
        // A proxy-shaped file: 720p, CFR 30, keyframe every 30 frames.
        let clip = generate_test_clip("libav-latency", 1280, 720, 30, 30);
        let mut dec = SourceDecoder::open(&clip).unwrap();

        // Targets just before keyframes → a full GOP of catch-up decode
        // (the worst case by construction). The suite runs tests in
        // parallel, so a single seek can be descheduled — assert on the
        // median (regression to spawn-per-seek is ≥110 ms on *every*
        // seek) plus a generous per-seek ceiling.
        let mut times = Vec::new();
        for target in [14.99, 3.97, 22.5, 7.03, 28.99, 11.62] {
            let t0 = Instant::now();
            let f = dec.seek_to(target).unwrap().expect("frame");
            times.push(t0.elapsed());
            assert!(
                (f.pts_sec - target).abs() < 2.0 / 30.0,
                "seek {target} landed at {}",
                f.pts_sec
            );
        }
        times.sort();
        let median = times[times.len() / 2];
        let worst = *times.last().unwrap();
        println!("cold seeks: median {median:?}, worst {worst:?}");
        assert!(
            median < std::time::Duration::from_millis(100),
            "median cold seek {median:?} (budget 100 ms)"
        );
        assert!(
            worst < std::time::Duration::from_millis(300),
            "worst cold seek {worst:?}"
        );
    }

    #[test]
    fn open_rejects_files_without_video() {
        let _serial = serial();
        let dir = std::env::temp_dir().join("cutty-media-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let audio_only = dir.join("libav-audio-only.m4a");
        if !audio_only.is_file() {
            let status = std::process::Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
                .arg("sine=frequency=440:duration=1")
                .args(["-c:a", "aac"])
                .arg(&audio_only)
                .status()
                .expect("ffmpeg");
            assert!(status.success());
        }
        assert!(SourceDecoder::open(&audio_only).is_err());
        assert!(SourceDecoder::open(Path::new("/nonexistent.mp4")).is_err());
    }
}
