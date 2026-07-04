//! The playback master clock.
//!
//! The cpal output callback is the only writer of the counters; anyone may
//! read the position. Video presentation chases this clock — never the
//! reverse (CLAUDE.md rule 5).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// Sample-accurate playback position, owned by the audio thread.
///
/// Position model while playing: `base + consumed` frames have been handed
/// to the device; the sample hitting the DAC *now* lags that queue head by
/// the measured output latency, and `since_cb` wall time has elapsed since
/// the last callback measured it. While paused the position freezes at the
/// queue head, which is exactly where playback resumes.
pub struct PlaybackClock {
    sample_rate: u32,
    epoch: Instant,
    /// Frames handed to the device since the last rebase.
    frames_consumed: AtomicU64,
    /// Media position (in frames) that `frames_consumed == 0` maps to.
    base_frames: AtomicU64,
    /// Output latency (nanoseconds) measured in the last callback.
    latency_nanos: AtomicU64,
    /// Wall time (nanos since epoch) of the last callback.
    last_cb_nanos: AtomicU64,
    /// Transport state. Paused ⇒ the callback emits silence and the
    /// position freezes.
    playing: AtomicBool,
    /// Seek handshake: the decoder thread parks the new base position here;
    /// the audio callback drains the ring buffer and rebases the counters so
    /// audio and position flip atomically.
    rebase: Mutex<Option<u64>>,
}

impl PlaybackClock {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            epoch: Instant::now(),
            frames_consumed: AtomicU64::new(0),
            base_frames: AtomicU64::new(0),
            latency_nanos: AtomicU64::new(0),
            last_cb_nanos: AtomicU64::new(0),
            playing: AtomicBool::new(false),
            rebase: Mutex::new(None),
        }
    }

    /// Current playback position in seconds.
    pub fn position_secs(&self) -> f64 {
        let rate = f64::from(self.sample_rate);
        let queued = (self.base_frames.load(Ordering::Acquire)
            + self.frames_consumed.load(Ordering::Acquire)) as f64
            / rate;
        if !self.playing.load(Ordering::Acquire) {
            return queued;
        }
        let latency = self.latency_nanos.load(Ordering::Acquire) as f64 * 1e-9;
        let now = self.epoch.elapsed().as_nanos() as u64;
        let since_cb =
            now.saturating_sub(self.last_cb_nanos.load(Ordering::Acquire)) as f64 * 1e-9;
        (queued - latency + since_cb).clamp(0.0, queued)
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Acquire)
    }

    pub(crate) fn set_playing(&self, playing: bool) {
        self.playing.store(playing, Ordering::Release);
    }

    /// Request a rebase to `frames` (presentation frames). Applied by the
    /// next audio callback, which also drains stale samples from the ring.
    pub(crate) fn request_rebase(&self, frames: u64) {
        *self.rebase.lock().expect("clock rebase poisoned") = Some(frames);
    }

    /// Called by the audio callback: returns a pending rebase, if any.
    /// Uses `try_lock` so the realtime thread never blocks.
    pub(crate) fn take_rebase(&self) -> Option<u64> {
        self.rebase.try_lock().ok().and_then(|mut r| r.take())
    }

    pub(crate) fn apply_rebase(&self, frames: u64) {
        self.frames_consumed.store(0, Ordering::Release);
        self.base_frames.store(frames, Ordering::Release);
    }

    pub(crate) fn advance(&self, frames: u64) {
        self.frames_consumed.fetch_add(frames, Ordering::AcqRel);
    }

    pub(crate) fn record_callback(&self, latency_nanos: u64) {
        self.latency_nanos.store(latency_nanos, Ordering::Release);
        self.last_cb_nanos
            .store(self.epoch.elapsed().as_nanos() as u64, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_paused_at_zero() {
        let c = PlaybackClock::new(48_000);
        assert!(!c.is_playing());
        assert_eq!(c.position_secs(), 0.0);
    }

    #[test]
    fn paused_position_is_the_queue_head() {
        let c = PlaybackClock::new(48_000);
        c.advance(48_000); // 1s handed to the device
        assert_eq!(c.position_secs(), 1.0);
    }

    #[test]
    fn rebase_resets_consumed_and_moves_base() {
        let c = PlaybackClock::new(48_000);
        c.advance(48_000);
        c.request_rebase(10 * 48_000);
        let pending = c.take_rebase().expect("rebase pending");
        c.apply_rebase(pending);
        assert_eq!(c.position_secs(), 10.0);
        assert!(c.take_rebase().is_none(), "rebase consumed");
    }

    #[test]
    fn playing_position_subtracts_latency_and_extrapolates() {
        let c = PlaybackClock::new(48_000);
        c.set_playing(true);
        c.advance(48_000);
        c.record_callback(20_000_000); // 20ms latency, callback "now"
        let pos = c.position_secs();
        assert!(pos < 1.0, "latency must pull position behind the queue head");
        assert!(pos > 0.9, "but by roughly the latency, got {pos}");
        // Never ahead of what was actually consumed.
        assert!(pos <= 1.0 + f64::EPSILON);
    }

    #[test]
    fn playing_position_never_negative() {
        let c = PlaybackClock::new(48_000);
        c.set_playing(true);
        c.record_callback(500_000_000); // huge latency, nothing consumed
        assert_eq!(c.position_secs(), 0.0);
    }
}
