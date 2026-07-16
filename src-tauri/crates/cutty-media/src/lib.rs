//! # cutty-media
//!
//! Media I/O built on the system ffmpeg/ffprobe binaries (via
//! `ffmpeg-sidecar`): probing, 720p proxy generation, frame decoding for
//! playback, and stream-copy export.
//!
//! Phase 0: probe → proxy → playback decode → trim export.

pub(crate) mod audio_layout;
pub(crate) mod cache;
pub mod audio_source;
pub mod compose;
pub mod decode;
pub mod encoders;
pub mod error;
pub mod export;
pub mod files;
pub mod filmstrip;
pub mod framecache;
pub mod jpeg;
pub mod peaks;
pub mod playback;
pub mod probe;
pub mod proxy;
pub mod render;
#[cfg(test)]
pub(crate) mod test_support;
pub mod thumbnail;
pub mod tools;

pub use compose::{
    measure_text_block, text_font_families, FrameSlice, RenderStats, TimelineRenderer,
};
pub use audio_source::{open_audio_source, LibavAudioSource};
pub use cutty_gpu::{transition_kind, transitions, TransitionDef};
pub use decode::{FrameView, SourceDecoder};
pub use encoders::{detected_h264_encoder, start_encoder_detection, H264Encoder};
pub use error::MediaError;
pub use export::{export_trim, TrimResult};
pub use files::paths_exist;
pub use filmstrip::{filmstrip_path_for, generate_filmstrip};
pub use peaks::{generate_peaks, peaks_path_for, PEAKS_PER_SEC};
pub use playback::{PlayerEvent, TimelinePlayer};
pub use probe::{probe, AudioStreamInfo, MediaInfo, StreamSummary, VideoStreamInfo};
pub use proxy::{generate_proxy, proxy_path_for, ProxyProgress};
pub use render::{
    export_audio_timeline, for_each_composited_frame, run_export, CancelToken, CompositeRunStats,
    ExportProgress, ExportQuality, ExportSpec, ExportStage, ExportSummary,
};
pub use thumbnail::{generate_thumbnail, thumbnail_path_for};
pub use tools::ensure_tools;
