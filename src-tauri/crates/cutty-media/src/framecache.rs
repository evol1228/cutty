//! LRU-ish cache of encoded preview frames.
//!
//! Spawn-per-seek CLI decoding has a ~100 ms floor (ffmpeg process startup
//! alone is ~60 ms), so cold seeks cannot hit the <100 ms budget. Seeks
//! into *visited* content — the dominant pattern when scrubbing — are
//! served from this cache instantly instead (CLAUDE.md: "a background job
//! or a cache, not a pass").
//!
//! Eviction is insertion-ordered (oldest decoded first), which for video
//! playback approximates LRU: the most recently played region stays hot.
//! Owned by the player control thread — no locking.

use std::collections::{HashMap, VecDeque};

/// Default capacity. 720p JPEG q80 frames run 60–120 KB, so this holds
/// roughly 20–35 seconds of 30 fps video — comfortably inside the 300 MB
/// idle-RAM budget alongside decode buffers.
pub const DEFAULT_CAPACITY_BYTES: usize = 64 * 1024 * 1024;

/// A cached, encoded frame.
#[derive(Clone)]
pub struct CachedFrame {
    pub pts_sec: f64,
    pub width: u32,
    pub height: u32,
    pub jpeg: Vec<u8>,
}

/// Frame-index-keyed cache of encoded frames with a byte budget.
pub struct FrameCache {
    frames: HashMap<i64, CachedFrame>,
    order: VecDeque<i64>,
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

    pub fn get(&self, frame_index: i64) -> Option<&CachedFrame> {
        self.frames.get(&frame_index)
    }

    pub fn insert(&mut self, frame_index: i64, frame: CachedFrame) {
        if let Some(old) = self.frames.remove(&frame_index) {
            self.bytes -= old.jpeg.len();
            self.order.retain(|&i| i != frame_index);
        }
        self.bytes += frame.jpeg.len();
        self.frames.insert(frame_index, frame);
        self.order.push_back(frame_index);

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
            pts_sec: 0.0,
            width: 2,
            height: 2,
            jpeg: vec![0u8; bytes],
        }
    }

    #[test]
    fn stores_and_returns_frames() {
        let mut c = FrameCache::new(1000);
        c.insert(5, frame(100));
        assert!(c.get(5).is_some());
        assert!(c.get(6).is_none());
        assert_eq!(c.bytes(), 100);
    }

    #[test]
    fn evicts_oldest_when_over_budget() {
        let mut c = FrameCache::new(250);
        c.insert(1, frame(100));
        c.insert(2, frame(100));
        c.insert(3, frame(100)); // 300 > 250: evict frame 1
        assert!(c.get(1).is_none());
        assert!(c.get(2).is_some());
        assert!(c.get(3).is_some());
        assert_eq!(c.bytes(), 200);
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn reinserting_a_frame_replaces_it_without_leaking_bytes() {
        let mut c = FrameCache::new(1000);
        c.insert(1, frame(100));
        c.insert(1, frame(300));
        assert_eq!(c.bytes(), 300);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn oversized_single_frame_does_not_wedge_the_cache() {
        let mut c = FrameCache::new(50);
        c.insert(1, frame(100)); // bigger than the whole budget
        assert!(c.is_empty());
        assert_eq!(c.bytes(), 0);
        // And the cache still works afterwards.
        c.insert(2, frame(40));
        assert!(c.get(2).is_some());
    }
}
