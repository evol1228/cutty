//! Text clips: placement (auto-lane creation), full timeline editing
//! (trim/move/split behave; split duplicates content), payload edits,
//! validation, resolution order, and the v2 project-file story.

mod common;

use common::{engine_with_media, span};
use cutty_engine::{
    project_file, resolve_text_layers, ClipId, Engine, EngineError, ProjectSettings, TextAlign,
    TextSpec, TextStyle, TrackId, TrackKind, Transform, Transition, TrimEdge, MIN_CLIP_DURATION,
};

fn text(content: &str) -> TextSpec {
    TextSpec {
        content: content.into(),
        style: TextStyle::default(),
    }
}

fn add_text(engine: &mut Engine, at: f64, dur: f64, content: &str) -> ClipId {
    engine
        .add_text_clip(at, dur, text(content), Transform::default(), None)
        .expect("add text clip")
}

/// The track currently holding `clip`.
fn track_of(engine: &Engine, clip: ClipId) -> TrackId {
    engine.project().find_clip(clip).expect("clip").0.id
}

// ---------------------------------------------------------------------
// Placement
// ---------------------------------------------------------------------

#[test]
fn first_text_clip_creates_a_text_lane_on_top_as_one_undo_step() {
    let mut f = engine_with_media();
    assert!(
        !f.engine
            .project()
            .tracks
            .iter()
            .any(|t| t.kind == TrackKind::Text),
        "fresh projects have no text lane"
    );

    let clip = add_text(&mut f.engine, 1.0, 3.0, "Hello");
    let project = f.engine.project();
    assert_eq!(project.tracks[0].kind, TrackKind::Text, "created on top");
    assert_eq!(project.tracks[0].name, "T1");
    assert_eq!(project.tracks[0].clips.len(), 1);
    let stored = &project.tracks[0].clips[0];
    assert_eq!(stored.id, clip);
    assert_eq!(stored.media_id, None);
    assert_eq!(
        stored.text.as_ref().map(|t| t.content.as_str()),
        Some("Hello")
    );
    assert_eq!(span(&f.engine, clip), (1.0, 4.0, 0.0, 3.0));

    // Track + clip are one undo entry: undo removes both.
    assert_eq!(f.engine.undo_depth(), 1);
    f.engine.undo().expect("undo");
    assert!(
        !f.engine
            .project()
            .tracks
            .iter()
            .any(|t| t.kind == TrackKind::Text),
        "undo removes the auto-created lane"
    );
    f.engine.redo().expect("redo");
    assert_eq!(f.engine.project().tracks[0].clips.len(), 1);
}

#[test]
fn overlapping_add_stacks_a_new_lane_above() {
    let mut f = engine_with_media();
    let a = add_text(&mut f.engine, 0.0, 3.0, "one");
    let b = add_text(&mut f.engine, 1.0, 3.0, "two");
    assert_ne!(track_of(&f.engine, a), track_of(&f.engine, b));
    let top_lane = {
        let project = f.engine.project();
        assert_eq!(project.tracks[0].kind, TrackKind::Text);
        assert_eq!(project.tracks[1].kind, TrackKind::Text);
        assert_eq!(project.tracks[0].name, "T2", "new lane stacks on top");
        project.tracks[0].id
    };

    // A third at a free spot reuses the topmost lane with room.
    let c = add_text(&mut f.engine, 10.0, 2.0, "three");
    assert_eq!(track_of(&f.engine, c), top_lane);
}

#[test]
fn explicit_track_placement_validates_kind_and_lock() {
    let mut f = engine_with_media();
    let err = f
        .engine
        .add_text_clip(0.0, 2.0, text("x"), Transform::default(), Some(f.video))
        .unwrap_err();
    assert!(matches!(err, EngineError::InvalidText { .. }), "{err}");

    let clip = add_text(&mut f.engine, 0.0, 2.0, "x");
    let lane = track_of(&f.engine, clip);
    f.engine
        .set_track_flag(lane, cutty_engine::TrackFlag::Locked, true)
        .expect("lock");
    let err = f
        .engine
        .add_text_clip(5.0, 2.0, text("y"), Transform::default(), Some(lane))
        .unwrap_err();
    assert!(matches!(err, EngineError::TrackLocked { .. }), "{err}");
    // Auto-placement skips the locked lane and creates a fresh one.
    let c2 = add_text(&mut f.engine, 5.0, 2.0, "y");
    assert_ne!(track_of(&f.engine, c2), lane);
}

#[test]
fn add_text_clip_rejects_degenerate_spans() {
    let mut f = engine_with_media();
    for (at, dur) in [
        (0.0, 0.0),
        (0.0, MIN_CLIP_DURATION / 2.0),
        (f64::NAN, 1.0),
        (0.0, f64::INFINITY),
    ] {
        let err = f
            .engine
            .add_text_clip(at, dur, text("x"), Transform::default(), None)
            .unwrap_err();
        assert!(matches!(err, EngineError::InvalidTimeRange { .. }), "{err}");
    }
    // Negative start clamps to 0 (matching clip moves).
    let clip = f
        .engine
        .add_text_clip(-3.0, 2.0, text("x"), Transform::default(), None)
        .expect("clamped add");
    assert_eq!(span(&f.engine, clip).0, 0.0);
}

// ---------------------------------------------------------------------
// Timeline editing
// ---------------------------------------------------------------------

#[test]
fn text_clips_trim_freely_in_both_directions() {
    let mut f = engine_with_media();
    let clip = add_text(&mut f.engine, 5.0, 3.0, "trim me");

    // Extend the right edge far past the original duration — no media
    // bound applies.
    let applied = f
        .engine
        .trim_clip(clip, TrimEdge::End, 30.0)
        .expect("extend right");
    assert_eq!(applied, 30.0);
    assert_eq!(span(&f.engine, clip), (5.0, 30.0, 0.0, 25.0));

    // Extend the left edge back to the timeline start.
    let applied = f
        .engine
        .trim_clip(clip, TrimEdge::Start, -2.0)
        .expect("extend left clamps at 0");
    assert_eq!(applied, 0.0);
    assert_eq!(span(&f.engine, clip), (0.0, 30.0, 0.0, 30.0));

    // Shrink below the minimum duration clamps, never inverts.
    let applied = f
        .engine
        .trim_clip(clip, TrimEdge::Start, 40.0)
        .expect("shrink clamps");
    assert!(applied < 30.0);
    let (t_in, t_out, s_in, s_out) = span(&f.engine, clip);
    assert!(t_out - t_in >= MIN_CLIP_DURATION - 1e-12);
    assert_eq!(s_in, 0.0, "source stays normalized");
    assert!((s_out - (t_out - t_in)).abs() < 1e-9);

    // Undo restores the exact original floats.
    while f.engine.undo_depth() > 1 {
        f.engine.undo().expect("undo");
    }
    assert_eq!(span(&f.engine, clip), (5.0, 8.0, 0.0, 3.0));
}

#[test]
fn split_duplicates_content_and_undo_rejoins() {
    let mut f = engine_with_media();
    let clip = add_text(&mut f.engine, 2.0, 4.0, "same text");
    let right = f.engine.split_clip(clip, 3.5).expect("split");

    assert_eq!(span(&f.engine, clip), (2.0, 3.5, 0.0, 1.5));
    assert_eq!(span(&f.engine, right), (3.5, 6.0, 0.0, 2.5));
    let project = f.engine.project();
    let (_, left) = project.find_clip(clip).expect("left");
    let (_, right_clip) = project.find_clip(right).expect("right");
    assert_eq!(
        left.text.as_ref().map(|t| t.content.as_str()),
        Some("same text")
    );
    assert_eq!(left.text, right_clip.text, "split duplicates the payload");
    assert_eq!(right_clip.media_id, None);

    f.engine.undo().expect("undo split");
    assert_eq!(span(&f.engine, clip), (2.0, 6.0, 0.0, 4.0));
    assert!(f.engine.project().find_clip(right).is_none());
}

#[test]
fn move_keeps_lanes_kind_pure() {
    let mut f = engine_with_media();
    let video_clip = f
        .engine
        .add_clip(f.video, f.media, 0.0, 0.0, 4.0)
        .expect("video clip");
    let text_clip = add_text(&mut f.engine, 0.0, 2.0, "x");
    let text_lane = track_of(&f.engine, text_clip);

    // Text clip onto a video track: validation rejects, state unchanged.
    let err = f
        .engine
        .move_clip_to_track(text_clip, f.video, 6.0)
        .unwrap_err();
    assert!(matches!(err, EngineError::InvalidText { .. }), "{err}");
    assert_eq!(track_of(&f.engine, text_clip), text_lane);

    // Media clip onto the text lane: same story.
    let err = f
        .engine
        .move_clip_to_track(video_clip, text_lane, 6.0)
        .unwrap_err();
    assert!(matches!(err, EngineError::InvalidText { .. }), "{err}");

    // Text → text lane moves fine (also plain time moves).
    let second = add_text(&mut f.engine, 5.0, 2.0, "y");
    let second_lane = track_of(&f.engine, second);
    f.engine.move_clip(text_clip, 9.0).expect("time move");
    assert_eq!(span(&f.engine, text_clip).0, 9.0);
    if second_lane != text_lane {
        f.engine
            .move_clip_to_track(second, text_lane, 0.0)
            .expect("lane move");
        assert_eq!(track_of(&f.engine, second), text_lane);
    }
}

#[test]
fn delete_and_ripple_work_on_text_lanes() {
    let mut f = engine_with_media();
    let a = add_text(&mut f.engine, 0.0, 2.0, "a");
    let lane = track_of(&f.engine, a);
    let b = f
        .engine
        .add_text_clip(2.0, 2.0, text("b"), Transform::default(), Some(lane))
        .expect("b");
    let c = f
        .engine
        .add_text_clip(5.0, 2.0, text("c"), Transform::default(), Some(lane))
        .expect("c");

    f.engine.ripple_delete(b).expect("ripple");
    assert!(f.engine.project().find_clip(b).is_none());
    assert_eq!(span(&f.engine, c).0, 3.0, "later clip closed the gap");

    f.engine.delete_clip(a).expect("delete");
    assert!(f.engine.project().find_clip(a).is_none());
}

#[test]
fn transitions_are_rejected_on_text_clips() {
    let mut f = engine_with_media();
    let a = add_text(&mut f.engine, 0.0, 2.0, "a");
    let lane = track_of(&f.engine, a);
    f.engine
        .add_text_clip(2.0, 2.0, text("b"), Transform::default(), Some(lane))
        .expect("touching next");
    let err = f
        .engine
        .set_transition(
            a,
            Some(Transition {
                kind: "fade".into(),
                duration: 0.5,
            }),
        )
        .unwrap_err();
    assert!(
        matches!(err, EngineError::InvalidTransition { .. }),
        "{err}"
    );
}

// ---------------------------------------------------------------------
// Payload edits
// ---------------------------------------------------------------------

#[test]
fn set_clip_text_round_trips_with_undo_and_skips_noops() {
    let mut f = engine_with_media();
    let clip = add_text(&mut f.engine, 0.0, 2.0, "before");
    let depth = f.engine.undo_depth();

    let mut edited = text("after");
    edited.style.fill = "#ffdd00".into();
    edited.style.align = TextAlign::Left;
    f.engine
        .set_clip_text(clip, edited.clone())
        .expect("edit text");
    assert_eq!(f.engine.undo_depth(), depth + 1);
    let (_, stored) = f.engine.project().find_clip(clip).expect("clip");
    assert_eq!(stored.text.as_ref(), Some(&edited));

    // Same payload again: no state change, no undo entry.
    f.engine.set_clip_text(clip, edited).expect("no-op");
    assert_eq!(f.engine.undo_depth(), depth + 1);

    f.engine.undo().expect("undo");
    let (_, stored) = f.engine.project().find_clip(clip).expect("clip");
    assert_eq!(
        stored.text.as_ref().map(|t| t.content.as_str()),
        Some("before")
    );
}

#[test]
fn set_clip_text_rejects_media_clips_and_validates_payloads() {
    let mut f = engine_with_media();
    let video_clip = f
        .engine
        .add_clip(f.video, f.media, 0.0, 0.0, 2.0)
        .expect("video clip");
    let err = f.engine.set_clip_text(video_clip, text("x")).unwrap_err();
    assert!(matches!(err, EngineError::InvalidText { .. }), "{err}");

    let clip = add_text(&mut f.engine, 3.0, 2.0, "ok");
    let cases: Vec<TextSpec> = vec![
        {
            let mut t = text("bad fill");
            t.style.fill = "red".into();
            t
        },
        {
            let mut t = text("bad size");
            t.style.font_size = 0.0;
            t
        },
        {
            let mut t = text("bad weight");
            t.style.weight = 50;
            t
        },
        {
            let mut t = text("bad alpha");
            t.style.shadow_alpha = 2.0;
            t
        },
        text(&"x".repeat(cutty_engine::MAX_TEXT_CONTENT_BYTES + 1)),
    ];
    for bad in cases {
        let before = common::serialized(&f.engine);
        let err = f.engine.set_clip_text(clip, bad).unwrap_err();
        assert!(
            matches!(
                err,
                EngineError::InvalidText { .. } | EngineError::InvalidProperty { .. }
            ),
            "{err}"
        );
        assert_eq!(common::serialized(&f.engine), before, "state untouched");
    }
}

// ---------------------------------------------------------------------
// Tracks, resolution, persistence
// ---------------------------------------------------------------------

#[test]
fn last_text_track_is_removable_unlike_video_and_audio() {
    let mut f = engine_with_media();
    let clip = add_text(&mut f.engine, 0.0, 2.0, "x");
    let lane = track_of(&f.engine, clip);
    f.engine.remove_track(lane).expect("text lane removable");
    assert!(!f
        .engine
        .project()
        .tracks
        .iter()
        .any(|t| t.kind == TrackKind::Text));
    let err = f.engine.remove_track(f.video).unwrap_err();
    assert!(matches!(err, EngineError::LastTrackOfKind { .. }));

    // Undo restores the lane with its clip.
    f.engine.undo().expect("undo remove");
    assert!(f.engine.project().find_clip(clip).is_some());
}

#[test]
fn text_layers_resolve_above_video_in_panel_order_and_respect_hidden() {
    let mut f = engine_with_media();
    let a = add_text(&mut f.engine, 0.0, 4.0, "bottom lane");
    let b = add_text(&mut f.engine, 1.0, 2.0, "top lane"); // overlaps → new lane above
    let lane_a = track_of(&f.engine, a);
    let lane_b = track_of(&f.engine, b);

    let layers = resolve_text_layers(f.engine.project(), 1.5);
    assert_eq!(
        layers.iter().map(|l| l.clip_id).collect::<Vec<_>>(),
        vec![a, b],
        "bottom (lower lane) first, exactly like video layers"
    );
    assert!((layers[0].source_time - 1.5).abs() < 1e-9);
    assert!((layers[1].source_time - 0.5).abs() < 1e-9);

    // Outside a clip: that lane contributes nothing.
    let layers = resolve_text_layers(f.engine.project(), 3.5);
    assert_eq!(
        layers.iter().map(|l| l.clip_id).collect::<Vec<_>>(),
        vec![a]
    );

    // Hidden text lanes drop out.
    f.engine
        .set_track_flag(lane_b, cutty_engine::TrackFlag::Hidden, true)
        .expect("hide");
    let layers = resolve_text_layers(f.engine.project(), 1.5);
    assert_eq!(
        layers.iter().map(|l| l.clip_id).collect::<Vec<_>>(),
        vec![a]
    );
    let _ = lane_a;
}

#[test]
fn text_projects_save_as_v2_and_round_trip() {
    let mut f = engine_with_media();
    let clip = add_text(&mut f.engine, 1.0, 3.0, "persist me");
    let json = project_file::serialize(f.engine.project(), None);
    assert!(json.contains("\"version\": 2"));
    assert!(json.contains("\"kind\": \"text\""));

    let loaded = project_file::deserialize(&json, None).expect("loads");
    assert_eq!(&loaded, f.engine.project());
    let (_, stored) = loaded.find_clip(clip).expect("clip survives");
    assert_eq!(
        stored.text.as_ref().map(|t| t.content.as_str()),
        Some("persist me")
    );
}

#[test]
fn projects_without_text_still_declare_v2() {
    // The writer stamps CURRENT_VERSION unconditionally — simpler than
    // feature-sniffing, and v2 is what this build reads best.
    let engine = Engine::new(ProjectSettings::default());
    let json = project_file::serialize(engine.project(), None);
    assert!(json.contains("\"version\": 2"));
    assert!(project_file::deserialize(&json, None).is_ok());
}

#[test]
fn validation_rejects_mismatched_payloads() {
    let mut f = engine_with_media();
    let text_clip = add_text(&mut f.engine, 0.0, 2.0, "x");
    let video_clip = f
        .engine
        .add_clip(f.video, f.media, 0.0, 0.0, 2.0)
        .expect("video clip");

    // Hand-corrupt clones of the project (bypassing the engine) and let
    // validate() catch each mismatch — this is the load-time guard for
    // hand-edited files.
    let base = f.engine.project().clone();

    let mut media_on_text = base.clone();
    let lane = track_of(&f.engine, text_clip);
    media_on_text
        .track(lane)
        .map(|t| t.id)
        .expect("text lane exists");
    for track in &mut media_on_text.tracks {
        if track.id == lane {
            track.clips[0].media_id = Some(f.media);
        }
    }
    assert!(matches!(
        media_on_text.validate(),
        Err(EngineError::InvalidText { .. })
    ));

    let mut text_on_video = base.clone();
    for track in &mut text_on_video.tracks {
        if track.kind == TrackKind::Video {
            track.clips[0].text = Some(text("nope"));
        }
    }
    assert!(matches!(
        text_on_video.validate(),
        Err(EngineError::InvalidText { .. })
    ));

    let mut neither = base.clone();
    for track in &mut neither.tracks {
        if track.kind == TrackKind::Video {
            track.clips[0].media_id = None;
        }
    }
    assert!(matches!(
        neither.validate(),
        Err(EngineError::InvalidText { .. })
    ));

    let mut denormalized = base.clone();
    for track in &mut denormalized.tracks {
        if track.kind == TrackKind::Text {
            track.clips[0].source_in = 0.5;
            track.clips[0].source_out = 2.5;
        }
    }
    assert!(matches!(
        denormalized.validate(),
        Err(EngineError::InvalidText { .. })
    ));
    let _ = video_clip;
}
