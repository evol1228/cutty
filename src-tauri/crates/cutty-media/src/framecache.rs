//! LRU-ish cache of encoded preview frames.
//!
//! Keyed by `(media id, source frame index)` — source-relative, so cached
//! frames stay valid across timeline edits and serve any clip that shows
//! the same source content. Seeks into visited content come from here in
//! well under a millisecond; cold seeks pay the in-process decode cost
//! (tens of ms).
//!
//! Eviction is insertion-ordered (oldest decoded first), which for video
//! playback approximates LRU: the most recently played region stays hot.
//! Owned by the player control thread — no locking.

use std::collections::{HashMap, VecDeque};

/// Default capacity. 720p JPEG q80 frames run 60–120 KB, so this holds
/// roughly 20–35 seconds of 30 fps video — comfortably inside the 300 MB
/// idle-RAM budget alongside decode buffers.
pub const DEFAULT_CAPACITY_BYTES: usize = 64 * 1024 * 1024;

/// Cache key: (media id, source frame index).
pub type FrameKey = (u64, i64);

/// A cached, encoded frame.
#[derive(Clone)]
pub struct CachedFrame {
    /// Presentation time within the *source*, seconds.
    pub source_pts_sec: f64,
    pub width: u32,
    pub height: u32,
    pub jpeg: Vec<u8>,
}

/// Frame-keyed cache of encoded frames with a byte budget.
pub struct FrameCache {
    frames: HashMap<FrameKey, CachedFrame>,
    order: VecDeque<FrameKey>,
    bytes: usize,
    max_bytes: usize,
}

impl FrameCache {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            frames: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            max_bytes,
        }
    }

    pub fn get(&self, key: FrameKey) -> Option<&CachedFrame> {
        self.frames.get(&key)
    }

    pub fn insert(&mut self, key: FrameKey, frame: CachedFrame) {
        if let Some(old) = self.frames.remove(&key) {
            self.bytes -= old.jpeg.len();
            self.order.retain(|&k| k != key);
        }
        self.bytes += frame.jpeg.len();
        self.frames.insert(key, frame);
        self.order.push_back(key);

        while self.bytes > self.max_bytes {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(evicted) = self.frames.remove(&oldest) {
                self.bytes -= evicted.jpeg.len();
            }
        }
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(bytes: usize) -> CachedFrame {
        CachedFrame {
            source_pts_sec: 0.0,
            width: 2,
            height: 2,
            jpeg: vec![0u8; bytes],
        }
    }

    #[test]
    fn stores_and_returns_frames_per_media() {
        let mut c = FrameCache::new(1000);
        c.insert((1, 5), frame(100));
        assert!(c.get((1, 5)).is_some());
        assert!(c.get((1, 6)).is_none());
        assert!(c.get((2, 5)).is_none(), "media id is part of the key");
        assert_eq!(c.bytes(), 100);
    }

    #[test]
    fn evicts_oldest_when_over_budget() {
        let mut c = FrameCache::new(250);
        c.insert((1, 1), frame(100));
        c.insert((1, 2), frame(100));
        c.insert((1, 3), frame(100)); // 300 > 250: evict frame 1
        assert!(c.get((1, 1)).is_none());
        assert!(c.get((1, 2)).is_some());
        assert!(c.get((1, 3)).is_some());
        assert_eq!(c.bytes(), 200);
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn reinserting_a_frame_replaces_it_without_leaking_bytes() {
        let mut c = FrameCache::new(1000);
        c.insert((1, 1), frame(100));
        c.insert((1, 1), frame(300));
        assert_eq!(c.bytes(), 300);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn oversized_single_frame_does_not_wedge_the_cache() {
        let mut c = FrameCache::new(50);
        c.insert((1, 1), frame(100)); // bigger than the whole budget
        assert!(c.is_empty());
        assert_eq!(c.bytes(), 0);
        // And the cache still works afterwards.
        c.insert((1, 2), frame(40));
        assert!(c.get((1, 2)).is_some());
    }
}
