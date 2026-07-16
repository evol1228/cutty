//! Volume-keyframe integration tests: the add/move/remove commands
//! (ordering, clamping, dedup, undo round-trips), the clip-relative
//! semantics through split/trim/move, fade sugar, and extract-audio.

mod common;

use common::{engine_with_media, serialized, Fixture, MEDIA_DUR};
use cutty_engine::{
    evaluate_keyframes, fade_in_duration, fade_out_duration, ClipId, Easing, EngineError, FadeSide,
    Keyframe, KeyframeProp, TrackKind, TrimEdge, KEYFRAME_MIN_DT,
};

const VOL: KeyframeProp = KeyframeProp::Volume;

/// Fixture plus one 4-second audio clip at timeline [1, 5).
fn with_audio_clip() -> (Fixture, ClipId) {
    let mut f = engine_with_media();
    let clip = f.engine.add_clip(f.audio, f.media, 1.0, 0.0, 4.0).unwrap();
    (f, clip)
}

fn lane(f: &Fixture, clip: ClipId) -> Vec<Keyframe> {
    let (_, c) = f.engine.project().find_clip(clip).unwrap();
    c.keyframes.get(&VOL).cloned().unwrap_or_default()
}

/// Envelope of `clip` at clip-relative `t` (unity when unanimated).
fn env(f: &Fixture, clip: ClipId, t: f64) -> f64 {
    evaluate_keyframes(&lane(f, clip), t).unwrap_or(1.0)
}

// ---------------------------------------------------------------------
// add / move / remove commands
// ---------------------------------------------------------------------

#[test]
fn add_keyframes_out_of_order_keeps_the_lane_sorted() {
    let (mut f, clip) = with_audio_clip();
    f.engine
        .add_keyframe(clip, VOL, 3.0, 0.5, Easing::Linear)
        .unwrap();
    f.engine
        .add_keyframe(clip, VOL, 1.0, 1.5, Easing::Linear)
        .unwrap();
    f.engine
        .add_keyframe(clip, VOL, 2.0, 1.0, Easing::EaseIn)
        .unwrap();
    let ts: Vec<f64> = lane(&f, clip).iter().map(|k| k.t).collect();
    assert_eq!(ts, vec![1.0, 2.0, 3.0]);
}

#[test]
fn add_clamps_time_into_the_clip() {
    let (mut f, clip) = with_audio_clip();
    let applied = f
        .engine
        .add_keyframe(clip, VOL, 99.0, 0.5, Easing::Linear)
        .unwrap();
    assert_eq!(applied, 4.0, "clamped to the clip duration");
    let applied = f
        .engine
        .add_keyframe(clip, VOL, -3.0, 0.5, Easing::Linear)
        .unwrap();
    assert_eq!(applied, 0.0, "clamped to the clip start");
}

#[test]
fn add_at_the_same_time_replaces_instead_of_duplicating() {
    let (mut f, clip) = with_audio_clip();
    f.engine
        .add_keyframe(clip, VOL, 2.0, 0.5, Easing::Linear)
        .unwrap();
    let before = serialized(&f.engine);
    // Within the separation minimum of the existing keyframe → replace.
    let applied = f
        .engine
        .add_keyframe(
            clip,
            VOL,
            2.0 + KEYFRAME_MIN_DT / 4.0,
            1.25,
            Easing::EaseOut,
        )
        .unwrap();
    assert_eq!(applied, 2.0, "landed on the existing keyframe's time");
    let l = lane(&f, clip);
    assert_eq!(l.len(), 1, "no duplicate");
    assert_eq!(l[0].value, 1.25);
    assert_eq!(l[0].easing, Easing::EaseOut);
    // The replace is one undoable step back to the pre-replace state.
    f.engine.undo().unwrap();
    assert_eq!(serialized(&f.engine), before);
    assert_eq!(lane(&f, clip)[0].value, 0.5);
}

#[test]
fn add_rejects_illegal_values() {
    let (mut f, clip) = with_audio_clip();
    for bad in [-0.1, f64::NAN, f64::INFINITY] {
        assert!(matches!(
            f.engine.add_keyframe(clip, VOL, 1.0, bad, Easing::Linear),
            Err(EngineError::InvalidProperty { .. })
        ));
    }
    assert!(matches!(
        f.engine
            .add_keyframe(clip, VOL, f64::NAN, 1.0, Easing::Linear),
        Err(EngineError::InvalidProperty { .. })
    ));
}

#[test]
fn move_clamps_between_neighbors_and_round_trips_undo() {
    let (mut f, clip) = with_audio_clip();
    for (t, v) in [(1.0, 0.2), (2.0, 0.8), (3.0, 0.4)] {
        f.engine
            .add_keyframe(clip, VOL, t, v, Easing::Linear)
            .unwrap();
    }
    let before = serialized(&f.engine);

    // Dragging the middle keyframe past its right neighbor stops one
    // separation short of it.
    let applied = f.engine.move_keyframe(clip, VOL, 2.0, 3.7, 0.9).unwrap();
    assert!(
        (applied - (3.0 - KEYFRAME_MIN_DT)).abs() < 1e-12,
        "{applied}"
    );
    let ts: Vec<f64> = lane(&f, clip).iter().map(|k| k.t).collect();
    assert_eq!(ts, vec![1.0, applied, 3.0], "still sorted, no crossing");

    f.engine.undo().unwrap();
    assert_eq!(serialized(&f.engine), before, "undo restores exact floats");
}

#[test]
fn move_unknown_keyframe_errors() {
    let (mut f, clip) = with_audio_clip();
    f.engine
        .add_keyframe(clip, VOL, 2.0, 0.5, Easing::Linear)
        .unwrap();
    assert!(matches!(
        f.engine.move_keyframe(clip, VOL, 2.5, 3.0, 0.5),
        Err(EngineError::UnknownKeyframe { .. })
    ));
}

#[test]
fn remove_drops_the_lane_when_it_empties_and_undo_restores() {
    let (mut f, clip) = with_audio_clip();
    f.engine
        .add_keyframe(clip, VOL, 2.0, 0.5, Easing::EaseInOut)
        .unwrap();
    let before = serialized(&f.engine);

    f.engine.remove_keyframe(clip, VOL, 2.0).unwrap();
    let (_, c) = f.engine.project().find_clip(clip).unwrap();
    assert!(c.keyframes.is_empty(), "empty lane leaves no map entry");

    f.engine.undo().unwrap();
    assert_eq!(serialized(&f.engine), before);
    f.engine.redo().unwrap();
    assert!(lane(&f, clip).is_empty());
}

#[test]
fn keyframe_edits_respect_track_locks() {
    let (mut f, clip) = with_audio_clip();
    f.engine
        .set_track_flag(f.audio, cutty_engine::TrackFlag::Locked, true)
        .unwrap();
    assert!(matches!(
        f.engine.add_keyframe(clip, VOL, 1.0, 0.5, Easing::Linear),
        Err(EngineError::TrackLocked { .. })
    ));
    assert!(matches!(
        f.engine.set_clip_fade(clip, FadeSide::In, 1.0),
        Err(EngineError::TrackLocked { .. })
    ));
}

#[test]
fn drag_gesture_in_a_transaction_is_one_undo_entry() {
    let (mut f, clip) = with_audio_clip();
    f.engine
        .add_keyframe(clip, VOL, 2.0, 1.0, Easing::Linear)
        .unwrap();
    let entries_before = f.engine.undo_depth();

    f.engine.begin_transaction().unwrap();
    let mut t = 2.0;
    for step in 1..=10 {
        t = f
            .engine
            .move_keyframe(
                clip,
                VOL,
                t,
                2.0 + 0.1 * f64::from(step),
                1.0 + 0.05 * f64::from(step),
            )
            .unwrap();
    }
    f.engine.commit_transaction().unwrap();

    assert_eq!(f.engine.undo_depth(), entries_before + 1, "one entry");
    f.engine.undo().unwrap();
    assert_eq!(lane(&f, clip)[0].t, 2.0, "back to the pre-drag keyframe");
}

// ---------------------------------------------------------------------
// Clip-relative semantics: move / split / trim
// ---------------------------------------------------------------------

#[test]
fn keyframes_ride_along_with_clip_moves() {
    let (mut f, clip) = with_audio_clip();
    f.engine
        .add_keyframe(clip, VOL, 1.5, 0.3, Easing::Linear)
        .unwrap();
    let before = lane(&f, clip);
    f.engine.move_clip(clip, 3.0).unwrap();
    assert_eq!(lane(&f, clip), before, "clip-relative times unchanged");
}

#[test]
fn split_preserves_a_linear_envelope_exactly() {
    let (mut f, clip) = with_audio_clip();
    // Ramp 0.2 → 1.0 across the whole clip [1, 5): kfs at local 0 and 4.
    f.engine
        .add_keyframe(clip, VOL, 0.0, 0.2, Easing::Linear)
        .unwrap();
    f.engine
        .add_keyframe(clip, VOL, 4.0, 1.0, Easing::Linear)
        .unwrap();

    let right = f.engine.split_clip(clip, 2.5).unwrap(); // local 1.5
    let (left_lane, right_lane) = (lane(&f, clip), lane(&f, right));
    assert_eq!(left_lane.len(), 2, "ramp start + boundary");
    assert_eq!(right_lane.len(), 2, "boundary + ramp end");

    // Sample the original ramp across both halves: identical values.
    for i in 0..=15 {
        let local = 1.5 * f64::from(i) / 15.0; // inside the left half
        let expected = 0.2 + (1.0 - 0.2) * (local / 4.0);
        assert!(
            (env(&f, clip, local) - expected).abs() < 1e-9,
            "left {local}"
        );
    }
    for i in 0..=25 {
        let local = 2.5 * f64::from(i) / 25.0; // inside the right half
        let expected = 0.2 + (1.0 - 0.2) * ((1.5 + local) / 4.0);
        assert!(
            (env(&f, right, local) - expected).abs() < 1e-9,
            "right {local}"
        );
    }

    // Undo restores the original clip and lane verbatim.
    f.engine.undo().unwrap();
    let ts: Vec<f64> = lane(&f, clip).iter().map(|k| k.t).collect();
    assert_eq!(ts, vec![0.0, 4.0]);
}

#[test]
fn split_hands_each_side_only_its_keyframes() {
    let (mut f, clip) = with_audio_clip();
    f.engine
        .add_keyframe(clip, VOL, 0.5, 0.5, Easing::Linear)
        .unwrap();
    f.engine
        .add_keyframe(clip, VOL, 3.5, 1.5, Easing::Linear)
        .unwrap();
    let right = f.engine.split_clip(clip, 3.0).unwrap(); // local 2.0

    assert_eq!(lane(&f, clip).len(), 2, "kf at 0.5 + cut boundary");
    let right_lane = lane(&f, right);
    assert_eq!(right_lane.len(), 2, "boundary + kf at old 3.5");
    assert!(
        (right_lane[1].t - 1.5).abs() < 1e-12,
        "re-anchored to the half"
    );
}

#[test]
fn trim_start_keeps_the_surviving_envelope_and_undo_restores_it() {
    let (mut f, clip) = with_audio_clip();
    // Ramp over the first half: local kfs 0 → 2.
    f.engine
        .add_keyframe(clip, VOL, 0.0, 0.0, Easing::Linear)
        .unwrap();
    f.engine
        .add_keyframe(clip, VOL, 2.0, 1.0, Easing::Linear)
        .unwrap();
    let before = serialized(&f.engine);

    // Trim the start from 1.0 to 2.0 (cutting one second into the ramp).
    f.engine.trim_clip(clip, TrimEdge::Start, 2.0).unwrap();
    let l = lane(&f, clip);
    assert_eq!(l.len(), 2);
    assert!(
        (l[0].t - 0.0).abs() < 1e-12 && (l[0].value - 0.5).abs() < 1e-9,
        "boundary keyframe holds the envelope value at the cut"
    );
    assert!(
        (l[1].t - 1.0).abs() < 1e-12 && (l[1].value - 1.0).abs() < 1e-9,
        "surviving keyframe keeps its absolute position"
    );

    f.engine.undo().unwrap();
    assert_eq!(
        serialized(&f.engine),
        before,
        "trim + lane rewrite is one entry"
    );
}

#[test]
fn trim_end_drops_cut_keyframes_and_extension_holds_the_edge_value() {
    let (mut f, clip) = with_audio_clip();
    f.engine
        .add_keyframe(clip, VOL, 1.0, 0.5, Easing::Linear)
        .unwrap();
    f.engine
        .add_keyframe(clip, VOL, 3.5, 1.5, Easing::Linear)
        .unwrap();

    // Shrink the end to local 2.0: the 3.5 keyframe is cut away, a
    // boundary at the new end holds the ramp value there.
    f.engine.trim_clip(clip, TrimEdge::End, 3.0).unwrap();
    let l = lane(&f, clip);
    assert_eq!(l.len(), 2);
    let expected = 0.5 + (1.5 - 0.5) * ((2.0 - 1.0) / (3.5 - 1.0));
    assert!((l[1].t - 2.0).abs() < 1e-12);
    assert!((l[1].value - expected).abs() < 1e-9);

    // Extend back out: pure reveal, the envelope holds its edge value.
    f.engine.trim_clip(clip, TrimEdge::End, 4.5).unwrap();
    assert_eq!(lane(&f, clip).len(), 2, "no keyframes conjured");
    assert!(
        (env(&f, clip, 3.4) - expected).abs() < 1e-9,
        "held constant"
    );
}

// ---------------------------------------------------------------------
// Fades
// ---------------------------------------------------------------------

#[test]
fn fade_handles_create_adjust_and_remove_keyframe_pairs() {
    let (mut f, clip) = with_audio_clip();
    let applied = f.engine.set_clip_fade(clip, FadeSide::In, 1.0).unwrap();
    assert_eq!(applied, 1.0);
    let applied = f.engine.set_clip_fade(clip, FadeSide::Out, 0.5).unwrap();
    assert_eq!(applied, 0.5);

    let l = lane(&f, clip);
    assert_eq!(fade_in_duration(&l), Some(1.0));
    assert_eq!(fade_out_duration(&l, 4.0), Some(0.5));
    assert_eq!(env(&f, clip, 0.0), 0.0);
    assert!((env(&f, clip, 0.5) - 0.5).abs() < 1e-9, "linear ramp");
    assert_eq!(env(&f, clip, 2.0), 1.0);
    assert_eq!(env(&f, clip, 4.0), 0.0);

    // Re-drag rescales; drag to zero removes; each step is one undo.
    f.engine.set_clip_fade(clip, FadeSide::In, 2.0).unwrap();
    assert_eq!(fade_in_duration(&lane(&f, clip)), Some(2.0));
    f.engine.set_clip_fade(clip, FadeSide::In, 0.0).unwrap();
    assert_eq!(fade_in_duration(&lane(&f, clip)), None);
    assert_eq!(
        fade_out_duration(&lane(&f, clip), 4.0),
        Some(0.5),
        "other fade untouched"
    );
    f.engine.set_clip_fade(clip, FadeSide::Out, 0.0).unwrap();
    assert!(lane(&f, clip).is_empty(), "no automation left behind");

    f.engine.undo().unwrap();
    assert_eq!(fade_out_duration(&lane(&f, clip), 4.0), Some(0.5));
}

#[test]
fn fade_clamps_against_the_clip_and_the_opposite_fade() {
    let (mut f, clip) = with_audio_clip();
    let applied = f.engine.set_clip_fade(clip, FadeSide::In, 99.0).unwrap();
    assert_eq!(applied, 4.0, "clamped to the clip duration");
    let applied = f.engine.set_clip_fade(clip, FadeSide::Out, 2.0).unwrap();
    assert!(
        applied < 2e-3,
        "no room left: fade-out removed, got {applied}"
    );

    f.engine.set_clip_fade(clip, FadeSide::In, 3.0).unwrap();
    let applied = f.engine.set_clip_fade(clip, FadeSide::Out, 2.0).unwrap();
    assert!(
        applied <= 1.0 - KEYFRAME_MIN_DT + 1e-12,
        "clamped to the fade-in's end: {applied}"
    );
}

// ---------------------------------------------------------------------
// Extract audio
// ---------------------------------------------------------------------

#[test]
fn extract_audio_copies_the_window_and_mutes_the_video_clip() {
    let mut f = engine_with_media();
    let video_clip = f.engine.add_clip(f.video, f.media, 1.0, 2.0, 6.0).unwrap();
    f.engine.set_clip_volume(video_clip, 0.8).unwrap();
    f.engine
        .set_clip_fade(video_clip, FadeSide::In, 1.0)
        .unwrap();
    let before = serialized(&f.engine);

    let audio_clip = f.engine.extract_audio(video_clip).unwrap();

    let (track, extracted) = f.engine.project().find_clip(audio_clip).unwrap();
    assert_eq!(track.kind, TrackKind::Audio);
    assert_eq!(extracted.media_id, Some(f.media));
    assert_eq!(
        (extracted.timeline_in, extracted.timeline_out),
        (1.0, 5.0),
        "same timeline window"
    );
    assert_eq!(
        (extracted.source_in, extracted.source_out),
        (2.0, 6.0),
        "same source range"
    );
    assert_eq!(extracted.volume, 0.8, "gain carried over");
    assert_eq!(
        fade_in_duration(extracted.keyframes.get(&VOL).unwrap()),
        Some(1.0),
        "automation carried over"
    );
    let (_, video) = f.engine.project().find_clip(video_clip).unwrap();
    assert_eq!(video.volume, 0.0, "video clip muted");

    // Deleting the video clip leaves working audio.
    f.engine.delete_clip(video_clip).unwrap();
    assert!(f.engine.project().find_clip(audio_clip).is_some());

    // The extraction itself was exactly one undo entry.
    f.engine.undo().unwrap(); // the delete
    f.engine.undo().unwrap(); // the extraction
    assert_eq!(serialized(&f.engine), before);
}

#[test]
fn extract_audio_creates_a_lane_when_audio_tracks_are_busy() {
    let mut f = engine_with_media();
    let video_clip = f.engine.add_clip(f.video, f.media, 0.0, 0.0, 4.0).unwrap();
    // Occupy the only audio track across the same span.
    f.engine.add_clip(f.audio, f.media, 0.0, 0.0, 5.0).unwrap();
    let tracks_before = f.engine.project().tracks.len();

    let audio_clip = f.engine.extract_audio(video_clip).unwrap();

    let project = f.engine.project();
    assert_eq!(project.tracks.len(), tracks_before + 1, "new lane created");
    let last = project.tracks.last().unwrap();
    assert_eq!(last.kind, TrackKind::Audio, "created at the panel bottom");
    assert!(last.clip(audio_clip).is_some());

    // One undo removes the clip, the mute, and the created track.
    f.engine.undo().unwrap();
    assert_eq!(f.engine.project().tracks.len(), tracks_before);
}

#[test]
fn extract_audio_rejects_unsuitable_clips() {
    let mut f = engine_with_media();
    let silent = f
        .engine
        .add_media("/tmp/mute.mp4", MEDIA_DUR, true, false)
        .unwrap();
    let no_audio_clip = f.engine.add_clip(f.video, silent, 0.0, 0.0, 2.0).unwrap();
    assert!(matches!(
        f.engine.extract_audio(no_audio_clip),
        Err(EngineError::ExtractAudio { .. })
    ));

    let music = f.engine.add_clip(f.audio, f.media, 0.0, 0.0, 2.0).unwrap();
    assert!(matches!(
        f.engine.extract_audio(music),
        Err(EngineError::ExtractAudio { .. })
    ));
}

// ---------------------------------------------------------------------
// Persistence & validation
// ---------------------------------------------------------------------

#[test]
fn keyframed_projects_round_trip_through_serde() {
    let (mut f, clip) = with_audio_clip();
    f.engine
        .add_keyframe(clip, VOL, 1.0, 0.5, Easing::EaseInOut)
        .unwrap();
    f.engine.set_clip_fade(clip, FadeSide::Out, 1.0).unwrap();

    let json = serialized(&f.engine);
    let reloaded: cutty_engine::Project = serde_json::from_str(&json).unwrap();
    reloaded.validate().unwrap();
    assert_eq!(&reloaded, f.engine.project());
    assert!(
        json.contains("\"easeInOut\""),
        "easing serializes camelCase"
    );
}

#[test]
fn unanimated_clips_serialize_without_a_keyframes_key() {
    let (f, _) = with_audio_clip();
    assert!(
        !serialized(&f.engine).contains("keyframes"),
        "additive schema field stays absent when unused"
    );
}

#[test]
fn validation_rejects_malformed_lanes() {
    let (mut f, clip) = with_audio_clip();
    f.engine
        .add_keyframe(clip, VOL, 1.0, 0.5, Easing::Linear)
        .unwrap();
    let mut project = f.engine.project().clone();

    // Hand-corrupt the lane: out-of-range time.
    let lane = project
        .tracks
        .iter_mut()
        .flat_map(|t| t.clips.iter_mut())
        .find(|c| c.id == clip)
        .unwrap()
        .keyframes
        .get_mut(&VOL)
        .unwrap();
    lane[0].t = 100.0;
    assert!(project.validate().is_err());
}
