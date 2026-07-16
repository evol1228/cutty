//! Full-timeline export: frame-accurate re-encode from **original media**
//! (never proxies), mixed audio from the engine's mixing graph, muxed to
//! an MP4.
//!
//! # Two video paths, one picture definition
//!
//! **Compositor path (the general case).** The same GPU frame renderer
//! preview uses ([`crate::compose::TimelineRenderer`]) runs offline: for
//! every output frame, decode the originals in-process, composite the
//! full layer stack (transforms, opacity, blend modes) at output
//! resolution, read back, and pipe rawvideo into a single ffmpeg encode
//! process that muxes the audio mix in the same pass. Readback is
//! double-buffered across two staging slots so the GPU renders frame N+1
//! while the CPU writes frame N. Preview == export by construction; the
//! golden-frame tests in `tests/golden_frames.rs` enforce it.
//!
//! **Fast path (Phase 1 survivor).** A timeline that composites nothing —
//! at most one unmuted video track with clips, every clip at identity
//! transform, full opacity, normal blend — is encoded per segment
//! straight from the sources: one ffmpeg invocation per clip window
//! (frame-accurate input seek, scale/pad, fps-normalize), black filler
//! for gaps, joined losslessly with the concat demuxer. This skips the
//! decode → GPU → readback → pipe round-trip and keeps hardware-encoder
//! plumbing trivial, so plain cut-and-export jobs run as fast as they did
//! in Phase 1. [`fast_path_eligible`] decides; the chosen path is logged
//! and reported in [`ExportSummary::renderer`].
//!
//! # No lossless stream-copy ("smart copy") path — deliberately
//!
//! Frame-accurate cuts almost never land on keyframes, so stream copy
//! cannot honor them (the Phase 0 trim export snaps to keyframes
//! instead). A future smart-copy optimization can stream-copy the
//! keyframe-aligned middle of untouched clips and re-encode only the
//! boundary GOPs, making plain-cut exports near-instant — that lands with
//! a later phase; correctness comes first here.
//!
//! # Audio == preview, by construction
//!
//! The audio side never re-derives anything: the same
//! [`cutty_audio::mixer`] code that feeds the speakers renders the full
//! mix offline to a WAV ([`cutty_audio::offline`]), from the same
//! resolved files playback uses (proxy audio for video media, originals
//! for audio-only media) with the same clip volumes, and that WAV is
//! muxed under the video. Decoding *original* audio for video media is a
//! quality upgrade for a later phase — it needs a libav-backed
//! `AudioSource` for codecs symphonia doesn't cover (opus/ac3/dts).

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use cutty_audio::{AudioSegment, MixerTimeline, EXPORT_SAMPLE_RATE};
use cutty_engine::{timeline_end, BlendMode, Project, TrackKind};
use ffmpeg_sidecar::command::FfmpegCommand;
use ffmpeg_sidecar::event::{FfmpegEvent, LogLevel};
use ffmpeg_sidecar::paths::ffmpeg_path;

use crate::compose::TimelineRenderer;
use crate::encoders::{detected_h264_encoder, H264Encoder};
use crate::error::MediaError;
use crate::proxy::parse_ffmpeg_time;
use crate::tools::ensure_tools;

/// CRF-style quality tiers exposed in the export dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExportQuality {
    High,
    Medium,
    /// "Small file".
    Small,
}

/// Everything the export needs to know about the target.
#[derive(Debug, Clone)]
pub struct ExportSpec {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub quality: ExportQuality,
    /// Final MP4 path (written atomically via a `.part` sibling).
    pub dst: PathBuf,
}

/// Which stage the export is in (progress display).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ExportStage {
    Audio,
    Video,
    Finalize,
}

/// A progress report from a running export.
#[derive(Debug, Clone, Copy)]
pub struct ExportProgress {
    pub stage: ExportStage,
    /// Overall completion, 0.0–100.0.
    pub percent: f32,
    /// Wall-clock estimate of the time remaining, once measurable.
    pub eta_sec: Option<f64>,
    /// Current encode speed as a multiple of realtime (0 when unknown).
    pub speed: f32,
}

/// What a finished export produced.
#[derive(Debug, Clone)]
pub struct ExportSummary {
    pub path: PathBuf,
    pub duration_sec: f64,
    /// ffmpeg name of the video encoder used (e.g. `h264_vaapi`).
    pub encoder: &'static str,
    pub hardware_encode: bool,
    /// Which video pipeline rendered the frames: `"segment-concat"` (the
    /// fast path) or `"gpu-compositor"`.
    pub renderer: &'static str,
}

// ---------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------

/// Cross-thread cancel handle for a running export.
///
/// `cancel()` flips the flag (checked between blocks/segments/events) and
/// SIGKILLs the currently running ffmpeg child so even a stalled encode
/// aborts instantly. The killed child is still reaped by the export
/// thread (`wait()`), so no zombies are left behind.
#[derive(Debug, Default)]
pub struct CancelToken {
    cancelled: AtomicBool,
    /// Pid of the ffmpeg child currently owned by the export thread.
    child_pid: Mutex<Option<u32>>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Request cancellation and kill the in-flight ffmpeg process (if
    /// any). Kill-by-pid has a theoretical pid-reuse race, but the slot
    /// is cleared before the child is reaped, so the window is the few
    /// microseconds between a load here and the export thread's `wait` —
    /// and a stale SIGKILL to a since-reaped pid is overwhelmingly likely
    /// to hit nothing (EPERM/ESRCH) rather than a bystander.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        let pid = *self.child_pid.lock().expect("cancel token poisoned");
        if let Some(pid) = pid {
            // SAFETY: plain syscall; killing an already-dead pid is a no-op.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }

    fn watch_child(&self, pid: u32) {
        *self.child_pid.lock().expect("cancel token poisoned") = Some(pid);
    }

    fn clear_child(&self) {
        *self.child_pid.lock().expect("cancel token poisoned") = None;
    }

    fn bail_if_cancelled(&self) -> Result<(), MediaError> {
        if self.is_cancelled() {
            Err(MediaError::ExportCancelled)
        } else {
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------
// Planning: timeline → segments on the output frame grid
// ---------------------------------------------------------------------

/// What fills one contiguous run of output frames.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SegmentSource {
    /// A window into an original media file.
    Clip {
        path: PathBuf,
        /// Source position of the segment's first output frame, already
        /// adjusted for the frame-grid rounding of the timeline in-point.
        source_in: f64,
        speed: f64,
    },
    /// Timeline gap: solid black.
    Black,
}

/// One planned video segment: `[start_frame, end_frame)` on the output
/// frame grid.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlannedSegment {
    pub source: SegmentSource,
    pub start_frame: i64,
    pub end_frame: i64,
}

impl PlannedSegment {
    pub fn frames(&self) -> i64 {
        self.end_frame - self.start_frame
    }
}

/// Snap a timeline instant to the output frame grid.
fn frame_at(t: f64, fps: f64) -> i64 {
    (t * fps).round() as i64
}

/// Whether a clip renders with no compositing work at all: identity
/// transform, full opacity, normal blend, and no transition at its out
/// cut (a transition needs two decoded streams and a shader).
fn visually_default(clip: &cutty_engine::Clip) -> bool {
    clip.transform.is_identity()
        && clip.opacity == 1.0
        && clip.blend_mode == BlendMode::Normal
        && clip.transition_out.is_none()
}

/// The single visible video track carrying clips, if the project has
/// exactly one (the shape the per-segment fast path can encode).
fn sole_video_track(project: &Project) -> Option<&cutty_engine::Track> {
    let mut tracks = project
        .tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Video && !t.hidden && !t.clips.is_empty());
    let first = tracks.next()?;
    tracks.next().is_none().then_some(first)
}

/// Whether the Phase 1 per-segment pipeline renders this project exactly:
/// nothing to composite (at most one visible video track holding clips,
/// no visible text) and every clip visually default. Everything else
/// goes through the GPU compositor.
pub(crate) fn fast_path_eligible(project: &Project) -> bool {
    // Any visible text is composited — the segment pipeline can't draw it.
    if project
        .tracks
        .iter()
        .any(|t| t.kind == TrackKind::Text && !t.hidden && !t.clips.is_empty())
    {
        return false;
    }
    let mut tracks = project
        .tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Video && !t.hidden && !t.clips.is_empty());
    let Some(track) = tracks.next() else {
        return true; // no video at all: black frames + the audio mix
    };
    if tracks.next().is_some() {
        return false;
    }
    // Stills, loops and alpha sources need the compositor (the ffmpeg
    // segment pipeline neither loops GIFs, holds stills, nor preserves
    // alpha semantics against the black base).
    let plain_media = track.clips.iter().all(|c| {
        c.media_id
            .and_then(|id| project.media(id))
            .is_some_and(|m| m.kind == cutty_engine::MediaKind::Video && !m.has_alpha)
    });
    plain_media && track.clips.iter().all(visually_default)
}

/// Walk the fast path's single video track into segments covering
/// `[0, timeline_end)` — one per clip window, black filler for gaps and
/// for any trailing stretch under audio that outlives the video. Segment
/// boundaries are snapped once to the output frame grid so the segment
/// frame counts sum exactly to the total (no cumulative rounding drift).
///
/// Only meaningful when [`fast_path_eligible`] holds (the compositor path
/// needs no plan — it renders every grid frame).
pub(crate) fn plan_video_segments(project: &Project, fps: f64) -> Vec<PlannedSegment> {
    let end = timeline_end(project);
    let total_frames = frame_at(end, fps);
    let mut segments: Vec<PlannedSegment> = Vec::new();
    let mut cursor = 0i64;

    if let Some(track) = sole_video_track(project) {
        for clip in &track.clips {
            let start_frame = frame_at(clip.timeline_in, fps).min(total_frames);
            let end_frame = frame_at(clip.timeline_out.min(end), fps).min(total_frames);
            if start_frame > cursor {
                segments.push(PlannedSegment {
                    source: SegmentSource::Black,
                    start_frame: cursor,
                    end_frame: start_frame,
                });
                cursor = start_frame;
            }
            if end_frame > cursor {
                let media = clip
                    .media_id
                    .and_then(|id| project.media(id))
                    .expect("validated video clip has media");
                // The first output frame sits at start_frame/fps, which
                // can differ from timeline_in by up to half a frame;
                // shift the source in-point to match.
                let grid_t = cursor as f64 / fps;
                let source_in =
                    (clip.source_in + (grid_t - clip.timeline_in) * clip.speed).max(0.0);
                segments.push(PlannedSegment {
                    source: SegmentSource::Clip {
                        path: PathBuf::from(&media.path),
                        source_in,
                        speed: clip.speed,
                    },
                    start_frame: cursor,
                    end_frame,
                });
                cursor = end_frame;
            }
        }
    }
    if cursor < total_frames {
        segments.push(PlannedSegment {
            source: SegmentSource::Black,
            start_frame: cursor,
            end_frame: total_frames,
        });
    }
    segments
}

/// The mixer input for export: every audio-contributing clip on an
/// unmuted track, resolved to **original media** — export never touches
/// proxies (their audio is a 128k stereo AAC transcode; the original is
/// the real thing). Codecs symphonia doesn't decode (ac3/dts/opus) go
/// through the libav fallback in [`crate::audio_source`]. Transition
/// crossfades and volume-keyframe envelopes come from the same
/// [`crate::audio_layout`] resolution the live mixer uses, so the
/// exported mix has the exact preview envelope. Public for the envelope
/// acceptance tests; `run_export` drives it internally.
pub fn export_audio_timeline(project: &Project) -> Result<MixerTimeline, MediaError> {
    let spans = cutty_engine::transition_spans(project);
    let mut segments = Vec::new();
    for track in project.tracks.iter().filter(|t| !t.muted) {
        for clip in &track.clips {
            // Text clips (`media_id: None`) contribute no audio.
            let Some(media) = clip.media_id.and_then(|id| project.media(id)) else {
                continue;
            };
            if !media.has_audio {
                continue;
            }
            let path = PathBuf::from(&media.path);
            let placement = crate::audio_layout::audio_placement(clip, &spans);
            segments.push(AudioSegment {
                path,
                timeline_in: placement.timeline_in,
                timeline_out: placement.timeline_out,
                source_in: placement.source_in,
                speed: clip.speed,
                volume: clip.volume,
                envelope: crate::audio_layout::volume_envelope(clip),
                fade_in: placement.fade_in,
                fade_out: placement.fade_out,
            });
        }
    }
    Ok(MixerTimeline { segments })
}

// ---------------------------------------------------------------------
// ffmpeg argument builders (pure, unit-tested)
// ---------------------------------------------------------------------

impl ExportQuality {
    /// CRF-style quality knob per encoder (lower = better).
    fn video_q(self, encoder: &H264Encoder) -> u32 {
        match (encoder, self) {
            (H264Encoder::X264, ExportQuality::High) => 18,
            (H264Encoder::X264, ExportQuality::Medium) => 23,
            (H264Encoder::X264, ExportQuality::Small) => 28,
            // VAAPI CQP and NVENC CQ scales sit close to CRF; one notch
            // conservative on High to offset hw-encoder efficiency loss.
            (_, ExportQuality::High) => 19,
            (_, ExportQuality::Medium) => 24,
            (_, ExportQuality::Small) => 28,
        }
    }

    fn audio_bitrate(self) -> &'static str {
        match self {
            ExportQuality::High => "192k",
            ExportQuality::Medium => "160k",
            ExportQuality::Small => "128k",
        }
    }
}

/// The software filter chain normalizing one clip segment to the target
/// frame: rebase timestamps (and apply speed), resample to the target
/// fps, fit within the frame preserving aspect, pad to exact size,
/// square pixels, then clone the last frame forever (`tpad`) so
/// `-frames:v` always reaches its exact count even if the source runs a
/// frame short.
fn clip_filter(spec: &ExportSpec, speed: f64, vaapi: bool) -> String {
    let setpts = if (speed - 1.0).abs() < 1e-9 {
        "setpts=PTS-STARTPTS".to_string()
    } else {
        format!("setpts=(PTS-STARTPTS)/{speed}")
    };
    let mut chain = format!(
        "{setpts},fps={fps},\
         scale={w}:{h}:force_original_aspect_ratio=decrease:force_divisible_by=2:out_color_matrix=bt709,\
         pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1,\
         tpad=stop_mode=clone:stop=-1",
        fps = spec.fps,
        w = spec.width,
        h = spec.height,
    );
    if vaapi {
        chain.push_str(",format=nv12,hwupload");
    }
    chain
}

/// Output-side video encoder arguments, shared by both video paths (for
/// segments, identical parameters are what make the concat demuxer's
/// `-c copy` join safe).
fn encoder_args(spec: &ExportSpec, encoder: &H264Encoder) -> Vec<String> {
    let q = spec.quality.video_q(encoder).to_string();
    let mut args: Vec<String> = match encoder {
        H264Encoder::X264 => [
            "-c:v", "libx264", "-preset", "medium", "-crf", &q, "-pix_fmt", "yuv420p",
        ]
        .map(String::from)
        .to_vec(),
        H264Encoder::Nvenc => [
            "-c:v",
            "h264_nvenc",
            "-preset",
            "p5",
            "-rc",
            "vbr",
            "-cq",
            &q,
            "-b:v",
            "0",
            "-pix_fmt",
            "yuv420p",
        ]
        .map(String::from)
        .to_vec(),
        H264Encoder::Vaapi { .. } => ["-c:v", "h264_vaapi", "-rc_mode", "CQP", "-qp", &q]
            .map(String::from)
            .to_vec(),
    };
    // Uniform GOP and colorimetry signaling (sources with differing tags
    // would otherwise produce mismatched SPS across segments).
    args.extend(
        [
            "-g",
            &((spec.fps * 2.0).round() as u32).to_string(),
            "-colorspace",
            "bt709",
            "-color_primaries",
            "bt709",
            "-color_trc",
            "bt709",
            "-fps_mode",
            "cfr",
        ]
        .map(String::from),
    );
    args
}

/// Full argument list encoding one planned segment to `out`.
fn segment_args(
    spec: &ExportSpec,
    encoder: &H264Encoder,
    segment: &PlannedSegment,
    out: &Path,
) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if let H264Encoder::Vaapi { device } = encoder {
        args.extend(["-vaapi_device".into(), device.display().to_string()]);
    }
    let vaapi = matches!(encoder, H264Encoder::Vaapi { .. });

    match &segment.source {
        SegmentSource::Clip {
            path,
            source_in,
            speed,
        } => {
            let source_span = segment.frames() as f64 / spec.fps * speed;
            // Input-side seek + read limit: with re-encode, `-ss` before
            // `-i` is frame-accurate (decode from the prior keyframe,
            // discard up to the target). The read margin gives fps
            // resampling its last frame; tpad covers any shortfall.
            args.extend([
                "-ss".into(),
                format!("{source_in:.6}"),
                "-t".into(),
                format!("{:.6}", source_span + 2.0 * speed / spec.fps),
                "-i".into(),
                path.display().to_string(),
                "-vf".into(),
                clip_filter(spec, *speed, vaapi),
            ]);
        }
        SegmentSource::Black => {
            args.extend([
                "-f".into(),
                "lavfi".into(),
                "-i".into(),
                format!(
                    "color=black:size={}x{}:rate={}",
                    spec.width, spec.height, spec.fps
                ),
            ]);
            if vaapi {
                args.extend(["-vf".into(), "format=nv12,hwupload".into()]);
            }
        }
    }

    args.extend(["-frames:v".into(), segment.frames().to_string()]);
    args.extend(encoder_args(spec, encoder));
    args.extend(["-an".into(), "-y".into(), out.display().to_string()]);
    args
}

/// Argument list for the compositor path's single encode process:
/// rawvideo RGBA frames on stdin + the audio WAV, one pass to the final
/// (`.part`) MP4. RGB→YUV conversion is pinned to bt709/limited range so
/// the colorimetry matches the fast path's segments.
fn raw_export_args(
    spec: &ExportSpec,
    encoder: &H264Encoder,
    wav: &Path,
    out: &Path,
) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if let H264Encoder::Vaapi { device } = encoder {
        args.extend(["-vaapi_device".into(), device.display().to_string()]);
    }
    args.extend(
        [
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "-video_size",
            &format!("{}x{}", spec.width, spec.height),
            "-framerate",
            &format!("{}", spec.fps),
            "-i",
            "pipe:0",
            "-i",
            &wav.display().to_string(),
            "-map",
            "0:v:0",
            "-map",
            "1:a:0",
        ]
        .map(String::from),
    );
    let vf = if matches!(encoder, H264Encoder::Vaapi { .. }) {
        "scale=out_color_matrix=bt709:out_range=tv,format=nv12,hwupload"
    } else {
        "scale=out_color_matrix=bt709:out_range=tv,format=yuv420p"
    };
    args.extend(["-vf".into(), vf.to_string()]);
    args.extend(encoder_args(spec, encoder));
    args.extend(
        [
            "-c:a",
            "aac",
            "-b:a",
            spec.quality.audio_bitrate(),
            "-movflags",
            "+faststart",
            // The `.part` name hides the container from extension sniffing.
            "-f",
            "mp4",
            "-y",
            &out.display().to_string(),
        ]
        .map(String::from),
    );
    args
}

/// Argument list for the final pass: concat the segments (stream copy —
/// they are encode-identical by construction) and mux in the audio mix.
fn mux_args(spec: &ExportSpec, list: &Path, wav: &Path, out: &Path) -> Vec<String> {
    [
        "-f",
        "concat",
        "-safe",
        "0",
        "-i",
        &list.display().to_string(),
        "-i",
        &wav.display().to_string(),
        "-map",
        "0:v:0",
        "-map",
        "1:a:0",
        "-c:v",
        "copy",
        "-c:a",
        "aac",
        "-b:a",
        spec.quality.audio_bitrate(),
        "-movflags",
        "+faststart",
        // The `.part` name hides the container from extension sniffing.
        "-f",
        "mp4",
        "-y",
        &out.display().to_string(),
    ]
    .map(String::from)
    .to_vec()
}

/// Concat-demuxer list file body. Single quotes in paths are closed,
/// escaped, and reopened per the demuxer's quoting rules.
fn concat_list(paths: &[PathBuf]) -> String {
    let mut body = String::new();
    for p in paths {
        let escaped = p.display().to_string().replace('\'', "'\\''");
        body.push_str("file '");
        body.push_str(&escaped);
        body.push_str("'\n");
    }
    body
}

// ---------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------

/// Removes the export's temp directory (and the `.part` output) on drop,
/// so every exit path — success, error, cancel, panic — cleans up.
struct CleanupGuard {
    temp_dir: PathBuf,
    part: PathBuf,
    /// On success the final file was renamed away; keep it.
    keep_part: bool,
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.temp_dir);
        if !self.keep_part {
            let _ = std::fs::remove_file(&self.part);
        }
    }
}

/// Wall-clock progress reporter: stages are weighted (audio render is
/// decode-speed fast, video encode dominates, finalize is stream copy).
struct Progress<'a> {
    sink: &'a mut dyn FnMut(ExportProgress),
    started: Instant,
}

const AUDIO_END: f32 = 4.0;
const VIDEO_END: f32 = 96.0;

impl Progress<'_> {
    fn report(&mut self, stage: ExportStage, fraction: f64, speed: f32) {
        let (lo, hi) = match stage {
            ExportStage::Audio => (0.0, AUDIO_END),
            ExportStage::Video => (AUDIO_END, VIDEO_END),
            ExportStage::Finalize => (VIDEO_END, 100.0),
        };
        let percent = lo + (hi - lo) * (fraction.clamp(0.0, 1.0) as f32);
        let frac = f64::from(percent) / 100.0;
        let eta_sec = (frac > 0.02).then(|| {
            let elapsed = self.started.elapsed().as_secs_f64();
            (elapsed * (1.0 - frac) / frac).max(0.0)
        });
        (self.sink)(ExportProgress {
            stage,
            percent,
            eta_sec,
            speed,
        });
    }
}

/// Run one ffmpeg invocation to completion under the cancel token:
/// registers the child pid for cross-thread kill, forwards `Progress`
/// events (out-time seconds) to `on_time`, collects error log lines, and
/// always reaps the child.
fn run_ffmpeg(
    args: &[String],
    context: &str,
    cancel: &CancelToken,
    mut on_time: impl FnMut(f64, f32),
) -> Result<(), MediaError> {
    cancel.bail_if_cancelled()?;
    let mut child = FfmpegCommand::new()
        .args(args.iter().map(String::as_str))
        .spawn()
        .map_err(|source| MediaError::Spawn {
            tool: "ffmpeg",
            source,
        })?;
    cancel.watch_child(child.as_inner().id());

    let mut errors: Vec<String> = Vec::new();
    let iter = child.iter().map_err(|e| MediaError::FfmpegFailed {
        context: Some(context.to_string()),
        message: e.to_string(),
    });
    match iter {
        Ok(iter) => {
            for event in iter {
                match event {
                    FfmpegEvent::Progress(p) => {
                        on_time(parse_ffmpeg_time(&p.time), p.speed);
                        if cancel.is_cancelled() {
                            // The kill also lands via pid; this closes the
                            // no-event window between check and kill.
                            let _ = child.kill();
                        }
                    }
                    FfmpegEvent::Log(LogLevel::Error | LogLevel::Fatal, msg) => errors.push(msg),
                    _ => {}
                }
            }
        }
        Err(e) => {
            let _ = child.kill();
            cancel.clear_child();
            let _ = child.wait();
            return Err(e);
        }
    }

    // Reap. Clear the pid slot first so a racing cancel() can't target a
    // recycled pid after the wait returns.
    cancel.clear_child();
    let status = child.wait()?;
    cancel.bail_if_cancelled()?;
    if !status.success() {
        return Err(MediaError::FfmpegFailed {
            context: Some(context.to_string()),
            message: if errors.is_empty() {
                format!("ffmpeg exited with {status}")
            } else {
                errors.join("; ")
            },
        });
    }
    Ok(())
}

/// Export `project` per `spec`. Blocking — run on a worker thread; wire
/// `cancel` to the UI's Cancel button. Progress lands on `on_progress`
/// (already coalesced to a sane cadence by ffmpeg's ~0.5 s stats period).
pub fn run_export(
    project: &Project,
    spec: &ExportSpec,
    cancel: &CancelToken,
    on_progress: &mut dyn FnMut(ExportProgress),
) -> Result<ExportSummary, MediaError> {
    ensure_tools()?;
    if spec.width == 0
        || spec.height == 0
        || !spec.width.is_multiple_of(2)
        || !spec.height.is_multiple_of(2)
    {
        return Err(MediaError::ExportNotReady {
            message: format!("invalid export resolution {}x{}", spec.width, spec.height),
        });
    }
    if !(spec.fps.is_finite() && spec.fps > 0.0) {
        return Err(MediaError::ExportNotReady {
            message: format!("invalid export frame rate {}", spec.fps),
        });
    }

    // Missing originals fail here, before any work: rendering black where
    // the user had content must never happen silently.
    for media in &project.media {
        let used = project
            .tracks
            .iter()
            .any(|t| t.clips.iter().any(|c| c.media_id == Some(media.id)));
        if used && !Path::new(&media.path).is_file() {
            return Err(MediaError::ExportNotReady {
                message: format!("source file is missing: {}", media.path),
            });
        }
    }

    let total_frames = frame_at(timeline_end(project), spec.fps);
    if total_frames <= 0 {
        return Err(MediaError::ExportNotReady {
            message: "the timeline is empty — add clips before exporting".into(),
        });
    }
    let audio_timeline = export_audio_timeline(project)?;
    let encoder = detected_h264_encoder();
    let duration_sec = total_frames as f64 / spec.fps;
    let fast_path = fast_path_eligible(project);
    let renderer = if fast_path {
        "segment-concat"
    } else {
        "gpu-compositor"
    };
    eprintln!("cutty-media: export video path: {renderer}");

    // Temp segments can be large; use the cache dir, not tmpfs.
    static JOB_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let temp_dir = crate::cache::cache_dir("export")?.join(format!(
        "job-{}-{}",
        std::process::id(),
        JOB_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&temp_dir)?;
    let part = spec.dst.with_extension("mp4.part");
    let mut guard = CleanupGuard {
        temp_dir: temp_dir.clone(),
        part: part.clone(),
        keep_part: false,
    };
    if let Some(parent) = spec.dst.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut progress = Progress {
        sink: on_progress,
        started: Instant::now(),
    };

    // --- Stage 1: the audio mix (exact video duration) ---
    // Sources decode through the symphonia→libav chain so original-media
    // audio works regardless of codec (ac3/dts/opus included).
    let wav = temp_dir.join("mix.wav");
    let audio_frames =
        (total_frames as f64 / spec.fps * f64::from(EXPORT_SAMPLE_RATE)).round() as u64;
    cutty_audio::render_timeline_to_wav_with_factory(
        audio_timeline,
        EXPORT_SAMPLE_RATE,
        audio_frames,
        &wav,
        &|| cancel.is_cancelled(),
        &mut |done, total| {
            progress.report(ExportStage::Audio, done as f64 / total as f64, 0.0);
        },
        &mut crate::audio_source::open_audio_source,
    )
    .map_err(|e| match e {
        cutty_audio::AudioError::Cancelled => MediaError::ExportCancelled,
        other => MediaError::Audio(other),
    })?;

    if fast_path {
        // --- Stage 2 (fast): encode each video segment ---
        let segments = plan_video_segments(project, spec.fps);
        let mut part_files: Vec<PathBuf> = Vec::new();
        let mut frames_done: i64 = 0;
        for (idx, segment) in segments.iter().enumerate() {
            let out = temp_dir.join(format!("seg-{idx:04}.mp4"));
            let context = match &segment.source {
                SegmentSource::Clip { path, .. } => format!(
                    "encoding segment {} of {} ({})",
                    idx + 1,
                    segments.len(),
                    path.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string())
                ),
                SegmentSource::Black => {
                    format!("encoding segment {} of {} (gap)", idx + 1, segments.len())
                }
            };
            let args = segment_args(spec, encoder, segment, &out);
            run_ffmpeg(&args, &context, cancel, |out_time, speed| {
                let seg_frames = (out_time * spec.fps).min(segment.frames() as f64);
                let done = (frames_done as f64 + seg_frames) / total_frames as f64;
                progress.report(ExportStage::Video, done, speed);
            })?;
            frames_done += segment.frames();
            progress.report(
                ExportStage::Video,
                frames_done as f64 / total_frames as f64,
                0.0,
            );
            part_files.push(out);
        }

        // --- Stage 3 (fast): concat + mux (video copied, audio → AAC) ---
        let list = temp_dir.join("concat.txt");
        let mut list_file = std::fs::File::create(&list)?;
        list_file.write_all(concat_list(&part_files).as_bytes())?;
        list_file.sync_all()?;
        drop(list_file);

        progress.report(ExportStage::Finalize, 0.0, 0.0);
        run_ffmpeg(
            &mux_args(spec, &list, &wav, &part),
            "muxing the final file",
            cancel,
            |out_time, speed| {
                progress.report(ExportStage::Finalize, out_time / duration_sec, speed);
            },
        )?;
    } else {
        // --- Stage 2+3 (compositor): render every frame, pipe into one
        // encode+mux process ---
        composite_and_encode(
            project,
            spec,
            encoder,
            &wav,
            &part,
            total_frames,
            cancel,
            &mut progress,
        )?;
    }

    std::fs::rename(&part, &spec.dst)?;
    guard.keep_part = true; // renamed away; nothing to delete
    progress.report(ExportStage::Finalize, 1.0, 0.0);

    Ok(ExportSummary {
        path: spec.dst.clone(),
        duration_sec,
        encoder: encoder.ffmpeg_name(),
        hardware_encode: encoder.is_hardware(),
        renderer,
    })
}

// ---------------------------------------------------------------------
// The compositor video path
// ---------------------------------------------------------------------

/// Counters from a compositor-path frame run (logged for the perf
/// budget; the readback wait is the time spent blocked on GPU→CPU
/// copies, which double buffering is meant to hide).
pub struct CompositeRunStats {
    pub frames: i64,
    pub readback_wait: Duration,
    pub adapter: String,
}

/// Sink for composited frames: `(frame index, padded RGBA rows, row
/// pitch)`.
pub type FrameSink<'a> = dyn FnMut(i64, &[u8], usize) -> Result<(), MediaError> + 'a;

/// Drive the compositor frame loop at output resolution: for every output
/// frame, decode originals in-process, composite the layer stack, and
/// hand the padded RGBA readback to `emit(frame_idx, data, stride)`.
/// Readback is double-buffered: frame N+1's decode+composite is submitted
/// before frame N's readback is consumed. This is THE export frame
/// generator — the golden-frame tests drive it directly against the
/// preview renderer.
pub fn for_each_composited_frame(
    project: &Project,
    width: u32,
    height: u32,
    fps: f64,
    total_frames: i64,
    should_cancel: &dyn Fn() -> bool,
    emit: &mut FrameSink<'_>,
) -> Result<CompositeRunStats, MediaError> {
    let mut renderer = TimelineRenderer::new(width, height, true)?;
    let originals: HashMap<u64, PathBuf> = project
        .media
        .iter()
        .filter(|m| m.has_video)
        .map(|m| (m.id.0, PathBuf::from(&m.path)))
        .collect();
    let path_of = |media: u64| originals.get(&media).cloned();

    let mut readback_wait = Duration::ZERO;
    if total_frames > 0 {
        renderer.begin_frame(project, 0.0, &path_of, 0)?;
    }
    for idx in 0..total_frames {
        if should_cancel() {
            return Err(MediaError::ExportCancelled);
        }
        // Submit the next frame before consuming this one: the GPU
        // renders N+1 while the CPU waits on / writes N.
        if idx + 1 < total_frames {
            renderer.begin_frame(
                project,
                (idx + 1) as f64 / fps,
                &path_of,
                ((idx + 1) % 2) as usize,
            )?;
        }
        let started = Instant::now();
        let mut wait = Duration::ZERO;
        renderer.read_frame((idx % 2) as usize, |frame| {
            wait = started.elapsed();
            emit(idx, frame.data, frame.stride)
        })??;
        readback_wait += wait;
    }
    Ok(CompositeRunStats {
        frames: total_frames,
        readback_wait,
        adapter: renderer.adapter_label(),
    })
}

/// The compositor path's encode stage: spawn one ffmpeg (rawvideo stdin +
/// audio WAV → `.part` MP4) and stream every composited frame into it.
#[allow(clippy::too_many_arguments)]
fn composite_and_encode(
    project: &Project,
    spec: &ExportSpec,
    encoder: &H264Encoder,
    wav: &Path,
    part: &Path,
    total_frames: i64,
    cancel: &CancelToken,
    progress: &mut Progress<'_>,
) -> Result<(), MediaError> {
    cancel.bail_if_cancelled()?;
    let args = raw_export_args(spec, encoder, wav, part);
    let mut child = std::process::Command::new(ffmpeg_path())
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|source| MediaError::Spawn {
            tool: "ffmpeg",
            source,
        })?;
    cancel.watch_child(child.id());

    // Drain stderr on a side thread (ffmpeg blocks when the pipe fills);
    // keep the last lines for error reporting.
    let stderr = child.stderr.take().expect("stderr piped");
    let stderr_tail = std::thread::spawn(move || {
        use std::io::BufRead;
        let mut tail: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        for line in std::io::BufReader::new(stderr)
            .lines()
            .map_while(Result::ok)
        {
            if tail.len() == 40 {
                tail.pop_front();
            }
            tail.push_back(line);
        }
        tail.into_iter().collect::<Vec<_>>().join("; ")
    });

    let stdin = child.stdin.take().expect("stdin piped");
    let mut writer = std::io::BufWriter::with_capacity(1 << 20, stdin);
    let row_bytes = spec.width as usize * 4;
    let height = spec.height as usize;
    let started = Instant::now();
    let mut bytes_out: u64 = 0;

    let run = for_each_composited_frame(
        project,
        spec.width,
        spec.height,
        spec.fps,
        total_frames,
        &|| cancel.is_cancelled(),
        &mut |idx, data, stride| {
            for row in 0..height {
                writer
                    .write_all(&data[row * stride..row * stride + row_bytes])
                    .map_err(MediaError::Io)?;
            }
            bytes_out += (row_bytes * height) as u64;
            if idx % 15 == 0 {
                let elapsed = started.elapsed().as_secs_f64().max(1e-6);
                let speed = (idx as f64 / spec.fps / elapsed) as f32;
                progress.report(ExportStage::Video, idx as f64 / total_frames as f64, speed);
            }
            Ok(())
        },
    );

    // Close stdin (flush) so ffmpeg finalizes, then reap — on *every*
    // path, including render errors and cancel. Clearing the pid slot
    // before the reap keeps a racing `cancel()` off recycled pids.
    let flush = writer.flush();
    drop(writer);
    progress.report(ExportStage::Finalize, 0.0, 0.0);
    cancel.clear_child();
    let status = child.wait().map_err(MediaError::Io);
    let tail = stderr_tail.join().unwrap_or_default();

    // Error precedence: cancel, then the encoder's own failure (a dead
    // ffmpeg surfaces to the render loop as EPIPE — its stderr is the
    // real story), then render/pipe errors.
    cancel.bail_if_cancelled()?;
    let status = status?;
    if !status.success() {
        return Err(MediaError::FfmpegFailed {
            context: Some("compositor encode".into()),
            message: if tail.is_empty() {
                format!("ffmpeg exited with {status}")
            } else {
                tail
            },
        });
    }
    let run = run?;
    flush?;

    let elapsed = started.elapsed().as_secs_f64().max(1e-6);
    eprintln!(
        "cutty-media: compositor export: {} frames at {}x{} in {:.2}s ({:.1} fps), \
         readback wait {:.0} ms total ({:.2} GB/s effective), {:.1} MB piped, on {}",
        run.frames,
        spec.width,
        spec.height,
        elapsed,
        run.frames as f64 / elapsed,
        run.readback_wait.as_secs_f64() * 1e3,
        (bytes_out as f64 / 1e9) / run.readback_wait.as_secs_f64().max(1e-6),
        bytes_out as f64 / 1e6,
        run.adapter,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cutty_engine::{Engine, ProjectSettings, TrackKind};

    fn spec(w: u32, h: u32, fps: f64) -> ExportSpec {
        ExportSpec {
            width: w,
            height: h,
            fps,
            quality: ExportQuality::Medium,
            dst: PathBuf::from("/tmp/out.mp4"),
        }
    }

    /// Engine with one 10s A/V media and the given video clips
    /// (timeline_in, source_in, source_out).
    fn project_with(clips: &[(f64, f64, f64)]) -> Project {
        let mut engine = Engine::new(ProjectSettings::default());
        let media = engine.add_media("/tmp/a.mp4", 10.0, true, true).unwrap();
        let video = engine
            .project()
            .tracks
            .iter()
            .find(|t| t.kind == TrackKind::Video)
            .unwrap()
            .id;
        for &(t_in, s_in, s_out) in clips {
            engine.add_clip(video, media, t_in, s_in, s_out).unwrap();
        }
        engine.project().clone()
    }

    #[test]
    fn plans_clips_gaps_and_frame_grid_exactly() {
        // [1.0, 2.5) and [4.0, 6.0): leading gap, middle gap, no tail.
        let p = project_with(&[(1.0, 2.0, 3.5), (4.0, 0.0, 2.0)]);
        let segs = plan_video_segments(&p, 30.0);
        assert_eq!(segs.len(), 4);

        assert_eq!(segs[0].source, SegmentSource::Black);
        assert_eq!((segs[0].start_frame, segs[0].end_frame), (0, 30));
        match &segs[1].source {
            SegmentSource::Clip {
                source_in, speed, ..
            } => {
                assert!((source_in - 2.0).abs() < 1e-9);
                assert_eq!(*speed, 1.0);
            }
            other => panic!("expected clip, got {other:?}"),
        }
        assert_eq!((segs[1].start_frame, segs[1].end_frame), (30, 75));
        assert_eq!(segs[2].source, SegmentSource::Black);
        assert_eq!((segs[2].start_frame, segs[2].end_frame), (75, 120));
        assert_eq!((segs[3].start_frame, segs[3].end_frame), (120, 180));

        // Segments tile the whole timeline with no gaps or overlaps.
        for pair in segs.windows(2) {
            assert_eq!(pair[0].end_frame, pair[1].start_frame);
        }
    }

    #[test]
    fn plans_black_tail_under_longer_audio() {
        let mut engine = Engine::new(ProjectSettings::default());
        let av = engine.add_media("/tmp/a.mp4", 10.0, true, true).unwrap();
        let music = engine.add_media("/tmp/m.mp3", 30.0, false, true).unwrap();
        let (mut video, mut audio) = (None, None);
        for t in &engine.project().tracks {
            match t.kind {
                TrackKind::Video => video = Some(t.id),
                TrackKind::Audio => audio = Some(t.id),
                TrackKind::Text => {}
            }
        }
        engine.add_clip(video.unwrap(), av, 0.0, 0.0, 2.0).unwrap();
        engine
            .add_clip(audio.unwrap(), music, 0.0, 0.0, 5.0)
            .unwrap();
        let segs = plan_video_segments(engine.project(), 30.0);

        assert_eq!(segs.len(), 2);
        assert!(matches!(segs[0].source, SegmentSource::Clip { .. }));
        assert_eq!((segs[0].start_frame, segs[0].end_frame), (0, 60));
        assert_eq!(segs[1].source, SegmentSource::Black);
        assert_eq!(
            (segs[1].start_frame, segs[1].end_frame),
            (60, 150),
            "black under the music tail"
        );
    }

    #[test]
    fn plan_snaps_off_grid_cuts_without_drift() {
        // Cuts at 1/3-second positions: every boundary lands on one frame
        // index, and counts still sum to the total.
        let p = project_with(&[(0.0, 0.0, 1.0 / 3.0), (1.0 / 3.0, 5.0, 5.0 + 2.0 / 3.0)]);
        let segs = plan_video_segments(&p, 30.0);
        assert_eq!(segs.len(), 2);
        assert_eq!((segs[0].start_frame, segs[0].end_frame), (0, 10));
        assert_eq!((segs[1].start_frame, segs[1].end_frame), (10, 30));
        match &segs[1].source {
            SegmentSource::Clip { source_in, .. } => {
                // Grid start 10/30 == timeline_in exactly here.
                assert!((source_in - 5.0).abs() < 1e-9);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn plan_of_empty_project_is_empty() {
        let p = project_with(&[]);
        assert!(plan_video_segments(&p, 30.0).is_empty());
    }

    #[test]
    fn hidden_video_track_plans_black() {
        let mut p = project_with(&[(0.0, 0.0, 2.0)]);
        for t in &mut p.tracks {
            if t.kind == TrackKind::Video {
                t.hidden = true;
            }
        }
        let segs = plan_video_segments(&p, 30.0);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].source, SegmentSource::Black);
        assert_eq!(segs[0].frames(), 60);

        // Muting, by contrast, silences audio only — the picture plans
        // as usual.
        let mut p = project_with(&[(0.0, 0.0, 2.0)]);
        for t in &mut p.tracks {
            t.muted = true;
        }
        let segs = plan_video_segments(&p, 30.0);
        assert_eq!(segs.len(), 1);
        assert!(matches!(segs[0].source, SegmentSource::Clip { .. }));
    }

    #[test]
    fn clip_filter_shape() {
        let s = spec(1080, 1920, 30.0);
        let f = clip_filter(&s, 1.0, false);
        assert!(f.starts_with("setpts=PTS-STARTPTS,fps=30,"));
        assert!(f.contains("scale=1080:1920:force_original_aspect_ratio=decrease"));
        assert!(f.contains("out_color_matrix=bt709"));
        assert!(f.contains("pad=1080:1920:(ow-iw)/2:(oh-ih)/2:color=black"));
        assert!(f.contains("setsar=1"));
        assert!(f.ends_with("tpad=stop_mode=clone:stop=-1"));

        let hw = clip_filter(&s, 1.0, true);
        assert!(hw.ends_with("format=nv12,hwupload"));

        let fast = clip_filter(&s, 2.0, false);
        assert!(fast.starts_with("setpts=(PTS-STARTPTS)/2,"));
    }

    #[test]
    fn segment_args_shape_clip_and_black() {
        let s = spec(1920, 1080, 30.0);
        let seg = PlannedSegment {
            source: SegmentSource::Clip {
                path: PathBuf::from("/media/in.mkv"),
                source_in: 12.25,
                speed: 1.0,
            },
            start_frame: 0,
            end_frame: 90,
        };
        let args = segment_args(&s, &H264Encoder::X264, &seg, Path::new("/t/seg.mp4"));
        let joined = args.join(" ");
        assert!(joined.starts_with("-ss 12.250000 -t 3.066667 -i /media/in.mkv"));
        assert!(joined.contains("-frames:v 90"));
        assert!(joined.contains("-c:v libx264 -preset medium -crf 23"));
        assert!(joined.contains("-pix_fmt yuv420p"));
        assert!(joined.contains("-g 60"));
        assert!(joined.contains("-colorspace bt709"));
        assert!(joined.contains("-an"));
        assert!(joined.ends_with("-y /t/seg.mp4"));

        let black = PlannedSegment {
            source: SegmentSource::Black,
            start_frame: 90,
            end_frame: 120,
        };
        let args = segment_args(&s, &H264Encoder::X264, &black, Path::new("/t/b.mp4"));
        let joined = args.join(" ");
        assert!(joined.contains("-f lavfi -i color=black:size=1920x1080:rate=30"));
        assert!(joined.contains("-frames:v 30"));
    }

    #[test]
    fn segment_args_vaapi_uses_device_and_hwupload() {
        let s = spec(1920, 1080, 30.0);
        let enc = H264Encoder::Vaapi {
            device: PathBuf::from("/dev/dri/renderD128"),
        };
        let seg = PlannedSegment {
            source: SegmentSource::Clip {
                path: PathBuf::from("/a.mp4"),
                source_in: 0.0,
                speed: 1.0,
            },
            start_frame: 0,
            end_frame: 30,
        };
        let joined = segment_args(&s, &enc, &seg, Path::new("/t/s.mp4")).join(" ");
        assert!(joined.starts_with("-vaapi_device /dev/dri/renderD128 "));
        assert!(joined.contains("hwupload"));
        assert!(joined.contains("-c:v h264_vaapi -rc_mode CQP -qp 24"));
        assert!(!joined.contains("-pix_fmt"), "vaapi formats via hwupload");
    }

    #[test]
    fn fast_path_requires_single_default_track() {
        // Plain single track: eligible.
        let p = project_with(&[(0.0, 0.0, 2.0), (2.0, 3.0, 5.0)]);
        assert!(fast_path_eligible(&p));

        // Any non-default visual parameter disqualifies.
        let mut transformed = p.clone();
        transformed.tracks[0].clips[0].transform.scale = 0.5;
        assert!(!fast_path_eligible(&transformed));
        let mut faded = p.clone();
        faded.tracks[0].clips[1].opacity = 0.9;
        assert!(!fast_path_eligible(&faded));
        let mut blended = p.clone();
        blended.tracks[0].clips[0].blend_mode = BlendMode::Screen;
        assert!(!fast_path_eligible(&blended));

        // A second video track with clips disqualifies…
        let mut multi = p.clone();
        let mut track = multi.tracks[0].clone();
        track.id = cutty_engine::TrackId(999);
        track.clips = vec![multi.tracks[0].clips[0].clone()];
        track.clips[0].id = cutty_engine::ClipId(998);
        multi.tracks.insert(0, track);
        assert!(!fast_path_eligible(&multi));
        // …unless it is hidden (⇒ nothing to composite). Muted alone
        // doesn't help — a muted track still shows its picture.
        multi.tracks[0].muted = true;
        assert!(!fast_path_eligible(&multi));
        multi.tracks[0].hidden = true;
        assert!(fast_path_eligible(&multi));

        // No video clips at all (audio-only timeline): eligible.
        let mut empty = p.clone();
        empty.tracks[0].clips.clear();
        assert!(fast_path_eligible(&empty));
    }

    #[test]
    fn raw_export_args_shape() {
        let s = spec(1920, 1080, 30.0);
        let joined = raw_export_args(
            &s,
            &H264Encoder::X264,
            Path::new("/t/mix.wav"),
            Path::new("/out/final.mp4.part"),
        )
        .join(" ");
        assert!(joined.starts_with(
            "-f rawvideo -pix_fmt rgba -video_size 1920x1080 -framerate 30 -i pipe:0"
        ));
        assert!(joined.contains("-i /t/mix.wav"));
        assert!(joined.contains("-map 0:v:0 -map 1:a:0"));
        assert!(joined.contains("-vf scale=out_color_matrix=bt709:out_range=tv,format=yuv420p"));
        assert!(joined.contains("-c:v libx264"));
        assert!(joined.contains("-colorspace bt709"));
        assert!(joined.contains("-c:a aac -b:a 160k"));
        assert!(joined.contains("-movflags +faststart"));
        assert!(joined.ends_with("-f mp4 -y /out/final.mp4.part"));
        assert!(!joined.contains("-an"), "audio is muxed in the same pass");

        let vaapi = H264Encoder::Vaapi {
            device: PathBuf::from("/dev/dri/renderD128"),
        };
        let joined = raw_export_args(
            &s,
            &vaapi,
            Path::new("/t/mix.wav"),
            Path::new("/out/final.mp4.part"),
        )
        .join(" ");
        assert!(joined.starts_with("-vaapi_device /dev/dri/renderD128 "));
        assert!(
            joined.contains("-vf scale=out_color_matrix=bt709:out_range=tv,format=nv12,hwupload")
        );
        assert!(joined.contains("-c:v h264_vaapi"));
    }

    #[test]
    fn mux_args_shape() {
        let s = spec(1920, 1080, 30.0);
        let joined = mux_args(
            &s,
            Path::new("/t/concat.txt"),
            Path::new("/t/mix.wav"),
            Path::new("/out/final.mp4.part"),
        )
        .join(" ");
        assert!(joined.starts_with("-f concat -safe 0 -i /t/concat.txt -i /t/mix.wav"));
        assert!(joined.contains("-map 0:v:0 -map 1:a:0"));
        assert!(joined.contains("-c:v copy"));
        assert!(joined.contains("-c:a aac -b:a 160k"));
        assert!(joined.contains("-movflags +faststart"));
        assert!(joined.contains("-f mp4"));
        assert!(joined.ends_with("-y /out/final.mp4.part"));
    }

    #[test]
    fn concat_list_escapes_quotes() {
        let body = concat_list(&[
            PathBuf::from("/t/seg-0000.mp4"),
            PathBuf::from("/we'ird/seg.mp4"),
        ]);
        assert_eq!(body, "file '/t/seg-0000.mp4'\nfile '/we'\\''ird/seg.mp4'\n");
    }

    #[test]
    fn quality_knobs_are_monotonic() {
        for enc in [
            H264Encoder::X264,
            H264Encoder::Nvenc,
            H264Encoder::Vaapi {
                device: PathBuf::from("/dev/dri/renderD128"),
            },
        ] {
            let hi = ExportQuality::High.video_q(&enc);
            let med = ExportQuality::Medium.video_q(&enc);
            let small = ExportQuality::Small.video_q(&enc);
            assert!(hi < med && med < small, "{enc:?}: {hi} {med} {small}");
        }
    }

    #[test]
    fn export_rejects_empty_timeline_and_bad_specs() {
        let p = project_with(&[]);
        let cancel = CancelToken::new();
        let err = run_export(&p, &spec(1920, 1080, 30.0), &cancel, &mut |_| {}).unwrap_err();
        assert!(matches!(err, MediaError::ExportNotReady { .. }), "{err}");

        let p = project_with(&[(0.0, 0.0, 1.0)]);
        for bad in [
            spec(1921, 1080, 30.0),
            spec(0, 1080, 30.0),
            spec(1920, 1080, 0.0),
        ] {
            let err = run_export(&p, &bad, &cancel, &mut |_| {}).unwrap_err();
            assert!(matches!(err, MediaError::ExportNotReady { .. }), "{err}");
        }
    }

    #[test]
    fn export_rejects_missing_source_files() {
        // /tmp/a.mp4 does not exist; the clip references it.
        let p = project_with(&[(0.0, 0.0, 1.0)]);
        let cancel = CancelToken::new();
        let err = run_export(&p, &spec(1920, 1080, 30.0), &cancel, &mut |_| {}).unwrap_err();
        assert!(
            matches!(err, MediaError::ExportNotReady { ref message } if message.contains("missing")),
            "{err}"
        );
    }
}
