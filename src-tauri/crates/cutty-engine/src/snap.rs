//! Snapping as a pure function over candidate times.
//!
//! The UI owns pixel space: it converts its pixel snap radius to seconds
//! via the current zoom, gathers candidates (clip edges + playhead), and
//! calls [`snap`]. The engine performs no pixel math.

use serde::Serialize;

use crate::error::EngineError;
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

/// Snap a single time against the standard project candidates (clip edges
/// plus the optional playhead), excluding the clips in `exclude`. Returns
/// `None` when nothing is within `threshold` seconds.
///
/// This is the one-shot form of [`snap_candidates`] + [`snap`] used by
/// trim gestures and playhead scrubbing.
pub fn snap_time(
    project: &Project,
    t: f64,
    threshold: f64,
    playhead: Option<f64>,
    exclude: &[ClipId],
) -> Option<f64> {
    snap(t, &snap_candidates(project, playhead, exclude), threshold)
}

/// Result of snapping a clip-move gesture: the `timeline_in` to request
/// from `MoveClip`, and the candidate time that matched (where the UI
/// draws its snap indicator), if any.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnappedMove {
    pub timeline_in: f64,
    pub snap_point: Option<f64>,
}

/// Snap a clip-move gesture: both the clip's would-be start *and* end edge
/// compete for the nearest candidate (the dragged clip's own edges are
/// excluded), and the closer match wins. Returns the resulting
/// `timeline_in` (unchanged if nothing is within `threshold`) plus the
/// matched candidate for the UI's indicator line.
pub fn snap_clip_move(
    project: &Project,
    clip_id: ClipId,
    desired_in: f64,
    threshold: f64,
    playhead: Option<f64>,
) -> Result<SnappedMove, EngineError> {
    let (_, clip) = project
        .find_clip(clip_id)
        .ok_or(EngineError::UnknownClip(clip_id))?;
    let duration = clip.duration();
    let desired_out = desired_in + duration;
    let candidates = snap_candidates(project, playhead, &[clip_id]);

    let start = snap(desired_in, &candidates, threshold);
    let end = snap(desired_out, &candidates, threshold);
    let (timeline_in, snap_point) = match (start, end) {
        (Some(s), Some(e)) => {
            // Ties go to the start edge, matching snap()'s determinism.
            if (s - desired_in).abs() <= (e - desired_out).abs() {
                (s, Some(s))
            } else {
                (e - duration, Some(e))
            }
        }
        (Some(s), None) => (s, Some(s)),
        (None, Some(e)) => (e - duration, Some(e)),
        (None, None) => (desired_in, None),
    };
    Ok(SnappedMove {
        timeline_in,
        snap_point,
    })
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
