//! Playback resolution: which clips are active at a timeline time, and
//! where in their source media. The playback pipeline (Phase 1 prompt 4)
//! is built on this.

use crate::model::{ClipId, Project, TrackId};

/// A clip active at a resolved timeline time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActiveClip {
    pub track_id: TrackId,
    pub clip_id: ClipId,
    /// Position within the clip's source media, seconds:
    /// `source_in + (t - timeline_in) * speed`.
    pub source_time: f64,
}

/// Resolve timeline time `t` to the set of active clips, in project track
/// order (top video track first).
///
/// Clip ranges are half-open `[timeline_in, timeline_out)`, so at an exact
/// cut point between two adjacent clips only the *right* clip is active —
/// a frame request at the cut shows the incoming clip, and the end of the
/// timeline resolves to nothing. Muted tracks are skipped (muted means
/// hidden for video tracks, silent for audio tracks).
pub fn resolve(project: &Project, t: f64) -> Vec<ActiveClip> {
    if !t.is_finite() {
        return Vec::new();
    }
    project
        .tracks
        .iter()
        .filter(|track| !track.muted)
        .flat_map(|track| {
            track
                .clips
                .iter()
                .filter(|clip| clip.timeline_in <= t && t < clip.timeline_out)
                .map(|clip| ActiveClip {
                    track_id: track.id,
                    clip_id: clip.id,
                    source_time: clip.source_in + (t - clip.timeline_in) * clip.speed,
                })
        })
        .collect()
}
