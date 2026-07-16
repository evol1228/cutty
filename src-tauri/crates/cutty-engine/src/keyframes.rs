//! Keyframed property animation: per-clip `{prop → [Keyframe]}` lanes,
//! their evaluation, and the lane arithmetic behind structural edits
//! (split/trim windows) and the fade-handle sugar.
//!
//! Semantics (shared by every consumer — mixer, future compositor, UI):
//!
//! - `Keyframe::t` is **clip-relative**: seconds from the clip's
//!   `timeline_in`. Moving a clip therefore moves its automation with it;
//!   splitting and trimming re-anchor lanes so the surviving window
//!   plays back exactly as it did before the edit (see [`window_lane`]).
//! - A lane is sorted by `t`, keyframes at least [`KEYFRAME_MIN_DT`]
//!   apart. Evaluation holds the first value before the first keyframe
//!   and the last value after the last one; between two keyframes the
//!   **left** keyframe's easing shapes the segment.
//! - The `Volume` prop is a **gain multiplier on top of the clip's
//!   static `volume`** (effective gain = `clip.volume × envelope(t)`).
//!   The Inspector's volume slider keeps working when automation
//!   exists, and fades are plain 0→1 ramps regardless of the slider.
//!
//! Phase 3 reuses this machinery for transform/opacity keyframes by
//! adding [`KeyframeProp`] variants (which bumps the project version —
//! see the schema rule on [`crate::model::BlendMode`]).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::model::EPS;

/// Minimum separation between two keyframes on a lane, seconds. Keeps
/// evaluation unambiguous and dots grabbable; matches
/// [`crate::model::MIN_CLIP_DURATION`] so even a minimum-length clip can
/// hold a keyframe at each end.
pub const KEYFRAME_MIN_DT: f64 = 1e-3;

/// Values at or below this read as silence for fade detection (a fade
/// is a lane starting/ending in a silent keyframe — see
/// [`fade_in_duration`]).
pub const FADE_SILENT: f64 = 1e-4;

/// How the segment leaving a keyframe approaches the next one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Easing {
    #[default]
    Linear,
    /// Quadratic start (slow in, fast out): `x²`.
    EaseIn,
    /// Quadratic end (fast in, slow out): `x·(2−x)`.
    EaseOut,
    /// Smoothstep: `x²·(3−2x)`.
    EaseInOut,
}

impl Easing {
    /// Map linear progress `x ∈ [0, 1]` to eased progress (clamped).
    pub fn apply(self, x: f64) -> f64 {
        let x = x.clamp(0.0, 1.0);
        match self {
            Easing::Linear => x,
            Easing::EaseIn => x * x,
            Easing::EaseOut => x * (2.0 - x),
            Easing::EaseInOut => x * x * (3.0 - 2.0 * x),
        }
    }
}

/// One point of a keyframe lane. `easing` shapes the segment from this
/// keyframe **to the next** (the last keyframe's easing is inert).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Keyframe {
    /// Clip-relative time, seconds from the clip's `timeline_in`.
    pub t: f64,
    pub value: f64,
    #[serde(default)]
    pub easing: Easing,
}

/// A keyframable clip property. Only `Volume` exists in Phase 2;
/// transform/opacity land in Phase 3 (new variants bump the project
/// version, like any enum-variant addition).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum KeyframeProp {
    /// Gain multiplier on top of the clip's static `volume`.
    Volume,
}

impl KeyframeProp {
    /// Whether `value` is legal for this property.
    pub(crate) fn valid_value(self, value: f64) -> bool {
        value.is_finite()
            && match self {
                KeyframeProp::Volume => value >= 0.0,
            }
    }

    /// Property path for error messages.
    pub(crate) fn name(self) -> &'static str {
        match self {
            KeyframeProp::Volume => "keyframes.volume",
        }
    }
}

/// A clip's keyframe lanes. Empty map = no animation (and the field
/// serializes away entirely — additive schema, no version bump).
/// Invariant: no empty lanes (removing a lane's last keyframe removes
/// the map entry).
pub type Keyframes = BTreeMap<KeyframeProp, Vec<Keyframe>>;

/// Evaluate a sorted lane at clip-relative time `t`: first value before
/// the first keyframe, last value after the last, eased interpolation
/// between. `None` on an empty lane (no animation — the caller's
/// neutral value applies).
pub fn evaluate(lane: &[Keyframe], t: f64) -> Option<f64> {
    let last = lane.last()?;
    let i = lane.partition_point(|k| k.t <= t);
    if i == 0 {
        return Some(lane[0].value);
    }
    if i == lane.len() {
        return Some(last.value);
    }
    let a = &lane[i - 1];
    let b = &lane[i];
    let span = b.t - a.t;
    if span <= 0.0 {
        return Some(b.value);
    }
    Some(a.value + (b.value - a.value) * a.easing.apply((t - a.t) / span))
}

/// The easing governing the segment that contains clip-relative time
/// `t` (the easing of the last keyframe at or before `t`). Linear when
/// `t` precedes every keyframe (the envelope is constant there).
fn easing_at(lane: &[Keyframe], t: f64) -> Easing {
    match lane.partition_point(|k| k.t <= t) {
        0 => Easing::Linear,
        i => lane[i - 1].easing,
    }
}

/// Re-anchor a lane to the clip window `[from, to]` (clip-relative
/// times of the *old* clip; new local time 0 = old `from`). This is the
/// one rule behind split and trim semantics: keyframes keep their
/// absolute timeline positions, and where the window cuts a moving
/// envelope a boundary keyframe holds the cut value — so the surviving
/// range **sounds exactly as it did before the edit**. (With non-linear
/// easing the boundary is value-exact and the cut segment re-eases over
/// its shorter span; linear segments are preserved exactly.)
///
/// Corollaries:
/// - extending an edge (`from < 0` or `to` past the old duration)
///   changes nothing — the envelope holds its edge value into the
///   revealed range;
/// - keyframes outside the window are dropped (a trimmed-away fade is
///   gone; what remains is continuous).
pub(crate) fn window_lane(lane: &[Keyframe], from: f64, to: f64) -> Vec<Keyframe> {
    let inside: Vec<Keyframe> = lane
        .iter()
        .filter(|k| k.t >= from - EPS && k.t <= to + EPS)
        .map(|k| Keyframe {
            t: (k.t - from).clamp(0.0, to - from),
            ..*k
        })
        .collect();
    let mut out = Vec::with_capacity(inside.len() + 2);

    // Left boundary: needed when keyframes before the window would have
    // shaped the envelope at the cut (and no surviving keyframe sits on
    // the boundary already).
    let cut_left = lane.first().is_some_and(|k| k.t < from - EPS)
        && inside.first().is_none_or(|k| k.t > KEYFRAME_MIN_DT);
    if cut_left {
        out.push(Keyframe {
            t: 0.0,
            value: evaluate(lane, from).expect("lane non-empty"),
            easing: easing_at(lane, from),
        });
    }
    out.extend(inside);
    // Right boundary: mirror of the left.
    let cut_right = lane.last().is_some_and(|k| k.t > to + EPS)
        && out
            .last()
            .is_none_or(|k| k.t < (to - from) - KEYFRAME_MIN_DT);
    if cut_right {
        out.push(Keyframe {
            t: to - from,
            value: evaluate(lane, to).expect("lane non-empty"),
            easing: Easing::Linear,
        });
    }
    debug_assert!(
        out.windows(2)
            .all(|w| w[1].t - w[0].t >= KEYFRAME_MIN_DT - EPS),
        "windowed lane keeps the separation invariant"
    );
    out
}

/// [`window_lane`] over every lane of a clip; lanes that end up empty
/// are dropped (the no-empty-lanes invariant).
pub(crate) fn window_lanes(keyframes: &Keyframes, from: f64, to: f64) -> Keyframes {
    keyframes
        .iter()
        .filter_map(|(prop, lane)| {
            let windowed = window_lane(lane, from, to);
            (!windowed.is_empty()).then_some((*prop, windowed))
        })
        .collect()
}

// ---------------------------------------------------------------------
// Fades: CapCut-style handles as keyframe sugar
// ---------------------------------------------------------------------

/// Which clip edge a fade handle drags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FadeSide {
    /// Fade-in at the clip start.
    In,
    /// Fade-out at the clip end.
    Out,
}

/// Detected fade-in: the lane starts with a silent keyframe at the clip
/// start; its duration is the next keyframe's time (the ramp end). This
/// is a *convention over the volume lane*, not extra state — automation
/// drawn by hand that happens to start silent reads (correctly) as a
/// fade, and the handles edit it.
pub fn fade_in_duration(lane: &[Keyframe]) -> Option<f64> {
    let first = lane.first()?;
    let second = lane.get(1)?;
    (first.t <= KEYFRAME_MIN_DT && first.value <= FADE_SILENT).then_some(second.t)
}

/// Detected fade-out: the lane ends with a silent keyframe at the clip
/// end; the fade spans from the previous keyframe (the ramp start).
pub fn fade_out_duration(lane: &[Keyframe], clip_duration: f64) -> Option<f64> {
    let last = lane.last()?;
    let prev = lane.get(lane.len().checked_sub(2)?)?;
    (last.t >= clip_duration - KEYFRAME_MIN_DT && last.value <= FADE_SILENT)
        .then_some(clip_duration - prev.t)
}

/// Shortest fade the lane stores; below this a drag removes the fade
/// (reads as "no fade" — and the pair must stay a legal lane).
const MIN_FADE: f64 = 2.0 * KEYFRAME_MIN_DT;

/// Rebuild a volume lane with the given fade set to `duration` seconds
/// (0 or too-short removes it). Returns the new lane and the applied
/// (clamped) duration. Rules:
///
/// - the current fade pair (if any) is stripped first, so re-drags
///   rescale the ramp rather than compounding;
/// - the opposite fade limits the span (fades never cross);
/// - other automation inside the new ramp is absorbed by it, and the
///   ramp's inner end takes the envelope's value there, so the fade
///   splices continuously into remaining automation.
pub(crate) fn with_fade(
    lane: &[Keyframe],
    clip_duration: f64,
    side: FadeSide,
    duration: f64,
) -> (Vec<Keyframe>, f64) {
    let mut base = lane.to_vec();
    match side {
        FadeSide::In => {
            if fade_in_duration(&base).is_some() {
                base.drain(..2);
            }
            let limit = fade_out_duration(&base, clip_duration)
                .map_or(clip_duration, |out| clip_duration - out - KEYFRAME_MIN_DT);
            let d = duration.clamp(0.0, limit.max(0.0));
            if d < MIN_FADE {
                return (base, 0.0);
            }
            let value = evaluate(&base, d).unwrap_or(1.0);
            let mut out = vec![
                Keyframe {
                    t: 0.0,
                    value: 0.0,
                    easing: Easing::Linear,
                },
                Keyframe {
                    t: d,
                    value,
                    // The segment leaving the ramp end keeps the shape the
                    // base automation had there.
                    easing: easing_at(&base, d),
                },
            ];
            out.extend(base.into_iter().filter(|k| k.t > d + KEYFRAME_MIN_DT - EPS));
            (out, d)
        }
        FadeSide::Out => {
            if fade_out_duration(&base, clip_duration).is_some() {
                let n = base.len();
                base.drain(n - 2..);
            }
            let limit = fade_in_duration(&base).map_or(clip_duration, |fade_in| {
                clip_duration - fade_in - KEYFRAME_MIN_DT
            });
            let d = duration.clamp(0.0, limit.max(0.0));
            if d < MIN_FADE {
                return (base, 0.0);
            }
            let start = clip_duration - d;
            let value = evaluate(&base, start).unwrap_or(1.0);
            let mut out: Vec<Keyframe> = base
                .into_iter()
                .filter(|k| k.t < start - KEYFRAME_MIN_DT + EPS)
                .collect();
            out.push(Keyframe {
                t: start,
                value,
                easing: Easing::Linear,
            });
            out.push(Keyframe {
                t: clip_duration,
                value: 0.0,
                easing: Easing::Linear,
            });
            (out, d)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kf(t: f64, value: f64) -> Keyframe {
        Keyframe {
            t,
            value,
            easing: Easing::Linear,
        }
    }

    #[test]
    fn evaluate_holds_edges_and_interpolates() {
        assert_eq!(evaluate(&[], 1.0), None);
        let lane = [kf(1.0, 0.2), kf(3.0, 1.0)];
        assert_eq!(evaluate(&lane, 0.0), Some(0.2), "before first");
        assert_eq!(evaluate(&lane, 5.0), Some(1.0), "after last");
        assert_eq!(evaluate(&lane, 1.0), Some(0.2), "on a keyframe");
        let mid = evaluate(&lane, 2.0).unwrap();
        assert!((mid - 0.6).abs() < 1e-12, "linear midpoint: {mid}");
    }

    #[test]
    fn easings_map_progress() {
        assert_eq!(Easing::Linear.apply(0.25), 0.25);
        assert_eq!(Easing::EaseIn.apply(0.5), 0.25);
        assert_eq!(Easing::EaseOut.apply(0.5), 0.75);
        assert_eq!(Easing::EaseInOut.apply(0.5), 0.5);
        assert_eq!(Easing::EaseInOut.apply(0.25), 0.15625);
        // All easings pin the endpoints (and clamp outside them).
        for e in [
            Easing::Linear,
            Easing::EaseIn,
            Easing::EaseOut,
            Easing::EaseInOut,
        ] {
            assert_eq!(e.apply(0.0), 0.0);
            assert_eq!(e.apply(1.0), 1.0);
            assert_eq!(e.apply(-1.0), 0.0);
            assert_eq!(e.apply(2.0), 1.0);
        }
    }

    #[test]
    fn eased_segment_uses_left_keyframes_easing() {
        let lane = [
            Keyframe {
                t: 0.0,
                value: 0.0,
                easing: Easing::EaseIn,
            },
            kf(2.0, 1.0),
        ];
        assert_eq!(evaluate(&lane, 1.0), Some(0.25), "x² at x=0.5");
    }

    #[test]
    fn window_keeps_inside_keyframes_and_adds_boundaries() {
        // Ramp 0→1 over [0, 10]; window [2, 6] must hold 0.2→0.6.
        let lane = [kf(0.0, 0.0), kf(10.0, 1.0)];
        let w = window_lane(&lane, 2.0, 6.0);
        assert_eq!(w.len(), 2);
        assert!((w[0].t - 0.0).abs() < 1e-12 && (w[0].value - 0.2).abs() < 1e-12);
        assert!((w[1].t - 4.0).abs() < 1e-12 && (w[1].value - 0.6).abs() < 1e-12);
        // The windowed envelope equals the original over the window.
        for i in 0..=20 {
            let local = 4.0 * f64::from(i) / 20.0;
            assert!(
                (evaluate(&w, local).unwrap() - evaluate(&lane, 2.0 + local).unwrap()).abs()
                    < 1e-12,
                "envelope preserved at {local}"
            );
        }
    }

    #[test]
    fn window_without_outside_keyframes_adds_no_boundaries() {
        let lane = [kf(1.0, 0.5), kf(2.0, 0.8)];
        let w = window_lane(&lane, 0.5, 3.0);
        assert_eq!(w, vec![kf(0.5, 0.5), kf(1.5, 0.8)], "pure shift");
    }

    #[test]
    fn window_extension_is_a_pure_shift() {
        // Extending left (from < 0) reveals constant envelope, no new kfs.
        let lane = [kf(1.0, 0.5), kf(2.0, 0.8)];
        let w = window_lane(&lane, -1.0, 3.0);
        assert_eq!(w, vec![kf(2.0, 0.5), kf(3.0, 0.8)]);
    }

    #[test]
    fn window_boundary_skipped_when_keyframe_sits_on_it() {
        let lane = [kf(0.0, 0.0), kf(2.0, 0.4), kf(10.0, 1.0)];
        let w = window_lane(&lane, 2.0, 6.0);
        // kf at exactly the window start serves as the boundary.
        assert_eq!(w[0], kf(0.0, 0.4));
        assert_eq!(w.len(), 2, "inside kf + right boundary");
        assert!((w[1].t - 4.0).abs() < 1e-12);
    }

    #[test]
    fn fade_detection_round_trips_with_builder() {
        let dur = 10.0;
        let (lane, applied) = with_fade(&[], dur, FadeSide::In, 1.5);
        assert_eq!(applied, 1.5);
        assert_eq!(fade_in_duration(&lane), Some(1.5));
        assert_eq!(fade_out_duration(&lane, dur), None);
        assert_eq!(evaluate(&lane, 0.0), Some(0.0));
        assert_eq!(evaluate(&lane, 1.5), Some(1.0));
        assert_eq!(evaluate(&lane, 5.0), Some(1.0));

        let (lane, applied) = with_fade(&lane, dur, FadeSide::Out, 2.0);
        assert_eq!(applied, 2.0);
        assert_eq!(fade_in_duration(&lane), Some(1.5));
        assert_eq!(fade_out_duration(&lane, dur), Some(2.0));
        assert_eq!(evaluate(&lane, 10.0), Some(0.0));
        assert_eq!(evaluate(&lane, 9.0), Some(0.5));

        // Re-drag rescales rather than compounding.
        let (lane, applied) = with_fade(&lane, dur, FadeSide::In, 3.0);
        assert_eq!(applied, 3.0);
        assert_eq!(fade_in_duration(&lane), Some(3.0));
        assert_eq!(evaluate(&lane, 3.0), Some(1.0));

        // Dragging to zero removes the pair entirely.
        let (lane, applied) = with_fade(&lane, dur, FadeSide::In, 0.0);
        assert_eq!(applied, 0.0);
        assert_eq!(fade_in_duration(&lane), None);
        assert_eq!(fade_out_duration(&lane, dur), Some(2.0), "fade-out kept");
        let (lane, _) = with_fade(&lane, dur, FadeSide::Out, 0.0);
        assert!(lane.is_empty(), "no automation left");
    }

    #[test]
    fn fades_never_cross() {
        let dur = 4.0;
        let (lane, _) = with_fade(&[], dur, FadeSide::In, 3.0);
        let (lane, applied) = with_fade(&lane, dur, FadeSide::Out, 3.5);
        assert!(
            applied <= 1.0 - KEYFRAME_MIN_DT + 1e-12,
            "clamped: {applied}"
        );
        assert_eq!(fade_in_duration(&lane), Some(3.0), "fade-in untouched");
        assert!(fade_out_duration(&lane, dur).is_some());
    }

    #[test]
    fn fade_splices_into_existing_automation() {
        // User automation: 0.5 everywhere via one keyframe at t=5.
        let base = vec![kf(5.0, 0.5)];
        let (lane, _) = with_fade(&base, 10.0, FadeSide::In, 2.0);
        // Ramp rises 0 → 0.5 (the envelope value at the ramp end).
        assert_eq!(evaluate(&lane, 2.0), Some(0.5));
        assert_eq!(evaluate(&lane, 7.0), Some(0.5));
        assert_eq!(lane.len(), 3, "pair + surviving user keyframe");
    }
}
