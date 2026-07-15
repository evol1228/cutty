//! Integration tests for track management (add/remove/reorder, flags),
//! lock enforcement, cross-track clip moves, and the per-clip video
//! properties (transform, opacity, blend mode).

mod common;

use common::{engine_with_media, serialized, track_of_kind, Fixture};
use cutty_engine::{
    resolve_video_layers, BlendMode, Engine, EngineError, ProjectSettings, TrackFlag, TrackId,
    TrackKind, Transform,
};

// ---------------------------------------------------------------------
// Add / remove / reorder tracks
// ---------------------------------------------------------------------

#[test]
fn add_track_inserts_at_index_with_generated_name() {
    let Fixture { mut engine, .. } = engine_with_media();
    let v2 = engine.add_track(TrackKind::Video, 0).expect("add V2");
    let a2 = engine.add_track(TrackKind::Audio, 99).expect("add A2");

    let tracks = &engine.project().tracks;
    assert_eq!(tracks.len(), 4);
    assert_eq!(tracks[0].id, v2, "inserted at the top");
    assert_eq!(tracks[0].name, "V2");
    assert_eq!(tracks[3].id, a2, "out-of-range index clamps to the end");
    assert_eq!(tracks[3].name, "A2");
    assert!(!tracks[0].locked && !tracks[0].muted && !tracks[0].hidden);
}

#[test]
fn add_track_undo_redo_round_trips() {
    let Fixture { mut engine, .. } = engine_with_media();
    let before = serialized(&engine);
    engine.add_track(TrackKind::Video, 0).expect("add");
    let after = serialized(&engine);

    assert!(engine.undo().expect("undo"));
    assert_eq!(serialized(&engine), before);
    assert!(engine.redo().expect("redo"));
    assert_eq!(serialized(&engine), after);
}

#[test]
fn track_names_never_collide_after_removal() {
    let Fixture { mut engine, .. } = engine_with_media();
    let v2 = engine.add_track(TrackKind::Video, 0).expect("V2");
    let v3 = engine.add_track(TrackKind::Video, 0).expect("V3");
    assert_eq!(engine.project().track(v3).unwrap().name, "V3");
    engine.remove_track(v2).expect("remove V2");
    // Highest existing is V3, so the next is V4 — not a second "V3".
    let v4 = engine.add_track(TrackKind::Video, 0).expect("V4");
    assert_eq!(engine.project().track(v4).unwrap().name, "V4");
}

#[test]
fn remove_track_restores_clips_verbatim_on_undo() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    let v2 = engine.add_track(TrackKind::Video, 0).expect("V2");
    engine.add_clip(v2, media, 1.0, 0.0, 3.0).expect("clip");
    engine.add_clip(v2, media, 5.0, 2.0, 4.0).expect("clip");
    let with_track = serialized(&engine);

    engine.remove_track(v2).expect("remove");
    assert!(engine.project().track(v2).is_none());
    assert_eq!(engine.project().track(video).unwrap().clips.len(), 0);

    assert!(engine.undo().expect("undo"));
    assert_eq!(serialized(&engine), with_track, "clips back verbatim");
}

#[test]
fn removing_the_last_track_of_a_kind_is_rejected() {
    let Fixture {
        mut engine,
        video,
        audio,
        ..
    } = engine_with_media();
    let err = engine.remove_track(video).unwrap_err();
    assert!(
        matches!(err, EngineError::LastTrackOfKind { .. }),
        "{err:?}"
    );
    let err = engine.remove_track(audio).unwrap_err();
    assert!(
        matches!(err, EngineError::LastTrackOfKind { .. }),
        "{err:?}"
    );

    // With a second video track present, removal works — down to one.
    let v2 = engine.add_track(TrackKind::Video, 0).expect("V2");
    engine.remove_track(v2).expect("no longer the last");
    let err = engine.remove_track(video).unwrap_err();
    assert!(
        matches!(err, EngineError::LastTrackOfKind { .. }),
        "{err:?}"
    );
}

#[test]
fn move_track_restacks_the_composite_and_round_trips() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    let v2 = engine.add_track(TrackKind::Video, 0).expect("V2");
    let base = engine.add_clip(video, media, 0.0, 0.0, 4.0).expect("base");
    let over = engine.add_clip(v2, media, 0.0, 4.0, 8.0).expect("overlay");

    // Panel order [V2, V1, A1] → composite bottom→top is [base, over].
    let layers = resolve_video_layers(engine.project(), 1.0);
    assert_eq!(
        layers.iter().map(|l| l.clip_id).collect::<Vec<_>>(),
        vec![base, over]
    );

    // Move V2 below V1 (panel index 1): stacking flips.
    engine.move_track(v2, 1).expect("move");
    let layers = resolve_video_layers(engine.project(), 1.0);
    assert_eq!(
        layers.iter().map(|l| l.clip_id).collect::<Vec<_>>(),
        vec![over, base]
    );

    assert!(engine.undo().expect("undo"));
    let layers = resolve_video_layers(engine.project(), 1.0);
    assert_eq!(
        layers.iter().map(|l| l.clip_id).collect::<Vec<_>>(),
        vec![base, over]
    );
}

#[test]
fn move_track_clamps_and_ignores_no_ops() {
    let Fixture {
        mut engine, video, ..
    } = engine_with_media();
    let depth = engine.undo_depth();
    engine.move_track(video, 0).expect("no-op");
    assert_eq!(engine.undo_depth(), depth, "no-op adds no undo entry");

    engine.move_track(video, 999).expect("clamped to the end");
    assert_eq!(engine.project().tracks.last().unwrap().id, video);

    let err = engine.move_track(TrackId(12345), 0).unwrap_err();
    assert!(matches!(err, EngineError::UnknownTrack(_)), "{err:?}");
}

// ---------------------------------------------------------------------
// Track flags
// ---------------------------------------------------------------------

#[test]
fn track_flags_toggle_and_round_trip() {
    let Fixture {
        mut engine, video, ..
    } = engine_with_media();
    for flag in [TrackFlag::Locked, TrackFlag::Muted, TrackFlag::Hidden] {
        engine.set_track_flag(video, flag, true).expect("set");
    }
    let track = engine.project().track(video).unwrap();
    assert!(track.locked && track.muted && track.hidden);

    // Three commands → three undo steps, each restoring one flag.
    assert!(engine.undo().expect("undo hidden"));
    assert!(!engine.project().track(video).unwrap().hidden);
    assert!(engine.undo().expect("undo muted"));
    assert!(!engine.project().track(video).unwrap().muted);
    assert!(engine.undo().expect("undo locked"));
    assert!(!engine.project().track(video).unwrap().locked);
}

#[test]
fn setting_a_flag_to_its_current_value_is_a_no_op() {
    let Fixture {
        mut engine, video, ..
    } = engine_with_media();
    let depth = engine.undo_depth();
    engine
        .set_track_flag(video, TrackFlag::Muted, false)
        .expect("no-op");
    assert_eq!(engine.undo_depth(), depth);
}

// ---------------------------------------------------------------------
// Lock enforcement — every clip edit is rejected on a locked track
// ---------------------------------------------------------------------

#[test]
fn locked_track_rejects_every_clip_edit() {
    let Fixture {
        mut engine,
        media,
        video,
        audio,
    } = engine_with_media();
    let clip = engine.add_clip(video, media, 0.0, 0.0, 4.0).expect("clip");
    let v2 = engine.add_track(TrackKind::Video, 0).expect("V2");
    engine
        .set_track_flag(video, TrackFlag::Locked, true)
        .expect("lock");
    let before = serialized(&engine);
    let depth = engine.undo_depth();
    engine.drain_events();

    let locked = |err: EngineError| {
        assert!(matches!(err, EngineError::TrackLocked { .. }), "{err:?}");
    };
    locked(engine.add_clip(video, media, 5.0, 0.0, 2.0).unwrap_err());
    locked(engine.move_clip(clip, 1.0).unwrap_err());
    locked(engine.move_clip_to_track(clip, v2, 0.0).unwrap_err());
    locked(
        engine
            .trim_clip(clip, cutty_engine::TrimEdge::End, 3.0)
            .unwrap_err(),
    );
    locked(engine.split_clip(clip, 2.0).unwrap_err());
    locked(engine.delete_clip(clip).unwrap_err());
    locked(engine.ripple_delete(clip).unwrap_err());
    locked(engine.set_clip_volume(clip, 0.5).unwrap_err());
    locked(
        engine
            .set_clip_transform(clip, Transform::default())
            .unwrap_err(),
    );
    locked(engine.set_clip_opacity(clip, 0.5).unwrap_err());
    locked(
        engine
            .set_clip_blend_mode(clip, BlendMode::Screen)
            .unwrap_err(),
    );
    // Removing media would delete the locked track's clip: rejected too.
    locked(engine.remove_media(media).unwrap_err());
    // Removing the locked track itself: rejected (unlock first). V2
    // exists, so this isn't the last-of-kind rejection.
    locked(engine.remove_track(video).unwrap_err());

    assert_eq!(serialized(&engine), before, "state untouched");
    assert_eq!(engine.undo_depth(), depth);
    assert!(engine.drain_events().is_empty(), "no events on rejection");

    // A locked *destination* rejects incoming clips as well.
    let a2 = engine.add_track(TrackKind::Audio, 99).expect("A2");
    let aclip = engine.add_clip(audio, media, 0.0, 0.0, 2.0).expect("a");
    engine
        .set_track_flag(a2, TrackFlag::Locked, true)
        .expect("lock A2");
    locked(engine.move_clip_to_track(aclip, a2, 0.0).unwrap_err());
}

#[test]
fn undo_crosses_locked_tracks() {
    // Lock protects against new edits; undo/redo restore history freely.
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    let clip = engine.add_clip(video, media, 0.0, 0.0, 4.0).expect("clip");
    engine.move_clip(clip, 2.0).expect("move");
    engine
        .set_track_flag(video, TrackFlag::Locked, true)
        .expect("lock");

    assert!(engine.undo().expect("undo the lock"));
    assert!(engine.undo().expect("undo the move — through the lock"));
    let (_, c) = engine.project().find_clip(clip).unwrap();
    assert_eq!(c.timeline_in, 0.0);
}

// ---------------------------------------------------------------------
// Cross-track clip moves
// ---------------------------------------------------------------------

#[test]
fn move_clip_to_track_carries_position_and_round_trips() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    let v2 = engine.add_track(TrackKind::Video, 0).expect("V2");
    let clip = engine.add_clip(video, media, 1.0, 0.0, 4.0).expect("clip");
    let before = serialized(&engine);

    engine.move_clip_to_track(clip, v2, 3.5).expect("move");
    let (track, moved) = engine.project().find_clip(clip).expect("clip");
    assert_eq!(track.id, v2);
    assert_eq!(moved.timeline_in, 3.5);
    assert_eq!(moved.timeline_out, 7.5, "duration preserved");
    assert_eq!((moved.source_in, moved.source_out), (0.0, 4.0));

    assert!(engine.undo().expect("undo"));
    assert_eq!(serialized(&engine), before, "one entry restores both axes");
}

#[test]
fn move_clip_to_same_track_is_a_plain_move() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    let clip = engine.add_clip(video, media, 1.0, 0.0, 4.0).expect("clip");
    engine.move_clip_to_track(clip, video, 2.0).expect("move");
    let (track, moved) = engine.project().find_clip(clip).expect("clip");
    assert_eq!(track.id, video);
    assert_eq!(moved.timeline_in, 2.0);
}

#[test]
fn move_clip_to_track_rejects_overlap_and_keeps_state() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    let v2 = engine.add_track(TrackKind::Video, 0).expect("V2");
    engine.add_clip(v2, media, 0.0, 0.0, 4.0).expect("occupant");
    let clip = engine.add_clip(video, media, 0.0, 0.0, 4.0).expect("mover");
    let before = serialized(&engine);

    let err = engine.move_clip_to_track(clip, v2, 2.0).unwrap_err();
    assert!(matches!(err, EngineError::ClipOverlap { .. }), "{err:?}");
    assert_eq!(serialized(&engine), before);
}

#[test]
fn move_clip_to_kind_incompatible_track_is_rejected() {
    let mut engine = Engine::new(ProjectSettings::default());
    let video_only = engine
        .add_media("/tmp/video-only.mp4", 10.0, true, false)
        .expect("media");
    let video = track_of_kind(&engine, TrackKind::Video);
    let audio = track_of_kind(&engine, TrackKind::Audio);
    let clip = engine
        .add_clip(video, video_only, 0.0, 0.0, 4.0)
        .expect("clip");

    let err = engine.move_clip_to_track(clip, audio, 0.0).unwrap_err();
    assert!(
        matches!(err, EngineError::IncompatibleMedia { .. }),
        "{err:?}"
    );
    // Still on the video track, untouched.
    let (track, _) = engine.project().find_clip(clip).expect("clip");
    assert_eq!(track.id, video);
}

// ---------------------------------------------------------------------
// Clip video properties: transform, opacity, blend mode
// ---------------------------------------------------------------------

#[test]
fn set_transform_applies_and_undoes_exactly() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    let clip = engine.add_clip(video, media, 0.0, 0.0, 4.0).expect("clip");
    let before = serialized(&engine);
    let t = Transform {
        x: 123.25,
        y: -48.5,
        scale: 0.37,
        rotation: 22.5,
    };
    engine.set_clip_transform(clip, t.clone()).expect("set");
    let (_, c) = engine.project().find_clip(clip).unwrap();
    assert_eq!(c.transform, t);

    assert!(engine.undo().expect("undo"));
    assert_eq!(serialized(&engine), before, "exact floats restored");
}

#[test]
fn set_transform_rejects_non_finite_and_non_positive_scale() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    let clip = engine.add_clip(video, media, 0.0, 0.0, 4.0).expect("clip");
    let bad = |t: Transform, engine: &mut Engine| {
        let err = engine.set_clip_transform(clip, t).unwrap_err();
        assert!(
            matches!(err, EngineError::InvalidProperty { .. }),
            "{err:?}"
        );
    };
    bad(
        Transform {
            x: f64::NAN,
            ..Transform::default()
        },
        &mut engine,
    );
    bad(
        Transform {
            rotation: f64::INFINITY,
            ..Transform::default()
        },
        &mut engine,
    );
    bad(
        Transform {
            scale: 0.0,
            ..Transform::default()
        },
        &mut engine,
    );
    bad(
        Transform {
            scale: -1.0,
            ..Transform::default()
        },
        &mut engine,
    );
}

#[test]
fn set_opacity_validates_range_and_round_trips() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    let clip = engine.add_clip(video, media, 0.0, 0.0, 4.0).expect("clip");
    engine.set_clip_opacity(clip, 0.35).expect("set");
    assert_eq!(engine.project().find_clip(clip).unwrap().1.opacity, 0.35);

    for bad in [-0.1, 1.1, f64::NAN] {
        let err = engine.set_clip_opacity(clip, bad).unwrap_err();
        assert!(
            matches!(err, EngineError::InvalidProperty { .. }),
            "{err:?}"
        );
    }

    assert!(engine.undo().expect("undo"));
    assert_eq!(engine.project().find_clip(clip).unwrap().1.opacity, 1.0);
}

#[test]
fn set_blend_mode_round_trips() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    let clip = engine.add_clip(video, media, 0.0, 0.0, 4.0).expect("clip");
    engine
        .set_clip_blend_mode(clip, BlendMode::Multiply)
        .expect("set");
    assert_eq!(
        engine.project().find_clip(clip).unwrap().1.blend_mode,
        BlendMode::Multiply
    );
    assert!(engine.undo().expect("undo"));
    assert_eq!(
        engine.project().find_clip(clip).unwrap().1.blend_mode,
        BlendMode::Normal
    );
}

#[test]
fn gizmo_drag_transaction_is_one_undo_entry() {
    // The gizmo/Inspector stream many transient property changes inside
    // a transaction; the commit must fold them into exactly one entry.
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    let clip = engine.add_clip(video, media, 0.0, 0.0, 4.0).expect("clip");
    let before = serialized(&engine);
    let depth = engine.undo_depth();

    engine.begin_transaction().expect("begin");
    for i in 1..=20 {
        engine
            .set_clip_transform(
                clip,
                Transform {
                    x: f64::from(i) * 5.0,
                    y: f64::from(i) * -2.0,
                    scale: 1.0 + f64::from(i) * 0.01,
                    rotation: f64::from(i),
                },
            )
            .expect("transient");
        engine
            .set_clip_opacity(clip, 1.0 - f64::from(i) * 0.01)
            .expect("transient");
    }
    engine.commit_transaction().expect("commit");

    assert_eq!(engine.undo_depth(), depth + 1, "exactly one undo entry");
    assert!(engine.undo().expect("undo"));
    assert_eq!(serialized(&engine), before, "whole gesture reverted");
}

// ---------------------------------------------------------------------
// Persistence of the new field
// ---------------------------------------------------------------------

#[test]
fn hidden_flag_survives_serialization() {
    let Fixture {
        mut engine, video, ..
    } = engine_with_media();
    engine
        .set_track_flag(video, TrackFlag::Hidden, true)
        .expect("hide");
    let json = cutty_engine::project_file::serialize(engine.project(), None);
    let loaded = cutty_engine::project_file::deserialize(&json, None).expect("loads");
    assert!(loaded.track(video).unwrap().hidden);
}
