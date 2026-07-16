//! Transition semantics: set/replace/remove, duration clamping, pruning
//! on every structural mutation (undoable), span resolution with handle
//! clamping, and the track-visual stack renderers consume.

mod common;

use common::{engine_with_media, serialized, Fixture, MEDIA_DUR};
use cutty_engine::{
    resolve_track_visuals, transition_spans, ClipId, EngineError, TrackVisual, Transition,
    MIN_TRANSITION_DURATION,
};

fn tr(kind: &str, duration: f64) -> Transition {
    Transition {
        kind: kind.into(),
        duration,
    }
}

/// Two touching 2s clips: A = [0,2) src [1,3), B = [2,4) src [5,7).
/// Both have generous source handles on both sides of the cut.
fn two_clip_fixture() -> (Fixture, ClipId, ClipId) {
    let mut f = engine_with_media();
    let a = f.engine.add_clip(f.video, f.media, 0.0, 1.0, 3.0).unwrap();
    let b = f.engine.add_clip(f.video, f.media, 2.0, 5.0, 7.0).unwrap();
    (f, a, b)
}

fn transition_of(f: &Fixture, clip: ClipId) -> Option<Transition> {
    let (_, c) = f.engine.project().find_clip(clip).unwrap();
    c.transition_out.clone()
}

// ---------------------------------------------------------------------
// set_transition
// ---------------------------------------------------------------------

#[test]
fn set_replace_remove_round_trip_with_undo() {
    let (mut f, a, _b) = two_clip_fixture();
    let before = serialized(&f.engine);

    let applied = f.engine.set_transition(a, Some(tr("fade", 1.0))).unwrap();
    assert_eq!(applied, Some(1.0));
    assert_eq!(transition_of(&f, a), Some(tr("fade", 1.0)));
    let with_fade = serialized(&f.engine);

    // Replace kind, keep duration: separate undo step.
    f.engine
        .set_transition(a, Some(tr("wipeleft", 1.0)))
        .unwrap();
    assert_eq!(transition_of(&f, a), Some(tr("wipeleft", 1.0)));

    // Remove.
    f.engine.set_transition(a, None).unwrap();
    assert_eq!(transition_of(&f, a), None);

    // Undo unwinds: remove → wipeleft → fade → none.
    assert!(f.engine.undo().unwrap());
    assert_eq!(transition_of(&f, a), Some(tr("wipeleft", 1.0)));
    assert!(f.engine.undo().unwrap());
    assert_eq!(serialized(&f.engine), with_fade);
    assert!(f.engine.undo().unwrap());
    assert_eq!(serialized(&f.engine), before);

    // Redo replays.
    assert!(f.engine.redo().unwrap());
    assert_eq!(serialized(&f.engine), with_fade);
}

#[test]
fn set_transition_rejects_impossible_targets() {
    let mut f = engine_with_media();
    let a = f.engine.add_clip(f.video, f.media, 0.0, 1.0, 3.0).unwrap();
    // No next clip at all.
    assert!(matches!(
        f.engine.set_transition(a, Some(tr("fade", 0.5))),
        Err(EngineError::InvalidTransition { .. })
    ));
    // A gap after A (next clip not touching).
    let _b = f.engine.add_clip(f.video, f.media, 2.5, 5.0, 7.0).unwrap();
    assert!(matches!(
        f.engine.set_transition(a, Some(tr("fade", 0.5))),
        Err(EngineError::InvalidTransition { .. })
    ));
    // Audio clips can't hold transitions.
    let m1 = f.engine.add_clip(f.audio, f.media, 0.0, 0.0, 1.0).unwrap();
    let _m2 = f.engine.add_clip(f.audio, f.media, 1.0, 1.0, 2.0).unwrap();
    assert!(matches!(
        f.engine.set_transition(m1, Some(tr("fade", 0.5))),
        Err(EngineError::InvalidTransition { .. })
    ));
    // Bad durations and kinds.
    let (mut f, a, _) = two_clip_fixture();
    for bad in [f64::NAN, f64::INFINITY, 0.0, -1.0] {
        assert!(f.engine.set_transition(a, Some(tr("fade", bad))).is_err());
    }
    assert!(f.engine.set_transition(a, Some(tr("", 0.5))).is_err());
    // Locked track rejects.
    let (mut f, a, _) = two_clip_fixture();
    f.engine
        .set_track_flag(f.video, cutty_engine::TrackFlag::Locked, true)
        .unwrap();
    assert!(matches!(
        f.engine.set_transition(a, Some(tr("fade", 0.5))),
        Err(EngineError::TrackLocked { .. })
    ));
}

#[test]
fn duration_clamps_to_clips_and_handles() {
    // Plenty of handle on both sides: limit is min(clip durations) = 2.0.
    let (mut f, a, _b) = two_clip_fixture();
    let applied = f.engine.set_transition(a, Some(tr("fade", 10.0))).unwrap();
    assert_eq!(applied, Some(2.0));

    // Outgoing handle of 0.2s (A's source ends 0.2s before media end):
    // limit = 2 * 0.2 = 0.4.
    let mut f = engine_with_media();
    let a = f
        .engine
        .add_clip(f.video, f.media, 0.0, MEDIA_DUR - 2.2, MEDIA_DUR - 0.2)
        .unwrap();
    let _b = f.engine.add_clip(f.video, f.media, 2.0, 1.0, 3.0).unwrap();
    let applied = f.engine.set_transition(a, Some(tr("fade", 1.0))).unwrap();
    assert!((applied.unwrap() - 0.4).abs() < 1e-9, "{applied:?}");

    // Zero outgoing handle (source ends exactly at media end): the freeze
    // side does NOT constrain — the incoming side's 1.0s pre-handle does
    // (limit 2.0), so the request stands.
    let mut f = engine_with_media();
    let a = f
        .engine
        .add_clip(f.video, f.media, 0.0, MEDIA_DUR - 2.0, MEDIA_DUR)
        .unwrap();
    let _b = f.engine.add_clip(f.video, f.media, 2.0, 1.0, 3.0).unwrap();
    let applied = f.engine.set_transition(a, Some(tr("fade", 1.0))).unwrap();
    assert_eq!(applied, Some(1.0));

    // Tiny incoming pre-handle (B starts 0.05s into its source):
    // limit = 0.1 — and the MIN floor must not push past the limit.
    let mut f = engine_with_media();
    let a = f.engine.add_clip(f.video, f.media, 0.0, 1.0, 3.0).unwrap();
    let _b = f.engine.add_clip(f.video, f.media, 2.0, 0.05, 2.0).unwrap();
    let applied = f.engine.set_transition(a, Some(tr("fade", 1.0))).unwrap();
    assert!((applied.unwrap() - 0.1).abs() < 1e-9, "{applied:?}");

    // Requests under the floor clamp up to it.
    let (mut f, a, _b) = two_clip_fixture();
    let applied = f.engine.set_transition(a, Some(tr("fade", 0.01))).unwrap();
    assert_eq!(applied, Some(MIN_TRANSITION_DURATION));
}

// ---------------------------------------------------------------------
// Spans
// ---------------------------------------------------------------------

#[test]
fn spans_center_on_the_cut_and_clamp_live() {
    let (mut f, a, b) = two_clip_fixture();
    f.engine.set_transition(a, Some(tr("fade", 1.0))).unwrap();

    let spans = transition_spans(f.engine.project());
    assert_eq!(spans.len(), 1);
    let s = &spans[0];
    assert_eq!((s.from_clip, s.to_clip), (a, b));
    assert_eq!(s.kind, "fade");
    assert!((s.cut - 2.0).abs() < 1e-9);
    assert!((s.start - 1.5).abs() < 1e-9);
    assert!((s.end - 2.5).abs() < 1e-9);
    assert!((s.requested - 1.0).abs() < 1e-9);
    assert!((s.max_duration - 2.0).abs() < 1e-9);

    // Trimming B's far edge down to 0.4s shrinks the *effective* span
    // (the acceptance case: neighbor shorter than the transition).
    f.engine
        .trim_clip(b, cutty_engine::TrimEdge::End, 2.4)
        .unwrap();
    let spans = transition_spans(f.engine.project());
    assert_eq!(spans.len(), 1);
    let s = &spans[0];
    assert!((s.end - s.start - 0.4).abs() < 1e-9, "span {s:?}");
    assert!(
        (s.requested - 1.0).abs() < 1e-9,
        "stored duration untouched"
    );

    // Undoing the trim restores the full span — nothing stored changed.
    f.engine.undo().unwrap();
    let spans = transition_spans(f.engine.project());
    assert!((spans[0].end - spans[0].start - 1.0).abs() < 1e-9);
}

#[test]
fn track_visuals_pair_inside_the_span_with_extended_sources() {
    let (mut f, a, b) = two_clip_fixture();
    f.engine.set_transition(a, Some(tr("fade", 1.0))).unwrap();
    let p = f.engine.project();

    // Before the span: A alone.
    match &resolve_track_visuals(p, 1.0)[..] {
        [TrackVisual::Single(c)] => assert_eq!(c.clip_id, a),
        other => panic!("expected single A, got {other:?}"),
    }

    // Inside, pre-cut (t = 1.75): pair; A normal, B extended backwards
    // (source_in 5.0, 0.25s before its start → 4.75). Progress 0.25.
    match &resolve_track_visuals(p, 1.75)[..] {
        [TrackVisual::Transition {
            from,
            to,
            kind,
            progress,
        }] => {
            assert_eq!((from.clip_id, to.clip_id), (a, b));
            assert_eq!(kind, "fade");
            assert!((from.source_time - 2.75).abs() < 1e-9);
            assert!((to.source_time - 4.75).abs() < 1e-9);
            assert!((progress - 0.25).abs() < 1e-9);
        }
        other => panic!("expected pair, got {other:?}"),
    }

    // Inside, post-cut (t = 2.25): A extended past its source_out
    // (3.0 → 3.25 — handle media), B normal. Progress 0.75.
    match &resolve_track_visuals(p, 2.25)[..] {
        [TrackVisual::Transition {
            from, to, progress, ..
        }] => {
            assert!((from.source_time - 3.25).abs() < 1e-9);
            assert!((to.source_time - 5.25).abs() < 1e-9);
            assert!((progress - 0.75).abs() < 1e-9);
        }
        other => panic!("expected pair, got {other:?}"),
    }

    // At the span end: B alone.
    match &resolve_track_visuals(p, 2.5)[..] {
        [TrackVisual::Single(c)] => assert_eq!(c.clip_id, b),
        other => panic!("expected single B, got {other:?}"),
    }
}

#[test]
fn consecutive_transitions_do_not_overlap() {
    // A|B|C, 2s each, transitions on both cuts at the max the clips
    // allow (2.0s): spans are [1,3) and [3,5) — touching, not
    // overlapping, so every instant resolves to at most one pair.
    let mut f = engine_with_media();
    let a = f.engine.add_clip(f.video, f.media, 0.0, 1.0, 3.0).unwrap();
    let b = f.engine.add_clip(f.video, f.media, 2.0, 4.0, 6.0).unwrap();
    let c = f.engine.add_clip(f.video, f.media, 4.0, 7.0, 9.0).unwrap();
    f.engine.set_transition(a, Some(tr("fade", 10.0))).unwrap();
    f.engine
        .set_transition(b, Some(tr("wipeleft", 10.0)))
        .unwrap();

    let spans = transition_spans(f.engine.project());
    assert_eq!(spans.len(), 2);
    assert!(spans[0].end <= spans[1].start + 1e-9);

    let p = f.engine.project();
    for (t, from, to) in [(1.5, a, b), (2.9, a, b), (3.1, b, c), (4.9, b, c)] {
        match &resolve_track_visuals(p, t)[..] {
            [TrackVisual::Transition {
                from: fr, to: t2, ..
            }] => {
                assert_eq!((fr.clip_id, t2.clip_id), (from, to), "at {t}");
            }
            other => panic!("expected pair at {t}, got {other:?}"),
        }
    }
}

#[test]
fn hidden_tracks_keep_spans_but_drop_visuals() {
    let (mut f, a, _b) = two_clip_fixture();
    f.engine.set_transition(a, Some(tr("fade", 1.0))).unwrap();
    f.engine
        .set_track_flag(f.video, cutty_engine::TrackFlag::Hidden, true)
        .unwrap();
    // Chips still resolve (the timeline shows them dimmed)…
    assert_eq!(transition_spans(f.engine.project()).len(), 1);
    // …but the composite skips the track entirely.
    assert!(resolve_track_visuals(f.engine.project(), 2.0).is_empty());
}

// ---------------------------------------------------------------------
// Pruning on structural mutations (all one undo step)
// ---------------------------------------------------------------------

#[test]
fn deleting_the_incoming_clip_prunes_as_one_undo_step() {
    let (mut f, a, b) = two_clip_fixture();
    f.engine.set_transition(a, Some(tr("fade", 1.0))).unwrap();
    let before = serialized(&f.engine);

    f.engine.delete_clip(b).unwrap();
    assert_eq!(transition_of(&f, a), None, "transition lost its cut");

    // ONE undo restores both the clip and the transition.
    assert!(f.engine.undo().unwrap());
    assert_eq!(serialized(&f.engine), before);
    assert_eq!(transition_of(&f, a), Some(tr("fade", 1.0)));

    // Redo re-prunes.
    assert!(f.engine.redo().unwrap());
    assert_eq!(transition_of(&f, a), None);
}

#[test]
fn deleting_the_outgoing_clip_takes_its_transition_along() {
    let (mut f, a, _b) = two_clip_fixture();
    f.engine.set_transition(a, Some(tr("fade", 1.0))).unwrap();
    let before = serialized(&f.engine);
    f.engine.delete_clip(a).unwrap();
    f.engine.project().validate().unwrap();
    assert!(f.engine.undo().unwrap());
    assert_eq!(serialized(&f.engine), before);
}

#[test]
fn ripple_delete_of_the_middle_clip_rebinds_to_the_closing_cut() {
    // A|B|C with a transition on A→B. Ripple-deleting B closes the gap
    // (C slides onto A's out edge): the transition stays valid on the
    // new A|C cut — "revalidates" rather than removes.
    let mut f = engine_with_media();
    let a = f.engine.add_clip(f.video, f.media, 0.0, 1.0, 3.0).unwrap();
    let b = f.engine.add_clip(f.video, f.media, 2.0, 4.0, 6.0).unwrap();
    let c = f.engine.add_clip(f.video, f.media, 4.0, 7.0, 9.0).unwrap();
    f.engine.set_transition(a, Some(tr("fade", 1.0))).unwrap();
    let before = serialized(&f.engine);

    f.engine.ripple_delete(b).unwrap();
    assert_eq!(transition_of(&f, a), Some(tr("fade", 1.0)));
    let spans = transition_spans(f.engine.project());
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].to_clip, c);

    assert!(f.engine.undo().unwrap());
    assert_eq!(serialized(&f.engine), before);

    // Plain delete (gap stays open) prunes instead.
    f.engine.delete_clip(b).unwrap();
    assert_eq!(transition_of(&f, a), None);
}

#[test]
fn moving_either_neighbor_away_prunes() {
    let (mut f, a, b) = two_clip_fixture();
    f.engine.set_transition(a, Some(tr("fade", 1.0))).unwrap();
    let before = serialized(&f.engine);

    f.engine.move_clip(b, 5.0).unwrap();
    assert_eq!(transition_of(&f, a), None);
    f.engine.undo().unwrap();
    assert_eq!(serialized(&f.engine), before);

    f.engine.move_clip(a, 6.0).unwrap();
    assert_eq!(transition_of(&f, a), None);
    f.engine.undo().unwrap();
    assert_eq!(serialized(&f.engine), before);

    // Moving to another track breaks the cut the same way.
    let v2 = f
        .engine
        .add_track(cutty_engine::TrackKind::Video, 0)
        .unwrap();
    f.engine.move_clip_to_track(b, v2, 2.0).unwrap();
    assert_eq!(transition_of(&f, a), None);
}

#[test]
fn trimming_the_cut_edge_prunes_but_far_edges_do_not() {
    use cutty_engine::TrimEdge;

    // Trim A's end (the cut edge) left: gap opens → prune.
    let (mut f, a, _b) = two_clip_fixture();
    f.engine.set_transition(a, Some(tr("fade", 1.0))).unwrap();
    f.engine.trim_clip(a, TrimEdge::End, 1.5).unwrap();
    assert_eq!(transition_of(&f, a), None);
    f.engine.undo().unwrap();
    assert_eq!(transition_of(&f, a), Some(tr("fade", 1.0)));

    // Trim B's start (the cut edge) right: gap opens → prune.
    let (mut f, a, b) = two_clip_fixture();
    f.engine.set_transition(a, Some(tr("fade", 1.0))).unwrap();
    f.engine.trim_clip(b, TrimEdge::Start, 2.5).unwrap();
    assert_eq!(transition_of(&f, a), None);

    // Far edges leave the cut alone: transition survives.
    let (mut f, a, b) = two_clip_fixture();
    f.engine.set_transition(a, Some(tr("fade", 1.0))).unwrap();
    f.engine.trim_clip(a, TrimEdge::Start, 0.5).unwrap();
    f.engine.trim_clip(b, TrimEdge::End, 3.5).unwrap();
    assert_eq!(transition_of(&f, a), Some(tr("fade", 1.0)));
}

#[test]
fn removing_media_prunes_neighbor_transitions() {
    // A (fixture media) | B (second media, removed): A's transition must
    // not survive pointing into the hole.
    let mut f = engine_with_media();
    let media2 = f
        .engine
        .add_media("/tmp/fixture-2.mp4", MEDIA_DUR, true, true)
        .unwrap();
    let a = f.engine.add_clip(f.video, f.media, 0.0, 1.0, 3.0).unwrap();
    let _b = f.engine.add_clip(f.video, media2, 2.0, 5.0, 7.0).unwrap();
    f.engine.set_transition(a, Some(tr("fade", 1.0))).unwrap();
    let before = serialized(&f.engine);

    f.engine.remove_media(media2).unwrap();
    assert_eq!(transition_of(&f, a), None);
    f.engine.project().validate().unwrap();

    assert!(f.engine.undo().unwrap());
    assert_eq!(serialized(&f.engine), before);
}

#[test]
fn splitting_the_outgoing_clip_moves_the_transition_to_the_right_half() {
    let (mut f, a, b) = two_clip_fixture();
    f.engine.set_transition(a, Some(tr("fade", 1.0))).unwrap();

    let right = f.engine.split_clip(a, 1.0).unwrap();
    assert_eq!(transition_of(&f, a), None, "left half loses the cut");
    assert_eq!(transition_of(&f, right), Some(tr("fade", 1.0)));
    let spans = transition_spans(f.engine.project());
    assert_eq!(spans.len(), 1);
    assert_eq!((spans[0].from_clip, spans[0].to_clip), (right, b));

    // Splitting the incoming clip keeps the transition bound to its
    // (unmoved) left half.
    let (mut f, a, b) = two_clip_fixture();
    f.engine.set_transition(a, Some(tr("fade", 1.0))).unwrap();
    let _b_right = f.engine.split_clip(b, 3.0).unwrap();
    let spans = transition_spans(f.engine.project());
    assert_eq!((spans[0].from_clip, spans[0].to_clip), (a, b));
}

#[test]
fn transient_transaction_moves_prune_and_rollback_restores() {
    let (mut f, a, b) = two_clip_fixture();
    f.engine.set_transition(a, Some(tr("fade", 1.0))).unwrap();
    let before = serialized(&f.engine);

    f.engine.begin_transaction().unwrap();
    f.engine.move_clip(b, 5.0).unwrap();
    assert_eq!(transition_of(&f, a), None, "transient move prunes live");
    f.engine.rollback_transaction().unwrap();
    assert_eq!(serialized(&f.engine), before);

    // A committed drag is one undo entry covering move + prune.
    f.engine.begin_transaction().unwrap();
    f.engine.move_clip(b, 5.0).unwrap();
    f.engine.commit_transaction().unwrap();
    assert_eq!(transition_of(&f, a), None);
    assert!(f.engine.undo().unwrap());
    assert_eq!(serialized(&f.engine), before);
}

// ---------------------------------------------------------------------
// Validation & persistence
// ---------------------------------------------------------------------

#[test]
fn validate_rejects_dangling_or_misplaced_transitions() {
    let (mut f, a, _b) = two_clip_fixture();
    f.engine.set_transition(a, Some(tr("fade", 1.0))).unwrap();

    // Hand-break adjacency: validation must reject the state.
    let mut project = f.engine.project().clone();
    let track = project.tracks.iter_mut().find(|t| t.id == f.video).unwrap();
    track.clips[1].timeline_in = 3.0;
    track.clips[1].timeline_out = 5.0;
    track.clips[1].source_in = 4.0;
    track.clips[1].source_out = 6.0;
    assert!(matches!(
        project.validate(),
        Err(EngineError::InvalidTransition { .. })
    ));

    // Hand-move the transition onto an audio clip: rejected.
    let mut project = f.engine.project().clone();
    let audio = project.tracks.iter_mut().find(|t| t.id == f.audio).unwrap();
    audio.clips = vec![];
    let mut clip = f.engine.project().track(f.video).unwrap().clips[0].clone();
    clip.id = ClipId(9001);
    let mut clip2 = f.engine.project().track(f.video).unwrap().clips[1].clone();
    clip2.id = ClipId(9002);
    let audio = project.tracks.iter_mut().find(|t| t.id == f.audio).unwrap();
    audio.clips = vec![clip, clip2];
    assert!(matches!(
        project.validate(),
        Err(EngineError::InvalidTransition { .. })
    ));
}

#[test]
fn project_file_round_trips_transitions_and_loads_legacy_files() {
    let (mut f, a, _b) = two_clip_fixture();
    f.engine
        .set_transition(a, Some(tr("circleopen", 0.75)))
        .unwrap();

    let json = cutty_engine::project_file::serialize(f.engine.project(), None);
    let loaded = cutty_engine::project_file::deserialize(&json, None).unwrap();
    assert_eq!(&loaded, f.engine.project());

    // A pre-transition file (no transitionOut keys) still loads, and a
    // project without transitions serializes without the key at all.
    let (f2, ..) = two_clip_fixture();
    let legacy = cutty_engine::project_file::serialize(f2.engine.project(), None);
    assert!(!legacy.contains("transitionOut"));
    let loaded = cutty_engine::project_file::deserialize(&legacy, None).unwrap();
    assert_eq!(&loaded, f2.engine.project());
}
