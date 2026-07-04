//! Raw video frame decoding via ffmpeg (rgb24 over stdout).

use std::path::Path;

use ffmpeg_sidecar::child::FfmpegChild;
use ffmpeg_sidecar::command::FfmpegCommand;
use ffmpeg_sidecar::event::{FfmpegEvent, LogLevel, OutputVideoFrame};
use ffmpeg_sidecar::iter::FfmpegIterator;

use crate::error::MediaError;

/// A running ffmpeg decode session that yields raw rgb24 frames from a given
/// start position. Kill-and-restart is the seek mechanism: proxies carry a
/// keyframe at least every 30 frames, so restarts are cheap.
pub struct VideoDecoder {
    child: FfmpegChild,
    events: FfmpegIterator,
    /// Media time of the session's first frame, snapped to the frame grid.
    ///
    /// The sidecar fabricates `OutputVideoFrame::timestamp` as
    /// `frame_index / fps` relative to the session, and ffmpeg's input-side
    /// `-ss` outputs the first frame with pts ≥ the target — so the true
    /// media pts of frame `n` is `ceil(start·fps)/fps + n/fps` for CFR
    /// input (proxies are forced CFR).
    grid_start_sec: f64,
}

impl VideoDecoder {
    /// Spawn a decode session at `start_sec` (input-side `-ss`: keyframe
    /// seek plus accurate decode up to the target). `fps` must be the
    /// stream's real frame rate (from probe).
    pub fn open(path: &Path, start_sec: f64, fps: f64) -> Result<Self, MediaError> {
        let start_sec = start_sec.max(0.0);
        let grid_start_sec = (start_sec * fps - 1e-6).ceil().max(0.0) / fps;
        let mut child = FfmpegCommand::new()
            // Trim input analysis: proxies are known-good H.264 MP4s and
            // every millisecond here is seek latency.
            .args([
                "-probesize",
                "65536",
                "-analyzeduration",
                "0",
                "-threads",
                "2",
            ])
            .seek(format!("{start_sec:.6}"))
            .input(path.to_string_lossy())
            .no_audio()
            .format("rawvideo")
            .pix_fmt("rgb24")
            .output("-")
            .spawn()
            .map_err(|source| MediaError::Spawn {
                tool: "ffmpeg",
                source,
            })?;
        let events = child.iter().map_err(|e| MediaError::FfmpegFailed {
            context: Some("starting video decode".into()),
            message: e.to_string(),
        })?;
        Ok(Self {
            child,
            events,
            grid_start_sec,
        })
    }

    /// Pull the next decoded frame. `None` means end of stream.
    ///
    /// Blocks until ffmpeg produces a frame (it decodes far faster than
    /// realtime for 720p proxies, so in steady state frames are ready).
    pub fn next_frame(&mut self) -> Option<OutputVideoFrame> {
        for event in self.events.by_ref() {
            match event {
                FfmpegEvent::OutputFrame(frame) => return Some(frame),
                FfmpegEvent::Log(LogLevel::Error | LogLevel::Fatal, msg) => {
                    eprintln!("cutty-media: video decode: {msg}");
                }
                // NOTE: LogEOF (stderr closed) must NOT end the stream —
                // the stdout frame reader can still be flushing the final
                // frames. Done is sent by the stdout thread after the last
                // frame; the iterator itself ends once all senders drop.
                FfmpegEvent::Done => return None,
                _ => {}
            }
        }
        None
    }

    /// Absolute presentation time of a frame from this session.
    pub fn frame_pts(&self, frame: &OutputVideoFrame) -> f64 {
        self.grid_start_sec + f64::from(frame.timestamp)
    }

    /// Kill the session. Also runs on drop; explicit for clarity at call
    /// sites that restart decoders.
    pub fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::generate_test_clip;

    #[test]
    fn decodes_frames_with_monotonic_timestamps() {
        let clip = generate_test_clip("video-decode", 320, 180, 30, 2);
        let mut dec = VideoDecoder::open(&clip, 0.0, 30.0).unwrap();

        let first = dec.next_frame().expect("first frame");
        assert_eq!((first.width, first.height), (320, 180));
        assert_eq!(first.pix_fmt, "rgb24");
        assert_eq!(first.data.len(), 320 * 180 * 3);
        assert_eq!(first.frame_num, 0);

        let mut last_ts = f64::from(first.timestamp);
        let mut count = 1;
        while let Some(f) = dec.next_frame() {
            let ts = f64::from(f.timestamp);
            assert!(ts > last_ts - 1e-6, "timestamps must not go backwards");
            last_ts = ts;
            count += 1;
        }
        // 2 seconds at 30fps: allow container rounding slack.
        assert!((55..=65).contains(&count), "got {count} frames");
    }

    #[test]
    fn seeked_decode_starts_near_target() {
        let clip = generate_test_clip("video-seek", 320, 180, 30, 2);
        let mut dec = VideoDecoder::open(&clip, 1.0, 30.0).unwrap();
        let first = dec.next_frame().expect("frame after seek");
        // Relative timestamp restarts at ~0; absolute pts is start + rel.
        let pts = dec.frame_pts(&first);
        assert!(
            (0.95..=1.10).contains(&pts),
            "first frame after seek to 1.0s had pts {pts}"
        );
        dec.stop();
    }

    #[test]
    fn off_grid_seek_snaps_pts_to_the_frame_grid() {
        let clip = generate_test_clip("video-grid", 320, 180, 30, 2);
        // 0.98s is between frames 29 (0.9667) and 30 (1.0); ffmpeg outputs
        // frame 30 first, so reported pts must snap to 1.0.
        let mut dec = VideoDecoder::open(&clip, 0.98, 30.0).unwrap();
        let first = dec.next_frame().expect("frame");
        let pts = dec.frame_pts(&first);
        assert!((pts - 1.0).abs() < 1e-6, "pts {pts} not snapped to grid");
        dec.stop();
    }
}
