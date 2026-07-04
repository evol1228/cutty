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
/// to the device, of which the most recent callback contributed
/// `last_buffer` frames that start hitting the DAC at `callback_instant +
/// latency`. The sample audible *now* is therefore `queued - last_buffer`
/// minus the latency, plus the wall time elapsed since the callback. While
/// paused the position freezes at the queue head, which is exactly where
/// playback resumes.
pub struct PlaybackClock {
    sample_rate: u32,
    epoch: Instant,
    /// Frames handed to the device since the last rebase.
    frames_consumed: AtomicU64,
    /// Media position (in frames) that `frames_consumed == 0` maps to.
    base_frames: AtomicU64,
    /// Output latency (nanoseconds) measured in the last callback.
    latency_nanos: AtomicU64,
    /// Media frames written by the last callback.
    last_buffer_frames: AtomicU64,
    /// Wall time (nanos since epoch) of the last callback.
    last_cb_nanos: AtomicU64,
    /// Transport state. Paused ⇒ the callback emits silence and the
    /// position freezes.
    playing: AtomicBool,
    /// The decoder hit end of stream; cleared by the next seek. Lets the
    /// video side detect that this clock will not advance further.
    ended: AtomicBool,
    /// Seek handshake: the decoder thread parks the new base position here
    /// and must not push post-seek samples until the audio callback has
    /// drained the ring and rebased (`rebase_pending` flips false) —
    /// otherwise the drain would eat valid post-seek audio.
    rebase: Mutex<Option<u64>>,
    rebase_pending: AtomicBool,
}

impl PlaybackClock {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            epoch: Instant::now(),
            frames_consumed: AtomicU64::new(0),
            base_frames: AtomicU64::new(0),
            latency_nanos: AtomicU64::new(0),
            last_buffer_frames: AtomicU64::new(0),
            last_cb_nanos: AtomicU64::new(0),
            playing: AtomicBool::new(false),
            ended: AtomicBool::new(false),
            rebase: Mutex::new(None),
            rebase_pending: AtomicBool::new(false),
        }
    }

    /// Current playback position in seconds.
    pub fn position_secs(&self) -> f64 {
        let rate = f64::from(self.sample_rate);
        let queued_frames =
            self.base_frames.load(Ordering::Acquire) + self.frames_consumed.load(Ordering::Acquire);
        let queued = queued_frames as f64 / rate;
        if !self.playing.load(Ordering::Acquire) {
            return queued;
        }
        // The last callback's buffer has not played yet at the instant it
        // was queued — without subtracting it the clock reads one device
        // period ahead of the DAC.
        let last_buffer = self.last_buffer_frames.load(Ordering::Acquire) as f64 / rate;
        let latency = self.latency_nanos.load(Ordering::Acquire) as f64 * 1e-9;
        let now = self.epoch.elapsed().as_nanos() as u64;
        let since_cb = now.saturating_sub(self.last_cb_nanos.load(Ordering::Acquire)) as f64 * 1e-9;
        (queued - last_buffer - latency + since_cb).clamp(0.0, queued)
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Acquire)
    }

    /// True once the decoder exhausted the stream (until the next seek).
    pub fn is_ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }

    pub(crate) fn set_ended(&self, ended: bool) {
        self.ended.store(ended, Ordering::Release);
    }

    pub(crate) fn set_playing(&self, playing: bool) {
        self.playing.store(playing, Ordering::Release);
    }

    /// Request a rebase to `frames` (presentation frames). Applied by the
    /// next audio callback, which also drains stale samples from the ring.
    pub(crate) fn request_rebase(&self, frames: u64) {
        *self.rebase.lock().expect("clock rebase poisoned") = Some(frames);
        self.rebase_pending.store(true, Ordering::Release);
    }

    /// True while a requested rebase has not yet been applied by the audio
    /// callback. The decoder must not push samples while this holds.
    pub(crate) fn rebase_pending(&self) -> bool {
        self.rebase_pending.load(Ordering::Acquire)
    }

    /// Called by the audio callback: returns a pending rebase, if any.
    /// Uses `try_lock` so the realtime thread never blocks.
    pub(crate) fn take_rebase(&self) -> Option<u64> {
        self.rebase.try_lock().ok().and_then(|mut r| r.take())
    }

    /// Called by the audio callback after draining the ring. Clearing
    /// `rebase_pending` last releases the decoder to push post-seek samples.
    pub(crate) fn apply_rebase(&self, frames: u64) {
        self.frames_consumed.store(0, Ordering::Release);
        self.base_frames.store(frames, Ordering::Release);
        self.rebase_pending.store(false, Ordering::Release);
    }

    pub(crate) fn advance(&self, frames: u64) {
        self.frames_consumed.fetch_add(frames, Ordering::AcqRel);
    }

    pub(crate) fn record_callback(&self, latency_nanos: u64, buffer_frames: u64) {
        self.latency_nanos.store(latency_nanos, Ordering::Release);
        self.last_buffer_frames
            .store(buffer_frames, Ordering::Release);
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
        assert!(c.rebase_pending(), "decoder must be gated while pending");
        let pending = c.take_rebase().expect("rebase pending");
        c.apply_rebase(pending);
        assert!(!c.rebase_pending(), "apply releases the decoder");
        assert_eq!(c.position_secs(), 10.0);
        assert!(c.take_rebase().is_none(), "rebase consumed");
    }

    #[test]
    fn playing_position_subtracts_unplayed_buffer_and_latency() {
        let c = PlaybackClock::new(48_000);
        c.set_playing(true);
        // 100 callbacks of 480 frames (10ms each) = 1s queued.
        for _ in 0..100 {
            c.advance(480);
        }
        c.record_callback(20_000_000, 480); // 20ms latency, 10ms buffer
        let pos = c.position_secs();
        // Expected ≈ 1.0 − 0.010 (unplayed buffer) − 0.020 (latency) = 0.97,
        // plus a few µs of since_cb.
        assert!((0.965..=0.975).contains(&pos), "expected ≈0.97, got {pos}");
        // Never ahead of what was actually consumed.
        assert!(pos <= 1.0 + f64::EPSILON);
    }

    #[test]
    fn playing_position_never_negative() {
        let c = PlaybackClock::new(48_000);
        c.set_playing(true);
        c.record_callback(500_000_000, 0); // huge latency, nothing consumed
        assert_eq!(c.position_secs(), 0.0);
    }

    #[test]
    fn ended_flag_round_trips() {
        let c = PlaybackClock::new(48_000);
        assert!(!c.is_ended());
        c.set_ended(true);
        assert!(c.is_ended());
        c.set_ended(false);
        assert!(!c.is_ended());
    }
}
