//! Playback resolution: which clips are active at a timeline time, and
//! where in their source media. The playback pipeline is built on this.

use crate::model::{clips_touch, Clip, ClipId, Project, Track, TrackId, TrackKind, TOUCH_EPS};

/// A clip active at a resolved timeline time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActiveClip {
    pub track_id: TrackId,
    pub clip_id: ClipId,
    /// Position within the clip's source media, seconds:
    /// `source_in + (t - timeline_in) * speed`.
    pub source_time: f64,
}

/// Whether a track contributes anything perceivable: video and text
/// tracks are out when hidden (no picture), audio tracks when muted (no
/// sound). A muted *video* track still shows its picture — mute only
/// silences the clips' embedded audio, which the mixer handles
/// separately.
fn perceivable(track: &Track) -> bool {
    match track.kind {
        TrackKind::Video | TrackKind::Text => !track.hidden,
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

/// The text layer stack at time `t`, in **compositing order: bottom
/// layer first** — the exact convention of [`resolve_video_layers`],
/// applied to text tracks. The whole text stack composites **above every
/// video track** (renderers append these after the video layers), so a
/// text track's panel position relative to video tracks never hides text
/// behind footage. Hidden text tracks are skipped. `source_time` is the
/// time into the clip (`t - timeline_in`; text has no source medium).
pub fn resolve_text_layers(project: &Project, t: f64) -> Vec<ActiveClip> {
    if !t.is_finite() {
        return Vec::new();
    }
    project
        .tracks
        .iter()
        .rev()
        .filter(|track| track.kind == TrackKind::Text && !track.hidden)
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

// ---------------------------------------------------------------------
// Transitions: cut-bound spans and the per-track visual stack
// ---------------------------------------------------------------------

/// A transition resolved against current clip geometry: where it actually
/// plays. The **effective** span `[start, end)` is `duration` centered on
/// the cut, clamped by [`transition_duration_limit`]; `requested` is the
/// stored duration before clamping.
#[derive(Debug, Clone, PartialEq)]
pub struct TransitionSpan {
    pub track_id: TrackId,
    /// The outgoing clip (owns the `transition_out`).
    pub from_clip: ClipId,
    /// The incoming clip across the cut.
    pub to_clip: ClipId,
    /// Transition id from the shader registry.
    pub kind: String,
    /// The cut instant (the incoming clip's `timeline_in`).
    pub cut: f64,
    /// Effective span start, `cut - d_eff / 2`.
    pub start: f64,
    /// Effective span end, `cut + d_eff / 2`.
    pub end: f64,
    /// Stored (requested) duration, seconds.
    pub requested: f64,
    /// The longest duration this cut currently supports (the UI clamps
    /// its duration drag against this).
    pub max_duration: f64,
}

/// The longest transition the cut between `a` and `b` supports:
///
/// - never longer than either clip (each half-span stays inside its clip,
///   which also keeps chained transitions from overlapping), and
/// - clamped to available **source handles** — the outgoing side needs
///   media past `a.source_out`, the incoming side media before
///   `b.source_in`. A side with (near-)zero handle does *not* constrain
///   the span: it freeze-frames across the overlap instead (clamping to
///   zero would erase the transition entirely).
pub fn transition_duration_limit(project: &Project, a: &Clip, b: &Clip) -> f64 {
    let mut limit = a.duration().min(b.duration());
    // Stills and loops have unlimited handles on both sides.
    if let Some(media) = a.media_id.and_then(|id| project.media(id)) {
        if !media.unbounded_source() {
            let post_handle = (media.duration - a.source_out) / a.speed;
            if post_handle > TOUCH_EPS {
                limit = limit.min(2.0 * post_handle);
            }
        }
    }
    let b_bounded = b
        .media_id
        .and_then(|id| project.media(id))
        .is_none_or(|m| !m.unbounded_source());
    if b_bounded {
        let pre_handle = b.source_in / b.speed;
        if pre_handle > TOUCH_EPS {
            limit = limit.min(2.0 * pre_handle);
        }
    }
    limit
}

/// Every transition in the project resolved to its effective span, in
/// track order. Hidden tracks are included (the timeline UI still shows
/// their chips); renderers skip hidden tracks themselves.
pub fn transition_spans(project: &Project) -> Vec<TransitionSpan> {
    let mut spans = Vec::new();
    for track in project.tracks.iter().filter(|t| t.kind == TrackKind::Video) {
        for pair in track.clips.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            let Some(transition) = &a.transition_out else {
                continue;
            };
            if !clips_touch(a, b) {
                continue; // defensive: validation should prevent this
            }
            let max_duration = transition_duration_limit(project, a, b);
            let d = transition.duration.min(max_duration);
            if d <= TOUCH_EPS || d.is_nan() {
                continue;
            }
            let cut = b.timeline_in;
            spans.push(TransitionSpan {
                track_id: track.id,
                from_clip: a.id,
                to_clip: b.id,
                kind: transition.kind.clone(),
                cut,
                start: cut - d / 2.0,
                end: cut + d / 2.0,
                requested: transition.duration,
                max_duration,
            });
        }
    }
    spans
}

/// What one video track shows at a timeline instant.
#[derive(Debug, Clone, PartialEq)]
pub enum TrackVisual {
    /// One clip, sampled normally.
    Single(ActiveClip),
    /// Two clips blended by a transition shader. Source times are
    /// **extended across the cut**: `from.source_time` may run past its
    /// clip's `source_out` (into handle media, or past the end of the
    /// file — decoders hold the last frame, the freeze case) and
    /// `to.source_time` may be negative (decoders clamp to frame 0,
    /// the incoming freeze case).
    Transition {
        from: ActiveClip,
        to: ActiveClip,
        kind: String,
        /// 0 at span start → 1 at span end, on the caller's time grid.
        progress: f64,
    },
}

/// The video layer stack at `t` with transitions applied: one
/// [`TrackVisual`] per contributing track, in **compositing order
/// (bottom layer first)**, exactly like [`resolve_video_layers`]. Inside
/// a transition span the track contributes the blended pair; everywhere
/// else it degrades to the plain single-clip resolution. Both render
/// frontends consume this.
pub fn resolve_track_visuals(project: &Project, t: f64) -> Vec<TrackVisual> {
    if !t.is_finite() {
        return Vec::new();
    }
    let spans = transition_spans(project);
    project
        .tracks
        .iter()
        .rev()
        .filter(|track| track.kind == TrackKind::Video && !track.hidden)
        .filter_map(|track| {
            let span = spans
                .iter()
                .find(|s| s.track_id == track.id && s.start <= t && t < s.end);
            if let Some(span) = span {
                let a = track.clip(span.from_clip)?;
                let b = track.clip(span.to_clip)?;
                let active = |clip: &Clip| ActiveClip {
                    track_id: track.id,
                    clip_id: clip.id,
                    source_time: clip.source_in + (t - clip.timeline_in) * clip.speed,
                };
                return Some(TrackVisual::Transition {
                    from: active(a),
                    to: active(b),
                    kind: span.kind.clone(),
                    progress: ((t - span.start) / (span.end - span.start)).clamp(0.0, 1.0),
                });
            }
            track
                .clips
                .iter()
                .rfind(|clip| clip.timeline_in <= t && t < clip.timeline_out)
                .map(|clip| {
                    TrackVisual::Single(ActiveClip {
                        track_id: track.id,
                        clip_id: clip.id,
                        source_time: clip.source_in + (t - clip.timeline_in) * clip.speed,
                    })
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
