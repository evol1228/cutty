//! Playback resolution: which clips are active at a timeline time, and
//! where in their source media. The playback pipeline is built on this.

use crate::model::{ClipId, Project, TrackId, TrackKind};

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

/// The clip the player shows at time `t`: the first hit in track order
/// (top video track wins), `None` in gaps and past the end.
pub fn active_video_clip(project: &Project, t: f64) -> Option<ActiveClip> {
    if !t.is_finite() {
        return None;
    }
    project
        .tracks
        .iter()
        .filter(|track| track.kind == TrackKind::Video && !track.muted)
        .find_map(|track| {
            track
                .clips
                .iter()
                .find(|clip| clip.timeline_in <= t && t < clip.timeline_out)
                .map(|clip| ActiveClip {
                    track_id: track.id,
                    clip_id: clip.id,
                    source_time: clip.source_in + (t - clip.timeline_in) * clip.speed,
                })
        })
}

/// End of the timeline: the latest `timeline_out` across all tracks
/// (muted tracks included — muting hides content, it doesn't shorten the
/// timeline). `0.0` for an empty project.
pub fn timeline_end(project: &Project) -> f64 {
    project
        .tracks
        .iter()
        .flat_map(|track| track.clips.iter().map(|clip| clip.timeline_out))
        .fold(0.0, f64::max)
}

/// The next edit point strictly after `t`: the nearest clip `timeline_in`
/// or `timeline_out` on an unmuted track. This is what playback lookahead
/// and gap traversal wait for. `None` when nothing changes after `t`.
pub fn next_boundary_after(project: &Project, t: f64) -> Option<f64> {
    if !t.is_finite() {
        return None;
    }
    project
        .tracks
        .iter()
        .filter(|track| !track.muted)
        .flat_map(|track| {
            track
                .clips
                .iter()
                .flat_map(|clip| [clip.timeline_in, clip.timeline_out])
        })
        .filter(|&edge| edge > t)
        .min_by(f64::total_cmp)
}
