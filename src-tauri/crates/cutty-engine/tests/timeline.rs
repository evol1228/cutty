//! Integration tests for timeline operations, undo/redo, and transactions.

mod common;

use common::{engine_with_media, serialized, span, Fixture, MEDIA_DUR};
use cutty_engine::{ClipId, Engine, EngineError, EngineEvent, TrimEdge, MIN_CLIP_DURATION};

// ---------------------------------------------------------------------
// AddClip
// ---------------------------------------------------------------------

#[test]
fn add_clip_derives_timeline_out_and_defaults() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    let id = engine.add_clip(video, media, 1.0, 2.0, 8.0).expect("add");
    assert_eq!(span(&engine, id), (1.0, 7.0, 2.0, 8.0));
    let (_, clip) = engine.project().find_clip(id).expect("clip");
    assert_eq!(clip.speed, 1.0);
    assert_eq!(clip.opacity, 1.0);
    assert_eq!(clip.volume, 1.0);
    assert_eq!(clip.transform, cutty_engine::Transform::default());
}

#[test]
fn add_overlapping_clip_rejected_and_state_untouched() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    engine.add_clip(video, media, 0.0, 0.0, 4.0).expect("first");
    let before = serialized(&engine);
    let depth = engine.undo_depth();
    engine.drain_events();

    let err = engine.add_clip(video, media, 2.0, 0.0, 4.0).unwrap_err();
    assert!(matches!(err, EngineError::ClipOverlap { .. }), "{err:?}");
    assert_eq!(serialized(&engine), before);
    assert_eq!(engine.undo_depth(), depth);
    assert!(engine.drain_events().is_empty(), "failed op must not emit");
}

#[test]
fn add_touching_clips_allowed() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    engine.add_clip(video, media, 0.0, 0.0, 4.0).expect("first");
    engine
        .add_clip(video, media, 4.0, 4.0, 8.0)
        .expect("touching");
    assert_eq!(engine.project().track(video).unwrap().clips.len(), 2);
}

#[test]
fn add_clip_source_beyond_media_rejected() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    let err = engine
        .add_clip(video, media, 0.0, 5.0, MEDIA_DUR + 1.0)
        .unwrap_err();
    assert!(
        matches!(err, EngineError::SourceOutOfBounds { .. }),
        "{err:?}"
    );
    let err = engine.add_clip(video, media, 0.0, -1.0, 4.0).unwrap_err();
    assert!(
        matches!(err, EngineError::SourceOutOfBounds { .. }),
        "{err:?}"
    );
}

#[test]
fn add_clip_incompatible_track_kind_rejected() {
    let Fixture {
        mut engine,
        video,
        audio,
        ..
    } = engine_with_media();
    let audio_only = engine
        .add_media("/tmp/song.flac", 30.0, false, true)
        .expect("media");
    let err = engine
        .add_clip(video, audio_only, 0.0, 0.0, 5.0)
        .unwrap_err();
    assert!(
        matches!(err, EngineError::IncompatibleMedia { .. }),
        "{err:?}"
    );
    engine
        .add_clip(audio, audio_only, 0.0, 0.0, 5.0)
        .expect("audio media on audio track is fine");
}

#[test]
fn add_clip_negative_timeline_rejected() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    let err = engine.add_clip(video, media, -1.0, 0.0, 4.0).unwrap_err();
    assert!(
        matches!(err, EngineError::InvalidTimeRange { .. }),
        "{err:?}"
    );
}

// ---------------------------------------------------------------------
// SplitClip
// ---------------------------------------------------------------------

#[test]
fn split_middle_has_correct_source_math() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    // Timeline [1, 7) mapping to source [2, 8).
    let left_id = engine.add_clip(video, media, 1.0, 2.0, 8.0).expect("add");
    let right_id = engine.split_clip(left_id, 4.0).expect("split");

    assert_ne!(left_id, right_id);
    assert_eq!(span(&engine, left_id), (1.0, 4.0, 2.0, 5.0));
    assert_eq!(span(&engine, right_id), (4.0, 7.0, 5.0, 8.0));

    let (_, left) = engine.project().find_clip(left_id).expect("left");
    let (_, right) = engine.project().find_clip(right_id).expect("right");
    assert_eq!(left.media_id, right.media_id, "halves share the source");
    assert_eq!(left.speed, right.speed);
    assert_eq!(left.opacity, right.opacity);
}

#[test]
fn split_at_exact_clip_edges_rejected() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    let id = engine.add_clip(video, media, 1.0, 2.0, 8.0).expect("add");
    let before = serialized(&engine);

    for at in [
        1.0,                           // exact left edge
        7.0,                           // exact right edge
        1.0 + MIN_CLIP_DURATION / 2.0, // inside, but a sliver would remain
        7.0 - MIN_CLIP_DURATION / 2.0,
        0.0, // outside entirely
        9.0,
    ] {
        let err = engine.split_clip(id, at).unwrap_err();
        assert!(
            matches!(err, EngineError::SplitOutOfRange { .. }),
            "at={at}: {err:?}"
        );
    }
    assert_eq!(
        serialized(&engine),
        before,
        "rejected splits must not mutate"
    );
}

#[test]
fn split_unknown_clip_rejected() {
    let Fixture { mut engine, .. } = engine_with_media();
    let err = engine.split_clip(ClipId(999), 1.0).unwrap_err();
    assert!(matches!(err, EngineError::UnknownClip(_)), "{err:?}");
}

// ---------------------------------------------------------------------
// TrimClip
// ---------------------------------------------------------------------

#[test]
fn trim_end_past_media_bounds_clamps() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    // Source [2, 8) of a 10s file: only 2s of headroom to the right.
    let id = engine.add_clip(video, media, 0.0, 2.0, 8.0).expect("add");
    let applied = engine.trim_clip(id, TrimEdge::End, 20.0).expect("trim");
    assert_eq!(applied, 8.0);
    assert_eq!(span(&engine, id), (0.0, 8.0, 2.0, MEDIA_DUR));
}

#[test]
fn trim_start_past_media_bounds_clamps() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    // Source [2, 8) at timeline [5, 11): dragging the left edge further
    // than 2s of source headroom clamps at source_in == 0.
    let id = engine.add_clip(video, media, 5.0, 2.0, 8.0).expect("add");
    let applied = engine.trim_clip(id, TrimEdge::Start, 0.0).expect("trim");
    assert_eq!(applied, 3.0);
    assert_eq!(span(&engine, id), (3.0, 11.0, 0.0, 8.0));
}

#[test]
fn trim_start_clamps_at_timeline_zero() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    // Plenty of source headroom, but the clip sits at timeline 1: the
    // timeline origin is the binding constraint.
    let id = engine.add_clip(video, media, 1.0, 5.0, 8.0).expect("add");
    let applied = engine.trim_clip(id, TrimEdge::Start, -10.0).expect("trim");
    assert_eq!(applied, 0.0);
    assert_eq!(span(&engine, id), (0.0, 4.0, 4.0, 8.0));
}

#[test]
fn trim_clamps_to_min_duration() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    let id = engine.add_clip(video, media, 0.0, 2.0, 8.0).expect("add");
    // Dragging the end edge past the start edge clamps to the minimum
    // duration instead of inverting or zeroing the clip.
    let applied = engine.trim_clip(id, TrimEdge::End, -5.0).expect("trim");
    assert_eq!(applied, MIN_CLIP_DURATION);
    let (t_in, t_out, s_in, s_out) = span(&engine, id);
    assert_eq!(t_in, 0.0);
    assert_eq!(t_out, MIN_CLIP_DURATION);
    assert!(s_out - s_in > 0.0, "source range stays non-empty");
    engine.project().validate().expect("still valid");
}

#[test]
fn trim_into_neighbor_rejected() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    let left = engine.add_clip(video, media, 0.0, 0.0, 4.0).expect("left");
    engine.add_clip(video, media, 4.0, 4.0, 8.0).expect("right");
    let before = serialized(&engine);

    let err = engine.trim_clip(left, TrimEdge::End, 6.0).unwrap_err();
    assert!(matches!(err, EngineError::ClipOverlap { .. }), "{err:?}");
    assert_eq!(serialized(&engine), before);
}

// ---------------------------------------------------------------------
// Move / Delete / RippleDelete
// ---------------------------------------------------------------------

#[test]
fn move_clip_repositions_and_clamps_to_zero() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    let id = engine.add_clip(video, media, 5.0, 0.0, 3.0).expect("add");
    engine.move_clip(id, 1.0).expect("move");
    assert_eq!(span(&engine, id), (1.0, 4.0, 0.0, 3.0));
    engine.move_clip(id, -2.5).expect("move negative");
    assert_eq!(span(&engine, id), (0.0, 3.0, 0.0, 3.0));
}

#[test]
fn move_onto_other_clip_rejected() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    let a = engine.add_clip(video, media, 0.0, 0.0, 4.0).expect("a");
    engine.add_clip(video, media, 5.0, 4.0, 8.0).expect("b");
    let before = serialized(&engine);
    let err = engine.move_clip(a, 3.0).unwrap_err();
    assert!(matches!(err, EngineError::ClipOverlap { .. }), "{err:?}");
    assert_eq!(serialized(&engine), before);
}

#[test]
fn delete_clip_leaves_gap() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    let a = engine.add_clip(video, media, 0.0, 0.0, 2.0).expect("a");
    let b = engine.add_clip(video, media, 3.0, 2.0, 4.0).expect("b");
    engine.delete_clip(a).expect("delete");
    assert!(engine.project().find_clip(a).is_none());
    assert_eq!(span(&engine, b), (3.0, 5.0, 2.0, 4.0), "b does not shift");
}

#[test]
fn ripple_delete_shifts_later_clips_and_preserves_gaps() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    // A[0,2)  gap(1s)  B[3,5)  gap(1s)  C[6,7)
    let a = engine.add_clip(video, media, 0.0, 0.0, 2.0).expect("a");
    let b = engine.add_clip(video, media, 3.0, 2.0, 4.0).expect("b");
    let c = engine.add_clip(video, media, 6.0, 4.0, 5.0).expect("c");

    engine.ripple_delete(a).expect("ripple");
    assert!(engine.project().find_clip(a).is_none());
    // Shift is exactly A's duration (2s); the B→C gap must survive.
    assert_eq!(span(&engine, b), (1.0, 3.0, 2.0, 4.0));
    assert_eq!(span(&engine, c), (4.0, 5.0, 4.0, 5.0));
}

#[test]
fn ripple_delete_middle_clip_only_shifts_later_clips() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    let a = engine.add_clip(video, media, 0.0, 0.0, 2.0).expect("a");
    let b = engine.add_clip(video, media, 2.0, 2.0, 4.0).expect("b");
    let c = engine.add_clip(video, media, 5.0, 4.0, 5.0).expect("c");

    engine.ripple_delete(b).expect("ripple");
    assert_eq!(span(&engine, a), (0.0, 2.0, 0.0, 2.0), "earlier clip fixed");
    assert_eq!(span(&engine, c), (3.0, 4.0, 4.0, 5.0));
}

#[test]
fn ripple_delete_does_not_touch_other_tracks() {
    let Fixture {
        mut engine,
        media,
        video,
        audio,
    } = engine_with_media();
    let v = engine.add_clip(video, media, 0.0, 0.0, 2.0).expect("v");
    engine.add_clip(video, media, 2.0, 2.0, 4.0).expect("v2");
    let a = engine.add_clip(audio, media, 2.0, 0.0, 3.0).expect("a");

    engine.ripple_delete(v).expect("ripple");
    assert_eq!(
        span(&engine, a),
        (2.0, 5.0, 0.0, 3.0),
        "audio track untouched"
    );
}

// ---------------------------------------------------------------------
// Undo / redo
// ---------------------------------------------------------------------

/// A standard timeline to run each operation against: three video clips
/// with gaps plus one audio clip.
fn populated() -> (Fixture, ClipId) {
    let mut fx = engine_with_media();
    let (media, video, audio) = (fx.media, fx.video, fx.audio);
    let b = fx.engine.add_clip(video, media, 3.0, 2.0, 5.0).expect("b");
    fx.engine.add_clip(video, media, 0.0, 0.0, 2.0).expect("a");
    fx.engine.add_clip(video, media, 7.0, 5.0, 6.0).expect("c");
    fx.engine
        .add_clip(audio, media, 1.0, 0.0, 4.0)
        .expect("audio");
    (fx, b)
}

#[test]
fn undo_redo_round_trips_every_operation() {
    type Op = (&'static str, fn(&mut Engine, ClipId));
    let ops: &[Op] = &[
        ("move", |e, b| {
            e.move_clip(b, 3.5).expect("move");
        }),
        ("trim_start", |e, b| {
            e.trim_clip(b, TrimEdge::Start, 2.5).expect("trim start");
        }),
        ("trim_end", |e, b| {
            e.trim_clip(b, TrimEdge::End, 6.5).expect("trim end");
        }),
        ("split", |e, b| {
            e.split_clip(b, 4.0).expect("split");
        }),
        ("delete", |e, b| {
            e.delete_clip(b).expect("delete");
        }),
        ("ripple_delete", |e, b| {
            e.ripple_delete(b).expect("ripple");
        }),
    ];

    for (name, op) in ops {
        let (mut fx, b) = populated();
        let s0 = serialized(&fx.engine);
        op(&mut fx.engine, b);
        let s1 = serialized(&fx.engine);
        assert_ne!(s0, s1, "{name}: op must change state");

        assert!(fx.engine.undo().expect("undo"), "{name}");
        assert_eq!(serialized(&fx.engine), s0, "{name}: undo restores exactly");
        assert!(fx.engine.redo().expect("redo"), "{name}");
        assert_eq!(serialized(&fx.engine), s1, "{name}: redo restores exactly");

        // Second round trip — inversion must be stable, not one-shot.
        fx.engine.undo().expect("undo 2");
        assert_eq!(serialized(&fx.engine), s0, "{name}: second undo");
        fx.engine.redo().expect("redo 2");
        assert_eq!(serialized(&fx.engine), s1, "{name}: second redo");
    }
}

#[test]
fn add_clip_undo_redo_round_trips() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    let s0 = serialized(&engine);
    engine.add_clip(video, media, 0.0, 0.0, 4.0).expect("add");
    let s1 = serialized(&engine);

    engine.undo().expect("undo");
    assert_eq!(serialized(&engine), s0);
    engine.redo().expect("redo");
    assert_eq!(serialized(&engine), s1);
}

#[test]
fn deep_undo_redo_chain_restores_every_intermediate_state() {
    let (mut fx, b) = populated();
    let base_depth = fx.engine.undo_depth();
    let mut snapshots = vec![serialized(&fx.engine)];

    fx.engine.move_clip(b, 3.25).expect("move");
    snapshots.push(serialized(&fx.engine));
    let right = fx.engine.split_clip(b, 4.5).expect("split");
    snapshots.push(serialized(&fx.engine));
    fx.engine
        .trim_clip(right, TrimEdge::End, 5.5)
        .expect("trim");
    snapshots.push(serialized(&fx.engine));
    fx.engine.ripple_delete(b).expect("ripple");
    snapshots.push(serialized(&fx.engine));

    for expected in snapshots.iter().rev().skip(1) {
        assert!(fx.engine.undo().expect("undo"));
        assert_eq!(&serialized(&fx.engine), expected);
    }
    assert_eq!(fx.engine.undo_depth(), base_depth, "chain fully unwound");

    for expected in snapshots.iter().skip(1) {
        assert!(fx.engine.redo().expect("redo"));
        assert_eq!(&serialized(&fx.engine), expected);
    }
    assert!(!fx.engine.redo().expect("redo at top"), "stack exhausted");
}

#[test]
fn empty_stacks_return_false() {
    let Fixture { mut engine, .. } = engine_with_media();
    assert!(!engine.undo().expect("undo"));
    assert!(!engine.redo().expect("redo"));
}

#[test]
fn new_command_clears_redo_stack() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    let id = engine.add_clip(video, media, 0.0, 0.0, 2.0).expect("add");
    engine.move_clip(id, 5.0).expect("move");
    engine.undo().expect("undo");
    assert_eq!(engine.redo_depth(), 1);
    engine.move_clip(id, 7.0).expect("new command");
    assert_eq!(engine.redo_depth(), 0, "divergent history discards redo");
}

// ---------------------------------------------------------------------
// Transactions
// ---------------------------------------------------------------------

#[test]
fn transaction_of_fifty_micro_moves_is_one_undo_entry() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    let id = engine.add_clip(video, media, 0.0, 0.0, 2.0).expect("add");
    let before_txn = serialized(&engine);
    let depth = engine.undo_depth();

    engine.begin_transaction().expect("begin");
    for i in 1..=50 {
        engine
            .move_clip(id, f64::from(i) * 0.1)
            .expect("micro move");
    }
    engine.commit_transaction().expect("commit");

    assert_eq!(engine.undo_depth(), depth + 1, "exactly one undo entry");
    let after_txn = serialized(&engine);
    assert_eq!(span(&engine, id).0, 5.0, "final drag position applied");

    assert!(engine.undo().expect("undo"));
    assert_eq!(
        serialized(&engine),
        before_txn,
        "one undo reverts the whole drag"
    );
    assert!(engine.redo().expect("redo"));
    assert_eq!(serialized(&engine), after_txn);
}

#[test]
fn transaction_emits_live_preview_events() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    let id = engine.add_clip(video, media, 0.0, 0.0, 2.0).expect("add");
    engine.drain_events();

    engine.begin_transaction().expect("begin");
    engine.move_clip(id, 3.0).expect("move");
    engine.move_clip(id, 4.0).expect("move");
    let during = engine.drain_events();
    assert_eq!(during.len(), 2, "each transient mutation emits a snapshot");
    let EngineEvent::ProjectChanged { project } = during.last().expect("event");
    assert_eq!(project.find_clip(id).expect("clip").1.timeline_in, 4.0);

    engine.commit_transaction().expect("commit");
    assert_eq!(engine.drain_events().len(), 1, "commit emits once more");
}

#[test]
fn transaction_rollback_restores_state_without_undo_entry() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    let id = engine.add_clip(video, media, 0.0, 0.0, 2.0).expect("add");
    let before = serialized(&engine);
    let depth = engine.undo_depth();

    engine.begin_transaction().expect("begin");
    engine.move_clip(id, 6.0).expect("move");
    engine.split_clip(id, 6.5).expect("split");
    engine.rollback_transaction().expect("rollback");

    assert_eq!(serialized(&engine), before);
    assert_eq!(engine.undo_depth(), depth);
    assert!(!engine.transaction_active());
}

#[test]
fn noop_transaction_creates_no_undo_entry() {
    let Fixture { mut engine, .. } = engine_with_media();
    let depth = engine.undo_depth();
    engine.begin_transaction().expect("begin");
    engine.commit_transaction().expect("commit");
    assert_eq!(engine.undo_depth(), depth);
}

#[test]
fn transaction_coalesces_mixed_operations() {
    let (mut fx, b) = populated();
    let before = serialized(&fx.engine);
    let depth = fx.engine.undo_depth();

    fx.engine.begin_transaction().expect("begin");
    fx.engine.move_clip(b, 3.5).expect("move");
    let right = fx.engine.split_clip(b, 4.5).expect("split");
    fx.engine
        .trim_clip(right, TrimEdge::End, 5.9)
        .expect("trim");
    fx.engine.commit_transaction().expect("commit");

    assert_eq!(fx.engine.undo_depth(), depth + 1);
    fx.engine.undo().expect("undo");
    assert_eq!(serialized(&fx.engine), before);
}

#[test]
fn transaction_misuse_is_rejected() {
    let Fixture { mut engine, .. } = engine_with_media();
    assert_eq!(
        engine.commit_transaction().unwrap_err(),
        EngineError::NoTransaction
    );
    assert_eq!(
        engine.rollback_transaction().unwrap_err(),
        EngineError::NoTransaction
    );

    engine.begin_transaction().expect("begin");
    assert_eq!(
        engine.begin_transaction().unwrap_err(),
        EngineError::TransactionActive
    );
    assert_eq!(engine.undo().unwrap_err(), EngineError::TransactionActive);
    assert_eq!(engine.redo().unwrap_err(), EngineError::TransactionActive);
    engine.rollback_transaction().expect("rollback");
}

// ---------------------------------------------------------------------
// Events & serialization
// ---------------------------------------------------------------------

#[test]
fn committed_command_emits_full_snapshot() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    engine.add_clip(video, media, 0.0, 0.0, 2.0).expect("add");
    let events = engine.drain_events();
    assert_eq!(events.len(), 1);
    let EngineEvent::ProjectChanged { project } = &events[0];
    assert_eq!(
        project,
        engine.project(),
        "event carries the full new state"
    );
    assert!(engine.drain_events().is_empty(), "drain empties the queue");
}

#[test]
fn undo_and_redo_emit_snapshots() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    engine.add_clip(video, media, 0.0, 0.0, 2.0).expect("add");
    engine.drain_events();
    engine.undo().expect("undo");
    engine.redo().expect("redo");
    assert_eq!(engine.drain_events().len(), 2);
}

#[test]
fn project_serde_round_trip_is_lossless() {
    let (fx, _) = populated();
    let json = serde_json::to_string_pretty(fx.engine.project()).expect("serialize");
    let parsed: cutty_engine::Project = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(&parsed, fx.engine.project());

    let reloaded = Engine::from_project(parsed).expect("load");
    assert_eq!(reloaded.project(), fx.engine.project());
}

#[test]
fn from_project_rejects_invalid_state() {
    let (fx, _) = populated();
    let mut broken = fx.engine.project().clone();
    // Force an overlap directly in the data (bypassing the command system,
    // as a corrupted .cutty file would): shift the second clip onto the
    // first, keeping its duration so only the overlap invariant trips.
    let track = &mut broken.tracks[0];
    track.clips[1].timeline_in -= 2.0;
    track.clips[1].timeline_out -= 2.0;
    let err = Engine::from_project(broken).unwrap_err();
    assert!(matches!(err, EngineError::ClipOverlap { .. }), "{err:?}");
}

#[test]
fn loaded_project_continues_id_allocation_without_collision() {
    let (fx, _) = populated();
    let mut reloaded = Engine::from_project(fx.engine.project().clone()).expect("load");
    let media = reloaded.project().media[0].id;
    let video = reloaded.project().tracks[0].id;
    let id = reloaded
        .add_clip(video, media, 20.0, 6.0, 8.0)
        .expect("add after load");
    reloaded.project().validate().expect("ids still unique");
    assert!(
        id.0 > fx.engine.project().max_id(),
        "new ids start above every persisted id"
    );
}

// ---------------------------------------------------------------------
// RemoveMedia (pool item + all its clips, one undoable command)
// ---------------------------------------------------------------------

#[test]
fn remove_media_removes_pool_entry_and_all_its_clips() {
    let Fixture {
        mut engine,
        media,
        video,
        audio,
    } = engine_with_media();
    let other = engine
        .add_media("/tmp/other.mp4", MEDIA_DUR, true, true)
        .expect("second media");
    engine.add_clip(video, media, 0.0, 0.0, 2.0).expect("v1");
    engine.add_clip(video, media, 5.0, 0.0, 2.0).expect("v2");
    engine.add_clip(audio, media, 1.0, 0.0, 3.0).expect("a1");
    let kept = engine.add_clip(video, other, 2.5, 0.0, 2.0).expect("kept");
    let depth = engine.undo_depth();

    engine.remove_media(media).expect("remove");

    let project = engine.project();
    assert!(project.media(media).is_none(), "pool entry gone");
    assert!(project.media(other).is_some(), "other media kept");
    let remaining: Vec<_> = project
        .tracks
        .iter()
        .flat_map(|t| t.clips.iter().map(|c| c.id))
        .collect();
    assert_eq!(remaining, vec![kept], "only the other media's clip is left");
    assert_eq!(engine.undo_depth(), depth + 1, "exactly one undo entry");
}

#[test]
fn remove_media_undo_redo_round_trips() {
    let Fixture {
        mut engine,
        media,
        video,
        audio,
    } = engine_with_media();
    engine.add_clip(video, media, 0.0, 0.0, 2.0).expect("v");
    engine.add_clip(audio, media, 1.5, 2.0, 6.0).expect("a");
    let before = serialized(&engine);

    engine.remove_media(media).expect("remove");
    let after = serialized(&engine);
    assert_ne!(before, after);

    assert!(engine.undo().expect("undo"));
    assert_eq!(
        serialized(&engine),
        before,
        "undo restores media and clips verbatim"
    );

    assert!(engine.redo().expect("redo"));
    assert_eq!(serialized(&engine), after, "redo removes them again");
}

#[test]
fn remove_media_without_clips_is_still_one_undoable_step() {
    let Fixture {
        mut engine, media, ..
    } = engine_with_media();
    engine.remove_media(media).expect("remove");
    assert!(engine.project().media.is_empty());
    assert!(engine.undo().expect("undo"));
    assert!(engine.project().media(media).is_some(), "pool entry back");
}

#[test]
fn remove_unknown_media_rejected_and_state_untouched() {
    let Fixture {
        mut engine, media, ..
    } = engine_with_media();
    let before = serialized(&engine);
    let depth = engine.undo_depth();
    engine.drain_events();

    let err = engine
        .remove_media(cutty_engine::MediaId(9999))
        .unwrap_err();
    assert!(matches!(err, EngineError::UnknownMedia(_)), "{err:?}");
    assert_eq!(serialized(&engine), before);
    assert_eq!(engine.undo_depth(), depth);
    assert!(engine.drain_events().is_empty(), "failed op must not emit");
    assert!(engine.project().media(media).is_some());
}

// ---------------------------------------------------------------------
// SetClipVolume
// ---------------------------------------------------------------------

#[test]
fn set_clip_volume_applies_and_undo_restores_exact_value() {
    let Fixture {
        mut engine,
        media,
        audio,
        ..
    } = engine_with_media();
    let id = engine.add_clip(audio, media, 0.0, 0.0, 4.0).expect("add");

    engine.set_clip_volume(id, 0.35).expect("set volume");
    let (_, clip) = engine.project().find_clip(id).expect("clip");
    assert_eq!(clip.volume, 0.35);

    // Volume 0 (mute) and >1 (boost) are both valid gains.
    engine.set_clip_volume(id, 0.0).expect("mute");
    engine.set_clip_volume(id, 2.0).expect("boost");

    assert!(engine.undo().expect("undo boost"));
    assert!(engine.undo().expect("undo mute"));
    let (_, clip) = engine.project().find_clip(id).expect("clip");
    assert_eq!(clip.volume, 0.35, "undo restores the exact prior gain");
    assert!(engine.redo().expect("redo mute"));
    let (_, clip) = engine.project().find_clip(id).expect("clip");
    assert_eq!(clip.volume, 0.0);
}

#[test]
fn set_clip_volume_rejects_invalid_gains_and_unknown_clips() {
    let Fixture {
        mut engine,
        media,
        audio,
        ..
    } = engine_with_media();
    let id = engine.add_clip(audio, media, 0.0, 0.0, 4.0).expect("add");
    let before = serialized(&engine);
    let depth = engine.undo_depth();
    engine.drain_events();

    for bad in [-0.1, f64::NAN, f64::INFINITY] {
        let err = engine.set_clip_volume(id, bad).unwrap_err();
        assert!(matches!(err, EngineError::InvalidProperty { .. }), "{err:?}");
    }
    let err = engine.set_clip_volume(ClipId(9999), 0.5).unwrap_err();
    assert!(matches!(err, EngineError::UnknownClip(_)), "{err:?}");

    assert_eq!(serialized(&engine), before);
    assert_eq!(engine.undo_depth(), depth);
    assert!(engine.drain_events().is_empty(), "failed op must not emit");
}

#[test]
fn set_clip_volume_inside_transaction_is_one_undo_entry() {
    let Fixture {
        mut engine,
        media,
        audio,
        ..
    } = engine_with_media();
    let id = engine.add_clip(audio, media, 0.0, 0.0, 4.0).expect("add");
    let depth = engine.undo_depth();

    // A volume-slider drag from unity down to 0.25: many transient
    // updates, one undo entry.
    engine.begin_transaction().expect("begin");
    for step in 1..=15 {
        engine
            .set_clip_volume(id, 1.0 - f64::from(step) * 0.05)
            .expect("transient");
    }
    engine.commit_transaction().expect("commit");

    let (_, clip) = engine.project().find_clip(id).expect("clip");
    assert_eq!(clip.volume, 0.25);
    assert_eq!(engine.undo_depth(), depth + 1, "gesture = one entry");
    assert!(engine.undo().expect("undo"));
    let (_, clip) = engine.project().find_clip(id).expect("clip");
    assert_eq!(clip.volume, 1.0, "back to pre-gesture volume");
}
