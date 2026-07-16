//! Where a clip's **audio** plays once video transitions are applied.
//!
//! A video transition implies a short equal-power crossfade of the two
//! clips' own audio across the same span as the picture: the outgoing
//! clip keeps sounding under the incoming one (fading out through its
//! handle audio; decoders yield silence past end of file), and the
//! incoming clip starts early (fading in from as far back as real audio
//! exists). Both the live mixer and the offline export mix are built
//! from this one function, so preview == export holds for sound too.
//! Audio-track (music) clips never participate — transitions are
//! video-only.

use cutty_audio::{EnvelopePoint, FadeRamp, VolumeEnvelope};
use cutty_engine::{Clip, KeyframeProp, TransitionSpan};

/// A clip's audio placement: possibly extended across transition spans,
/// with the matching ramps.
pub(crate) struct AudioPlacement {
    pub timeline_in: f64,
    pub timeline_out: f64,
    pub source_in: f64,
    pub fade_in: Option<FadeRamp>,
    pub fade_out: Option<FadeRamp>,
}

/// The clip's volume-keyframe lane baked into mixer terms: clip-relative
/// keyframe times become absolute timeline seconds (anchored to the
/// clip's own `timeline_in` — a transition-extended placement doesn't
/// shift automation), easings map one-to-one. `None` when the clip has
/// no volume automation. Both the live mixer and the offline export
/// build their segments through this, so automation previews == exports.
pub(crate) fn volume_envelope(clip: &Clip) -> Option<VolumeEnvelope> {
    let lane = clip.keyframes.get(&KeyframeProp::Volume)?;
    let points = lane
        .iter()
        .map(|k| EnvelopePoint {
            t: clip.timeline_in + k.t,
            value: k.value,
            easing: match k.easing {
                cutty_engine::Easing::Linear => cutty_audio::Easing::Linear,
                cutty_engine::Easing::EaseIn => cutty_audio::Easing::EaseIn,
                cutty_engine::Easing::EaseOut => cutty_audio::Easing::EaseOut,
                cutty_engine::Easing::EaseInOut => cutty_audio::Easing::EaseInOut,
            },
        })
        .collect();
    Some(VolumeEnvelope { points })
}

/// Resolve `clip`'s audio placement against the project's transition
/// spans. A clip inside a chain of transitions can be both the incoming
/// side of one span and the outgoing side of the next (both ramps set).
pub(crate) fn audio_placement(clip: &Clip, spans: &[TransitionSpan]) -> AudioPlacement {
    let mut placement = AudioPlacement {
        timeline_in: clip.timeline_in,
        timeline_out: clip.timeline_out,
        source_in: clip.source_in,
        fade_in: None,
        fade_out: None,
    };
    for span in spans {
        if span.from_clip == clip.id {
            // Outgoing: keep sounding to the span end, fading out.
            placement.timeline_out = span.end;
            placement.fade_out = Some(FadeRamp {
                start: span.start,
                end: span.end,
            });
        } else if span.to_clip == clip.id {
            // Incoming: start under the outgoing clip — but never
            // further back than the source's first sample (clamping a
            // negative source position would double up the opening).
            let pre_handle = clip.source_in / clip.speed;
            let t_in = span.start.max(clip.timeline_in - pre_handle);
            placement.source_in = clip.source_in - (clip.timeline_in - t_in) * clip.speed;
            placement.timeline_in = t_in;
            placement.fade_in = Some(FadeRamp {
                start: span.start,
                end: span.end,
            });
        }
    }
    placement
}

#[cfg(test)]
mod tests {
    use super::*;
    use cutty_engine::{transition_spans, Engine, ProjectSettings, TrackKind, Transition};

    /// A|B touching at 2.0 with a 1s fade; A has handle audio past its
    /// out, B starts `b_source_in` into its source.
    fn fixture(
        b_source_in: f64,
    ) -> (
        cutty_engine::Project,
        cutty_engine::ClipId,
        cutty_engine::ClipId,
    ) {
        let mut engine = Engine::new(ProjectSettings::default());
        let media = engine.add_media("/tmp/a.mp4", 10.0, true, true).unwrap();
        let video = engine
            .project()
            .tracks
            .iter()
            .find(|t| t.kind == TrackKind::Video)
            .unwrap()
            .id;
        let a = engine.add_clip(video, media, 0.0, 1.0, 3.0).unwrap();
        let b = engine
            .add_clip(video, media, 2.0, b_source_in, b_source_in + 2.0)
            .unwrap();
        engine
            .set_transition(
                a,
                Some(Transition {
                    kind: "fade".into(),
                    duration: 1.0,
                }),
            )
            .unwrap();
        (engine.project().clone(), a, b)
    }

    #[test]
    fn outgoing_extends_to_span_end_with_fade_out() {
        let (project, a, _) = fixture(5.0);
        let spans = transition_spans(&project);
        let (_, clip) = project.find_clip(a).unwrap();
        let p = audio_placement(clip, &spans);
        assert_eq!(p.timeline_in, 0.0);
        assert!((p.timeline_out - 2.5).abs() < 1e-9, "extends to span end");
        assert_eq!(p.source_in, 1.0);
        assert_eq!(p.fade_in, None);
        let ramp = p.fade_out.unwrap();
        assert!((ramp.start - 1.5).abs() < 1e-9);
        assert!((ramp.end - 2.5).abs() < 1e-9);
    }

    #[test]
    fn incoming_starts_early_with_fade_in() {
        let (project, _, b) = fixture(5.0);
        let spans = transition_spans(&project);
        let (_, clip) = project.find_clip(b).unwrap();
        let p = audio_placement(clip, &spans);
        assert!((p.timeline_in - 1.5).abs() < 1e-9, "starts at span start");
        assert!((p.source_in - 4.5).abs() < 1e-9, "source shifted back");
        assert_eq!(p.timeline_out, 4.0);
        assert!(p.fade_in.is_some());
        assert_eq!(p.fade_out, None);
    }

    #[test]
    fn incoming_without_audio_handle_starts_at_its_cut() {
        // B starts at source 0 — the transition freezes its first video
        // frame across the pre-cut half; audio has nothing before sample
        // 0, so it starts at the cut (ramp already at 0.707 there).
        let (project, _, b) = fixture(0.0);
        let spans = transition_spans(&project);
        let (_, clip) = project.find_clip(b).unwrap();
        let p = audio_placement(clip, &spans);
        assert!(
            (p.timeline_in - 2.0).abs() < 1e-9,
            "no audio before source 0"
        );
        assert_eq!(p.source_in, 0.0);
        let ramp = p.fade_in.unwrap();
        assert!((ramp.gain_in(2.0) - std::f64::consts::FRAC_1_SQRT_2 as f32).abs() < 1e-6);
    }

    #[test]
    fn volume_envelope_bakes_clip_relative_times_to_absolute() {
        use cutty_engine::{Easing, FadeSide};
        let mut engine = Engine::new(ProjectSettings::default());
        let media = engine.add_media("/tmp/a.flac", 10.0, false, true).unwrap();
        let audio = engine
            .project()
            .tracks
            .iter()
            .find(|t| t.kind == TrackKind::Audio)
            .unwrap()
            .id;
        let clip_id = engine.add_clip(audio, media, 2.0, 0.0, 4.0).unwrap();
        let (_, clip) = engine.project().find_clip(clip_id).unwrap();
        assert!(
            volume_envelope(clip).is_none(),
            "no lane → no envelope (unity)"
        );

        engine.set_clip_fade(clip_id, FadeSide::In, 1.0).unwrap();
        engine
            .add_keyframe(
                clip_id,
                cutty_engine::KeyframeProp::Volume,
                2.5,
                0.5,
                Easing::EaseInOut,
            )
            .unwrap();
        let (_, clip) = engine.project().find_clip(clip_id).unwrap();
        let env = volume_envelope(clip).expect("lane → envelope");
        // Clip at timeline 2.0: local keyframes 0 / 1 / 2.5 become
        // absolute 2.0 / 3.0 / 4.5, values and easing preserved.
        let ts: Vec<f64> = env.points.iter().map(|p| p.t).collect();
        assert_eq!(ts, vec![2.0, 3.0, 4.5]);
        assert_eq!(env.points[0].value, 0.0);
        assert_eq!(env.points[2].value, 0.5);
        assert_eq!(env.points[2].easing, cutty_audio::Easing::EaseInOut);
        assert_eq!(env.gain_at(2.5), 0.5, "mid-fade");
        assert_eq!(env.gain_at(0.0), 0.0, "constant before the first point");
    }

    #[test]
    fn equal_power_ramps_cross_at_the_cut() {
        let ramp = FadeRamp {
            start: 1.5,
            end: 2.5,
        };
        assert!((ramp.gain_in(1.5) - 0.0).abs() < 1e-6);
        assert!((ramp.gain_out(1.5) - 1.0).abs() < 1e-6);
        assert!((ramp.gain_in(2.5) - 1.0).abs() < 1e-6);
        assert!((ramp.gain_out(2.5) - 0.0).abs() < 1e-6);
        // Midpoint (the cut): both at √½ — constant summed power.
        let mid_in = ramp.gain_in(2.0);
        let mid_out = ramp.gain_out(2.0);
        assert!((mid_in - std::f64::consts::FRAC_1_SQRT_2 as f32).abs() < 1e-6);
        assert!((mid_in * mid_in + mid_out * mid_out - 1.0).abs() < 1e-6);
    }
}
