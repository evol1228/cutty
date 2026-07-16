//! Probing media files with ffprobe.

use std::path::Path;
use std::process::Command;

use cutty_engine::MediaKind;
use ffmpeg_sidecar::ffprobe::ffprobe_path;
use serde::{Deserialize, Serialize};

use crate::error::MediaError;
use crate::tools::ensure_tools;

/// A GIF at or under this duration is a single frame in a trenchcoat —
/// imported as a still, not a loop (a loop period needs real length).
const MIN_GIF_LOOP_SEC: f64 = 0.11;

/// Everything Cutty needs to know about a media file up front.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaInfo {
    /// Absolute path of the probed file.
    pub path: String,
    /// Container duration in seconds (`0` for still images).
    pub duration_sec: f64,
    /// Container format name as reported by ffprobe (e.g. `mov,mp4,m4a,...`).
    pub container: String,
    /// File size in bytes.
    pub size_bytes: u64,
    /// What the timeline should treat this file as: bounded video/audio,
    /// a still image, or a looping GIF.
    pub kind: MediaKind,
    /// First video stream, if any.
    pub video: Option<VideoStreamInfo>,
    /// First audio stream, if any.
    pub audio: Option<AudioStreamInfo>,
    /// One-line summary of every stream in the file.
    pub streams: Vec<StreamSummary>,
}

/// Properties of a video stream.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoStreamInfo {
    pub codec: String,
    /// Stored dimensions. When `rotation` is ±90/±270 the *display*
    /// dimensions are swapped (ffmpeg applies the rotation when
    /// transcoding, so proxies come out already rotated).
    pub width: u32,
    pub height: u32,
    /// Average frame rate. Falls back to `r_frame_rate` when the average is
    /// unknown (e.g. some fragmented MP4s).
    pub fps: f64,
    /// Display-matrix rotation in degrees (0 when absent) — phone footage
    /// is commonly stored landscape with a ±90 rotation tag.
    pub rotation: i32,
    /// The stream carries an alpha channel: an alpha-capable pixel format
    /// (PNG `rgba`, GIF `bgra`/`pal8`) or WebM's `alpha_mode` stream tag
    /// (VP8/VP9 alpha rides a side stream the pix_fmt doesn't show).
    pub has_alpha: bool,
}

/// Properties of an audio stream.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioStreamInfo {
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u32,
}

/// Type and codec of a single stream, for display in the UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamSummary {
    pub index: u32,
    pub kind: String,
    pub codec: String,
}

/// Probe a media file with ffprobe and return its properties.
pub fn probe(path: &Path) -> Result<MediaInfo, MediaError> {
    ensure_tools()?;

    let output = Command::new(ffprobe_path())
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        .output()
        .map_err(|source| MediaError::Spawn {
            tool: "ffprobe",
            source,
        })?;

    if !output.status.success() {
        return Err(MediaError::ProbeFailed {
            path: path.display().to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }

    parse_probe_output(&String::from_utf8_lossy(&output.stdout), path)
}

// --- ffprobe JSON shape ---

#[derive(Debug, Deserialize)]
struct RawProbe {
    #[serde(default)]
    streams: Vec<RawStream>,
    format: RawFormat,
}

#[derive(Debug, Deserialize)]
struct RawFormat {
    #[serde(default)]
    format_name: String,
    duration: Option<String>,
    size: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawStream {
    index: u32,
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    pix_fmt: Option<String>,
    avg_frame_rate: Option<String>,
    r_frame_rate: Option<String>,
    sample_rate: Option<String>,
    channels: Option<u32>,
    duration: Option<String>,
    #[serde(default)]
    disposition: RawDisposition,
    #[serde(default)]
    side_data_list: Vec<RawSideData>,
    #[serde(default)]
    tags: RawStreamTags,
}

#[derive(Debug, Default, Deserialize)]
struct RawStreamTags {
    /// `"1"` on WebM VP8/VP9 streams whose alpha rides a side stream.
    alpha_mode: Option<String>,
}

/// Pixel formats whose RGBA conversion carries real alpha (matches the
/// decoder's `pixel_has_alpha`; `pal8` counts for GIF transparency).
fn pix_fmt_has_alpha(pix_fmt: &str) -> bool {
    pix_fmt.starts_with("yuva")
        || pix_fmt.starts_with("rgba")
        || pix_fmt.starts_with("bgra")
        || pix_fmt.starts_with("argb")
        || pix_fmt.starts_with("abgr")
        || pix_fmt.starts_with("gbrap")
        || pix_fmt.starts_with("ya")
        || pix_fmt == "pal8"
}

/// Containers that hold a single still image.
fn is_image_container(container: &str) -> bool {
    matches!(
        container,
        "image2" | "png_pipe" | "jpeg_pipe" | "webp_pipe" | "bmp_pipe" | "tiff_pipe"
    )
}

#[derive(Debug, Default, Deserialize)]
struct RawDisposition {
    /// 1 for cover-art streams (MP3/M4A album art) — decodable as video
    /// but not *the* video stream.
    #[serde(default)]
    attached_pic: u8,
}

#[derive(Debug, Deserialize)]
struct RawSideData {
    /// Present for Display Matrix entries; degrees, often negative.
    rotation: Option<f64>,
}

/// Parse ffprobe `-print_format json` output into a [`MediaInfo`].
///
/// Split out from [`probe`] so the parsing logic is testable without ffprobe.
fn parse_probe_output(json: &str, path: &Path) -> Result<MediaInfo, MediaError> {
    let raw: RawProbe = serde_json::from_str(json)?;

    let mut video = None;
    let mut audio = None;
    let mut streams = Vec::with_capacity(raw.streams.len());
    // Streams can also carry duration (needed when the container omits it).
    let mut stream_duration: f64 = 0.0;

    for s in &raw.streams {
        let mut kind = s.codec_type.clone().unwrap_or_else(|| "unknown".into());
        let codec = s.codec_name.clone().unwrap_or_else(|| "unknown".into());
        // Cover art (MP3/M4A album art) probes as a video stream but must
        // never be selected as one.
        if kind == "video" && s.disposition.attached_pic != 0 {
            kind = "attached_pic".into();
        }
        streams.push(StreamSummary {
            index: s.index,
            kind: kind.clone(),
            codec: codec.clone(),
        });

        if let Some(d) = s.duration.as_deref().and_then(|d| d.parse::<f64>().ok()) {
            stream_duration = stream_duration.max(d);
        }

        match kind.as_str() {
            "video" if video.is_none() => {
                let fps = s
                    .avg_frame_rate
                    .as_deref()
                    .and_then(parse_rate)
                    .or_else(|| s.r_frame_rate.as_deref().and_then(parse_rate));
                let rotation = s
                    .side_data_list
                    .iter()
                    .find_map(|d| d.rotation)
                    .map(|r| r.round() as i32)
                    .unwrap_or(0);
                let has_alpha = s.pix_fmt.as_deref().is_some_and(pix_fmt_has_alpha)
                    || s.tags.alpha_mode.as_deref() == Some("1");
                if let (Some(width), Some(height), Some(fps)) = (s.width, s.height, fps) {
                    video = Some(VideoStreamInfo {
                        codec,
                        width,
                        height,
                        fps,
                        rotation,
                        has_alpha,
                    });
                }
            }
            "audio" if audio.is_none() => {
                let sample_rate = s
                    .sample_rate
                    .as_deref()
                    .and_then(|r| r.parse::<u32>().ok())
                    .unwrap_or(0);
                audio = Some(AudioStreamInfo {
                    codec,
                    sample_rate,
                    channels: s.channels.unwrap_or(0),
                });
            }
            _ => {}
        }
    }

    if video.is_none() && audio.is_none() {
        return Err(MediaError::NoStreams {
            path: path.display().to_string(),
        });
    }

    let duration_sec = raw
        .format
        .duration
        .as_deref()
        .and_then(|d| d.parse::<f64>().ok())
        .unwrap_or(stream_duration);

    // Classify the file for the timeline model. Still-image containers
    // (and degenerate single-frame GIFs) become stills with no intrinsic
    // duration; real GIFs become loops; everything else is bounded
    // video/audio by stream presence.
    let container = raw.format.format_name;
    let (kind, duration_sec) = if video.is_some() && is_image_container(&container) {
        (MediaKind::Image, 0.0)
    } else if video.is_some() && container == "gif" {
        if duration_sec > MIN_GIF_LOOP_SEC {
            (MediaKind::Gif, duration_sec)
        } else {
            (MediaKind::Image, 0.0)
        }
    } else if video.is_some() {
        (MediaKind::Video, duration_sec)
    } else {
        (MediaKind::Audio, duration_sec)
    };

    Ok(MediaInfo {
        path: path.display().to_string(),
        duration_sec,
        container,
        size_bytes: raw
            .format
            .size
            .as_deref()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0),
        kind,
        video,
        audio,
        streams,
    })
}

/// Parse an ffprobe rational rate like `"30000/1001"` or `"30/1"` into f64.
///
/// Returns `None` for missing/zero denominators or the `"0/0"` placeholder.
fn parse_rate(rate: &str) -> Option<f64> {
    let (num, den) = rate.split_once('/')?;
    let num: f64 = num.parse().ok()?;
    let den: f64 = den.parse().ok()?;
    if den == 0.0 || num == 0.0 {
        return None;
    }
    Some(num / den)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_4K: &str = r#"{
        "streams": [
            {
                "index": 0,
                "codec_name": "h264",
                "codec_type": "video",
                "width": 3840,
                "height": 2160,
                "r_frame_rate": "30/1",
                "avg_frame_rate": "30000/1001",
                "duration": "90.023267"
            },
            {
                "index": 1,
                "codec_name": "aac",
                "codec_type": "audio",
                "sample_rate": "48000",
                "channels": 2,
                "r_frame_rate": "0/0",
                "duration": "90.000000"
            }
        ],
        "format": {
            "format_name": "mov,mp4,m4a,3gp,3g2,mj2",
            "duration": "90.046000",
            "size": "231072941"
        }
    }"#;

    #[test]
    fn parses_typical_4k_probe() {
        let info = parse_probe_output(SAMPLE_4K, Path::new("/tmp/clip.mp4")).unwrap();
        assert_eq!(info.duration_sec, 90.046);
        assert_eq!(info.size_bytes, 231_072_941);
        assert_eq!(info.streams.len(), 2);
        assert_eq!(info.kind, MediaKind::Video);

        let v = info.video.expect("video stream");
        assert_eq!((v.width, v.height), (3840, 2160));
        assert_eq!(v.codec, "h264");
        assert!((v.fps - 29.97).abs() < 0.01);

        let a = info.audio.expect("audio stream");
        assert_eq!(a.codec, "aac");
        assert_eq!(a.sample_rate, 48000);
        assert_eq!(a.channels, 2);
    }

    #[test]
    fn falls_back_to_r_frame_rate_and_stream_duration() {
        let json = r#"{
            "streams": [
                {
                    "index": 0,
                    "codec_name": "vp9",
                    "codec_type": "video",
                    "width": 1920,
                    "height": 1080,
                    "r_frame_rate": "60/1",
                    "avg_frame_rate": "0/0",
                    "duration": "12.5"
                }
            ],
            "format": { "format_name": "webm" }
        }"#;
        let info = parse_probe_output(json, Path::new("x.webm")).unwrap();
        let v = info.video.unwrap();
        assert_eq!(v.fps, 60.0);
        assert_eq!(info.duration_sec, 12.5);
        assert!(info.audio.is_none());
    }

    #[test]
    fn audio_only_file_is_valid() {
        let json = r#"{
            "streams": [
                {
                    "index": 0,
                    "codec_name": "mp3",
                    "codec_type": "audio",
                    "sample_rate": "44100",
                    "channels": 2
                }
            ],
            "format": { "format_name": "mp3", "duration": "180.1" }
        }"#;
        let info = parse_probe_output(json, Path::new("song.mp3")).unwrap();
        assert!(info.video.is_none());
        assert_eq!(info.audio.unwrap().sample_rate, 44100);
    }

    #[test]
    fn cover_art_is_not_the_video_stream() {
        // An MP3 with embedded album art: ffprobe reports the art as a
        // video stream with attached_pic disposition.
        let json = r#"{
            "streams": [
                {
                    "index": 0,
                    "codec_name": "mp3",
                    "codec_type": "audio",
                    "sample_rate": "44100",
                    "channels": 2
                },
                {
                    "index": 1,
                    "codec_name": "mjpeg",
                    "codec_type": "video",
                    "width": 600,
                    "height": 600,
                    "r_frame_rate": "90000/1",
                    "avg_frame_rate": "0/0",
                    "disposition": { "attached_pic": 1 }
                }
            ],
            "format": { "format_name": "mp3", "duration": "180.0" }
        }"#;
        let info = parse_probe_output(json, Path::new("song.mp3")).unwrap();
        assert!(info.video.is_none(), "cover art must not become video");
        assert!(info.audio.is_some());
        assert_eq!(info.streams[1].kind, "attached_pic");
    }

    #[test]
    fn display_matrix_rotation_is_reported() {
        let json = r#"{
            "streams": [
                {
                    "index": 0,
                    "codec_name": "h264",
                    "codec_type": "video",
                    "width": 1920,
                    "height": 1080,
                    "avg_frame_rate": "30/1",
                    "side_data_list": [
                        { "side_data_type": "Display Matrix", "rotation": -90 }
                    ]
                }
            ],
            "format": { "format_name": "mp4", "duration": "10.0" }
        }"#;
        let info = parse_probe_output(json, Path::new("phone.mp4")).unwrap();
        let v = info.video.unwrap();
        assert_eq!(v.rotation, -90);
        assert_eq!((v.width, v.height), (1920, 1080));
    }

    #[test]
    fn classifies_stills_gifs_and_alpha() {
        // PNG: png_pipe container, no duration → a still with duration 0.
        let png = r#"{
            "streams": [
                {
                    "index": 0, "codec_name": "png", "codec_type": "video",
                    "width": 800, "height": 600, "pix_fmt": "rgba",
                    "r_frame_rate": "25/1", "avg_frame_rate": "25/1"
                }
            ],
            "format": { "format_name": "png_pipe", "size": "1000" }
        }"#;
        let info = parse_probe_output(png, Path::new("poster.png")).unwrap();
        assert_eq!(info.kind, MediaKind::Image);
        assert_eq!(info.duration_sec, 0.0);
        assert!(info.video.as_ref().unwrap().has_alpha, "rgba png has alpha");

        // Opaque JPEG: image2 container, no alpha.
        let jpg = r#"{
            "streams": [
                {
                    "index": 0, "codec_name": "mjpeg", "codec_type": "video",
                    "width": 800, "height": 600, "pix_fmt": "yuvj420p",
                    "r_frame_rate": "25/1", "avg_frame_rate": "25/1"
                }
            ],
            "format": { "format_name": "image2", "duration": "0.040000" }
        }"#;
        let info = parse_probe_output(jpg, Path::new("photo.jpg")).unwrap();
        assert_eq!(info.kind, MediaKind::Image);
        assert_eq!(info.duration_sec, 0.0);
        assert!(!info.video.as_ref().unwrap().has_alpha);

        // Animated GIF: a loop with its real duration.
        let gif = r#"{
            "streams": [
                {
                    "index": 0, "codec_name": "gif", "codec_type": "video",
                    "width": 320, "height": 240, "pix_fmt": "bgra",
                    "r_frame_rate": "12/1", "avg_frame_rate": "25/2",
                    "duration": "2.000000"
                }
            ],
            "format": { "format_name": "gif", "duration": "2.000000" }
        }"#;
        let info = parse_probe_output(gif, Path::new("sticker.gif")).unwrap();
        assert_eq!(info.kind, MediaKind::Gif);
        assert_eq!(info.duration_sec, 2.0);
        assert!(info.video.as_ref().unwrap().has_alpha, "bgra gif has alpha");

        // Single-frame GIF: no loop period — imported as a still.
        let single = r#"{
            "streams": [
                {
                    "index": 0, "codec_name": "gif", "codec_type": "video",
                    "width": 320, "height": 240, "pix_fmt": "bgra",
                    "r_frame_rate": "100/1", "avg_frame_rate": "100/1",
                    "duration": "0.010000"
                }
            ],
            "format": { "format_name": "gif", "duration": "0.010000" }
        }"#;
        let info = parse_probe_output(single, Path::new("frame.gif")).unwrap();
        assert_eq!(info.kind, MediaKind::Image);
        assert_eq!(info.duration_sec, 0.0);

        // WebM VP9 alpha: pix_fmt lies (yuv420p) but alpha_mode tells.
        let webm = r#"{
            "streams": [
                {
                    "index": 0, "codec_name": "vp9", "codec_type": "video",
                    "width": 480, "height": 480, "pix_fmt": "yuv420p",
                    "r_frame_rate": "30/1", "avg_frame_rate": "30/1",
                    "tags": { "alpha_mode": "1" }
                }
            ],
            "format": { "format_name": "matroska,webm", "duration": "4.0" }
        }"#;
        let info = parse_probe_output(webm, Path::new("overlay.webm")).unwrap();
        assert_eq!(info.kind, MediaKind::Video);
        assert!(info.video.as_ref().unwrap().has_alpha, "alpha_mode counts");
        assert_eq!(info.duration_sec, 4.0);
    }

    #[test]
    fn no_streams_is_an_error() {
        let json = r#"{ "streams": [], "format": { "format_name": "mp4" } }"#;
        let err = parse_probe_output(json, Path::new("empty.mp4")).unwrap_err();
        assert!(matches!(err, MediaError::NoStreams { .. }));
    }

    #[test]
    fn garbage_json_is_a_parse_error() {
        let err = parse_probe_output("not json", Path::new("x")).unwrap_err();
        assert!(matches!(err, MediaError::ProbeParse(_)));
    }

    #[test]
    fn parse_rate_handles_edge_cases() {
        assert_eq!(parse_rate("30/1"), Some(30.0));
        assert!((parse_rate("30000/1001").unwrap() - 29.97).abs() < 0.01);
        assert_eq!(parse_rate("0/0"), None);
        assert_eq!(parse_rate("5/0"), None);
        assert_eq!(parse_rate("garbage"), None);
        assert_eq!(parse_rate(""), None);
    }

    /// End-to-end test against the real system ffprobe: generates a tiny clip
    /// with lavfi and probes it. Requires ffmpeg on PATH (guaranteed on the
    /// dev machine per CLAUDE.md).
    #[test]
    fn probes_a_real_generated_clip() {
        use ffmpeg_sidecar::paths::ffmpeg_path;

        let dir = std::env::temp_dir().join("cutty-media-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("probe-test.mp4");

        let status = Command::new(ffmpeg_path())
            .args(["-y", "-f", "lavfi", "-i"])
            .arg("testsrc2=size=320x180:rate=30:duration=1")
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=1"])
            .args(["-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac"])
            .arg(&file)
            .status()
            .expect("system ffmpeg must be installed for tests");
        assert!(status.success(), "test clip generation failed");

        let info = probe(&file).unwrap();
        assert!(
            (info.duration_sec - 1.0).abs() < 0.3,
            "{}",
            info.duration_sec
        );
        let v = info.video.expect("video");
        assert_eq!((v.width, v.height), (320, 180));
        assert!((v.fps - 30.0).abs() < 0.5);
        let a = info.audio.expect("audio");
        assert!(a.sample_rate > 0);
    }
}
