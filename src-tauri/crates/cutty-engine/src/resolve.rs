//! Playback resolution: which clips are active at a timeline time, and
//! where in their source media. The playback pipeline is built on this.

use crate::model::{ClipId, Project, Track, TrackId, TrackKind};

/// A clip active at a resolved timeline time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActiveClip {
    pub track_id: TrackId,
    pub clip_id: ClipId,
    /// Position within the clip's source media, seconds:
    /// `source_in + (t - timeline_in) * speed`.
    pub source_time: f64,
}

/// Whether a track contributes anything perceivable: video tracks are out
/// when hidden (no picture), audio tracks when muted (no sound). A muted
/// *video* track still shows its picture — mute only silences the clips'
/// embedded audio, which the mixer handles separately.
fn perceivable(track: &Track) -> bool {
    match track.kind {
        TrackKind::Video => !track.hidden,
        TrackKind::Audio => !track.muted,
    }
}

/// Resolve timeline time `t` to the set of active clips (at most one per
/// track), in project track order (index 0 first).
///
/// Clip ranges are half-open `[timeline_in, timeline_out)`, so at an exact
/// cut point between two adjacent clips only the *right* clip is active —
/// a frame request at the cut shows the incoming clip, and the end of the
/// timeline resolves to nothing. Adjacent clips may overlap by float dust
/// (within the model's `EPS` "touching" tolerance); the **incoming** clip
/// wins there too, which is why matching scans from the back of the
/// sorted clip list. Hidden video tracks and muted audio tracks are
/// skipped.
pub fn resolve(project: &Project, t: f64) -> Vec<ActiveClip> {
    if !t.is_finite() {
        return Vec::new();
    }
    project
        .tracks
        .iter()
        .filter(|track| perceivable(track))
        .filter_map(|track| {
            track
                .clips
                .iter()
                .rfind(|clip| clip.timeline_in <= t && t < clip.timeline_out)
                .map(|clip| ActiveClip {
                    track_id: track.id,
                    clip_id: clip.id,
                    source_time: clip.source_in + (t - clip.timeline_in) * clip.speed,
                })
        })
        .collect()
}

/// The video layer stack at time `t`, in **compositing order: bottom
/// layer first**. Tracks are stored in visual order (index 0 = top of the
/// timeline panel), so this walks video tracks in reverse — the last
/// video track is the base layer, earlier tracks paint over it. Hidden
/// tracks are skipped; gaps contribute nothing. Empty in gaps across all
/// tracks and past the end.
///
/// Both render frontends (preview and export) consume exactly this — the
/// output frame at `t` is defined as these layers composited bottom→top
/// over black.
pub fn resolve_video_layers(project: &Project, t: f64) -> Vec<ActiveClip> {
    if !t.is_finite() {
        return Vec::new();
    }
    project
        .tracks
        .iter()
        .rev()
        .filter(|track| track.kind == TrackKind::Video && !track.hidden)
        .filter_map(|track| {
            // `rfind`: at a cut instant (including float-dust overlaps
            // inside the model's touching tolerance) the incoming clip
            // wins — see `resolve`.
            track
                .clips
                .iter()
                .rfind(|clip| clip.timeline_in <= t && t < clip.timeline_out)
                .map(|clip| ActiveClip {
                    track_id: track.id,
                    clip_id: clip.id,
                    source_time: clip.source_in + (t - clip.timeline_in) * clip.speed,
                })
        })
        .collect()
}

/// End of the timeline: the latest `timeline_out` across all tracks
/// (hidden and muted tracks included — hiding content doesn't shorten
/// the timeline). `0.0` for an empty project.
pub fn timeline_end(project: &Project) -> f64 {
    project
        .tracks
        .iter()
        .flat_map(|track| track.clips.iter().map(|clip| clip.timeline_out))
        .fold(0.0, f64::max)
}

/// The next edit point strictly after `t`: the nearest clip `timeline_in`
/// or `timeline_out` on a perceivable track (visible video / audible
/// audio). This is what playback lookahead and gap traversal wait for.
/// `None` when nothing changes after `t`.
pub fn next_boundary_after(project: &Project, t: f64) -> Option<f64> {
    if !t.is_finite() {
        return None;
    }
    project
        .tracks
        .iter()
        .filter(|track| perceivable(track))
        .flat_map(|track| {
            track
                .clips
                .iter()
                .flat_map(|clip| [clip.timeline_in, clip.timeline_out])
        })
        .filter(|&edge| edge > t)
        .min_by(f64::total_cmp)
}
