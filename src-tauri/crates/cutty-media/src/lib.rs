//! # cutty-media
//!
//! Media I/O built on the system ffmpeg/ffprobe binaries (via
//! `ffmpeg-sidecar`): probing, 720p proxy generation, frame decoding for
//! playback, and stream-copy export.
//!
//! Phase 0: probe → proxy → playback decode → trim export.

pub mod error;
pub mod export;
pub mod framecache;
pub mod jpeg;
pub mod player;
pub mod probe;
pub mod proxy;
#[cfg(test)]
pub(crate) mod test_support;
pub mod tools;
pub mod video;

pub use error::MediaError;
pub use export::{export_trim, TrimResult};
pub use player::{EventSink, Player, PlayerEvent, PlayerInfo};
pub use probe::{probe, AudioStreamInfo, MediaInfo, StreamSummary, VideoStreamInfo};
pub use proxy::{generate_proxy, proxy_path_for, ProxyProgress};
pub use tools::ensure_tools;
