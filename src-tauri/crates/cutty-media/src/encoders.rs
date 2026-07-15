//! H.264 encoder detection: prefer hardware (VAAPI, then NVENC), fall
//! back to libx264 silently.
//!
//! `ffmpeg -encoders` only says an encoder was *compiled in* — a machine
//! can list `h264_nvenc` with no NVIDIA GPU, or `h264_vaapi` with a driver
//! that can't encode. So detection is functional: each candidate must
//! actually encode a few frames to the null muxer before it is chosen.
//! The result is cached for the process lifetime; call
//! [`start_encoder_detection`] at app startup so the export dialog opens
//! with the answer already in hand.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use ffmpeg_sidecar::paths::ffmpeg_path;

/// The encoder an export will use, with everything needed to build its
/// ffmpeg arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum H264Encoder {
    /// `h264_vaapi` through the given DRM render node (Intel/AMD).
    Vaapi { device: PathBuf },
    /// `h264_nvenc` (NVIDIA).
    Nvenc,
    /// `libx264` software fallback — always assumed present.
    X264,
}

impl H264Encoder {
    /// The ffmpeg encoder name (`-c:v` value).
    pub fn ffmpeg_name(&self) -> &'static str {
        match self {
            H264Encoder::Vaapi { .. } => "h264_vaapi",
            H264Encoder::Nvenc => "h264_nvenc",
            H264Encoder::X264 => "libx264",
        }
    }

    /// Human-readable label for the export dialog.
    pub fn label(&self) -> String {
        match self {
            H264Encoder::Vaapi { device } => {
                format!("VAAPI hardware (h264_vaapi on {})", device.display())
            }
            H264Encoder::Nvenc => "NVIDIA hardware (h264_nvenc)".to_string(),
            H264Encoder::X264 => "software (libx264)".to_string(),
        }
    }

    pub fn is_hardware(&self) -> bool {
        !matches!(self, H264Encoder::X264)
    }
}

/// Parse `ffmpeg -encoders` output into the set of encoder names.
///
/// Lines look like ` V....D h264_vaapi           H.264/AVC (VAAPI)`; the
/// first column is a capability flag block, the second the name.
fn parse_encoder_list(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let flags = cols.next()?;
            let name = cols.next()?;
            // Encoder rows start with a V/A/S capability block. The
            // legend lines (` V..... = Video`) share that shape, so the
            // name must also look like an encoder identifier.
            let is_row = flags.starts_with('V') || flags.starts_with('A') || flags.starts_with('S');
            let is_name = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            (is_row && is_name).then(|| name.to_string())
        })
        .collect()
}

fn list_encoders() -> Vec<String> {
    let output = Command::new(ffmpeg_path())
        .args(["-hide_banner", "-encoders"])
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(out) if out.status.success() => {
            parse_encoder_list(&String::from_utf8_lossy(&out.stdout))
        }
        _ => Vec::new(),
    }
}

/// Functionally probe one candidate: encode 3 black frames to the null
/// muxer. Exit status 0 means the encoder (and its device) really works.
fn probe_encoder(candidate: &H264Encoder) -> bool {
    let mut cmd = Command::new(ffmpeg_path());
    cmd.args(["-hide_banner", "-v", "error"]);
    match candidate {
        H264Encoder::Vaapi { device } => {
            cmd.arg("-vaapi_device").arg(device);
            cmd.args(["-f", "lavfi", "-i", "color=black:size=256x256:rate=30"]);
            cmd.args(["-frames:v", "3", "-vf", "format=nv12,hwupload"]);
        }
        H264Encoder::Nvenc | H264Encoder::X264 => {
            cmd.args(["-f", "lavfi", "-i", "color=black:size=256x256:rate=30"]);
            cmd.args(["-frames:v", "3", "-pix_fmt", "yuv420p"]);
        }
    }
    cmd.args(["-c:v", candidate.ffmpeg_name(), "-f", "null", "-"]);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

/// DRM render nodes to try for VAAPI, in order.
fn vaapi_devices() -> Vec<PathBuf> {
    let mut nodes: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("renderD"))
        })
        .collect();
    nodes.sort();
    nodes
}

fn detect() -> H264Encoder {
    let available = list_encoders();
    let has = |name: &str| available.iter().any(|e| e == name);

    if has("h264_vaapi") {
        for device in vaapi_devices() {
            let candidate = H264Encoder::Vaapi { device };
            if probe_encoder(&candidate) {
                eprintln!("cutty-media: export encoder: {}", candidate.label());
                return candidate;
            }
            eprintln!(
                "cutty-media: {} listed but failed the functional probe on {}",
                candidate.ffmpeg_name(),
                match &candidate {
                    H264Encoder::Vaapi { device } => device.display().to_string(),
                    _ => unreachable!(),
                }
            );
        }
    }
    if has("h264_nvenc") && probe_encoder(&H264Encoder::Nvenc) {
        eprintln!(
            "cutty-media: export encoder: {}",
            H264Encoder::Nvenc.label()
        );
        return H264Encoder::Nvenc;
    }
    // Silent fallback per the plan: libx264 always works where ffmpeg does.
    eprintln!("cutty-media: export encoder: {}", H264Encoder::X264.label());
    H264Encoder::X264
}

static DETECTED: OnceLock<H264Encoder> = OnceLock::new();

/// The H.264 encoder exports will use. First call runs the (blocking,
/// few-hundred-ms) functional detection; afterwards it's cached.
pub fn detected_h264_encoder() -> &'static H264Encoder {
    DETECTED.get_or_init(detect)
}

/// Warm the encoder-detection cache on a background thread (call at app
/// startup so opening the export dialog never blocks on ffmpeg probes).
pub fn start_encoder_detection() {
    std::thread::Builder::new()
        .name("cutty-encoder-detect".into())
        .spawn(|| {
            let _ = detected_h264_encoder();
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_encoder_listing() {
        let listing = "\
Encoders:
 V..... = Video
 A..... = Audio
 ------
 V....D libx264              libx264 H.264 / AVC (codec h264)
 V....D h264_nvenc           NVIDIA NVENC H.264 encoder (codec h264)
 V....D h264_vaapi           H.264/AVC (VAAPI) (codec h264)
 A....D aac                  AAC (Advanced Audio Coding)
";
        let names = parse_encoder_list(listing);
        assert!(names.contains(&"libx264".to_string()));
        assert!(names.contains(&"h264_vaapi".to_string()));
        assert!(names.contains(&"h264_nvenc".to_string()));
        assert!(names.contains(&"aac".to_string()));
        assert!(!names.contains(&"Encoders:".to_string()));
        assert!(!names.contains(&"=".to_string()));
    }

    #[test]
    fn x264_probe_succeeds_on_any_machine_with_ffmpeg() {
        assert!(probe_encoder(&H264Encoder::X264));
    }

    /// Detection completes, is cached, and never picks a hardware encoder
    /// that fails its own functional probe.
    #[test]
    fn detection_returns_a_working_encoder() {
        let encoder = detected_h264_encoder();
        assert!(probe_encoder(encoder), "chosen encoder must really encode");
        assert!(std::ptr::eq(encoder, detected_h264_encoder()), "cached");
    }

    #[test]
    fn labels_and_names_are_consistent() {
        let vaapi = H264Encoder::Vaapi {
            device: PathBuf::from("/dev/dri/renderD128"),
        };
        assert_eq!(vaapi.ffmpeg_name(), "h264_vaapi");
        assert!(vaapi.label().contains("renderD128"));
        assert!(vaapi.is_hardware());
        assert!(!H264Encoder::X264.is_hardware());
    }
}
