//! # cutty-media
//!
//! Media I/O built on the system ffmpeg/ffprobe binaries (via
//! `ffmpeg-sidecar`): probing, 720p proxy generation, frame decoding for
//! playback, and stream-copy export.
//!
//! Phase 0: probe → proxy → playback decode → trim export.

pub(crate) mod cache;
pub mod compose;
pub mod decode;
pub mod encoders;
pub mod error;
pub mod export;
pub mod files;
pub mod framecache;
pub mod jpeg;
pub mod playback;
pub mod probe;
pub mod proxy;
pub mod render;
#[cfg(test)]
pub(crate) mod test_support;
pub mod thumbnail;
pub mod tools;

pub use compose::{FrameSlice, RenderStats, TimelineRenderer};
pub use decode::{FrameView, SourceDecoder};
pub use encoders::{detected_h264_encoder, start_encoder_detection, H264Encoder};
pub use error::MediaError;
pub use export::{export_trim, TrimResult};
pub use files::paths_exist;
pub use playback::{PlayerEvent, TimelinePlayer};
pub use probe::{probe, AudioStreamInfo, MediaInfo, StreamSummary, VideoStreamInfo};
pub use proxy::{generate_proxy, proxy_path_for, ProxyProgress};
pub use render::{
    for_each_composited_frame, run_export, CancelToken, CompositeRunStats, ExportProgress,
    ExportQuality, ExportSpec, ExportStage, ExportSummary,
};
pub use thumbnail::{generate_thumbnail, thumbnail_path_for};
pub use tools::ensure_tools;
