//! Snapping as a pure function over candidate times.
//!
//! The UI owns pixel space: it converts its pixel snap radius to seconds
//! via the current zoom, gathers candidates (clip edges + playhead), and
//! calls [`snap`]. The engine performs no pixel math.

use crate::model::{ClipId, Project};

/// Snap `t` to the nearest candidate within `threshold` seconds.
///
/// Returns `None` when no candidate is in range (the caller keeps `t`
/// unsnapped). Non-finite candidates are ignored; a non-finite `t` or a
/// non-finite/negative `threshold` never snaps. On an exact distance tie
/// the earlier candidate wins, so the result is deterministic.
pub fn snap(t: f64, candidates: &[f64], threshold: f64) -> Option<f64> {
    if !t.is_finite() || !threshold.is_finite() || threshold < 0.0 {
        return None;
    }
    let mut best: Option<f64> = None;
    let mut best_distance = f64::INFINITY;
    for &candidate in candidates {
        if !candidate.is_finite() {
            continue;
        }
        let distance = (candidate - t).abs();
        let closer = distance < best_distance
            || (distance == best_distance && best.is_some_and(|b| candidate < b));
        if distance <= threshold && closer {
            best = Some(candidate);
            best_distance = distance;
        }
    }
    best
}

/// Collect the standard snap candidates from a project: every clip edge on
/// every track, plus the playhead if given. Clips in `exclude` contribute
/// no edges (a dragged clip must not snap to itself). The result is sorted
/// and deduplicated.
pub fn snap_candidates(project: &Project, playhead: Option<f64>, exclude: &[ClipId]) -> Vec<f64> {
    let mut candidates: Vec<f64> = project
        .tracks
        .iter()
        .flat_map(|track| track.clips.iter())
        .filter(|clip| !exclude.contains(&clip.id))
        .flat_map(|clip| [clip.timeline_in, clip.timeline_out])
        .chain(playhead)
        .filter(|t| t.is_finite())
        .collect();
    candidates.sort_by(f64::total_cmp);
    candidates.dedup();
    candidates
}
