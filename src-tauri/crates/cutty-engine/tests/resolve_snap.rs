//! Integration tests for the playback resolver and snapping.

mod common;

use common::{engine_with_media, Fixture};
use cutty_engine::{resolve, snap, snap_candidates, TrackKind};

// ---------------------------------------------------------------------
// Resolver
// ---------------------------------------------------------------------

#[test]
fn resolver_maps_timeline_time_to_source_time() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    // Timeline [1, 7) ← source [2, 8).
    let id = engine.add_clip(video, media, 1.0, 2.0, 8.0).expect("add");

    let active = resolve(engine.project(), 3.0);
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].clip_id, id);
    assert_eq!(active[0].track_id, video);
    assert_eq!(active[0].source_time, 4.0);
}

#[test]
fn resolver_at_cut_boundary_returns_the_incoming_clip() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    let left = engine.add_clip(video, media, 1.0, 2.0, 8.0).expect("add");
    let right = engine.split_clip(left, 4.0).expect("split");

    // Exactly at the cut: only the right clip, starting at its source_in.
    let at_cut = resolve(engine.project(), 4.0);
    assert_eq!(at_cut.len(), 1, "never both halves at the cut");
    assert_eq!(at_cut[0].clip_id, right);
    assert_eq!(at_cut[0].source_time, 5.0);

    // Just before the cut: still the left clip.
    let before_cut = resolve(engine.project(), 4.0 - 1e-6);
    assert_eq!(before_cut.len(), 1);
    assert_eq!(before_cut[0].clip_id, left);

    // Clip starts are inclusive.
    let at_start = resolve(engine.project(), 1.0);
    assert_eq!(at_start[0].clip_id, left);
    assert_eq!(at_start[0].source_time, 2.0);
}

#[test]
fn resolver_end_of_timeline_and_gaps_are_empty() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    engine.add_clip(video, media, 0.0, 0.0, 2.0).expect("a");
    engine.add_clip(video, media, 3.0, 2.0, 4.0).expect("b");

    assert!(resolve(engine.project(), 2.5).is_empty(), "gap");
    assert!(
        resolve(engine.project(), 5.0).is_empty(),
        "end is exclusive"
    );
    assert!(resolve(engine.project(), 100.0).is_empty(), "past the end");
    assert!(resolve(engine.project(), f64::NAN).is_empty(), "non-finite");
}

#[test]
fn resolver_returns_all_tracks_in_order() {
    let Fixture {
        mut engine,
        media,
        video,
        audio,
    } = engine_with_media();
    let v = engine.add_clip(video, media, 0.0, 0.0, 4.0).expect("v");
    let a = engine.add_clip(audio, media, 1.0, 2.0, 6.0).expect("a");

    let active = resolve(engine.project(), 2.0);
    assert_eq!(active.len(), 2);
    assert_eq!(active[0].clip_id, v, "project track order");
    assert_eq!(active[0].source_time, 2.0);
    assert_eq!(active[1].clip_id, a);
    assert_eq!(active[1].source_time, 3.0);
}

#[test]
fn resolver_skips_muted_tracks() {
    let Fixture {
        mut engine,
        media,
        video,
        audio,
    } = engine_with_media();
    engine.add_clip(video, media, 0.0, 0.0, 4.0).expect("v");
    engine.add_clip(audio, media, 0.0, 0.0, 4.0).expect("a");

    // Mute the audio track directly on a copy of the state — track
    // mute/lock toggles get their own commands in a later prompt.
    let mut project = engine.project().clone();
    let audio_track = project
        .tracks
        .iter_mut()
        .find(|t| t.kind == TrackKind::Audio)
        .expect("audio track");
    audio_track.muted = true;

    let active = resolve(&project, 1.0);
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].track_id, video);
}

// ---------------------------------------------------------------------
// Snapping
// ---------------------------------------------------------------------

#[test]
fn snap_within_threshold_returns_candidate() {
    assert_eq!(snap(4.9, &[5.0, 10.0], 0.2), Some(5.0));
    assert_eq!(
        snap(5.0, &[5.0], 0.0),
        Some(5.0),
        "exact hit at zero threshold"
    );
}

#[test]
fn snap_outside_threshold_returns_none() {
    assert_eq!(snap(4.5, &[5.0, 10.0], 0.2), None);
    assert_eq!(snap(1.0, &[], 10.0), None, "no candidates");
}

#[test]
fn snap_picks_the_nearest_candidate() {
    assert_eq!(snap(5.3, &[5.0, 5.4, 6.0], 1.0), Some(5.4));
}

#[test]
fn snap_tie_prefers_the_earlier_candidate() {
    assert_eq!(snap(5.0, &[6.0, 4.0], 1.0), Some(4.0));
}

#[test]
fn snap_ignores_degenerate_inputs() {
    assert_eq!(snap(5.0, &[f64::NAN, f64::INFINITY, 5.1], 0.2), Some(5.1));
    assert_eq!(snap(f64::NAN, &[5.0], 0.2), None);
    assert_eq!(snap(5.0, &[5.0], f64::NAN), None);
    assert_eq!(
        snap(5.0, &[5.0], -1.0),
        None,
        "negative threshold never snaps"
    );
}

#[test]
fn snap_candidates_collects_edges_and_playhead_excluding_dragged_clip() {
    let Fixture {
        mut engine,
        media,
        video,
        audio,
    } = engine_with_media();
    let dragged = engine.add_clip(video, media, 0.0, 0.0, 2.0).expect("drag");
    engine.add_clip(video, media, 4.0, 2.0, 6.0).expect("other");
    engine.add_clip(audio, media, 4.0, 0.0, 3.0).expect("audio");

    let candidates = snap_candidates(engine.project(), Some(9.5), &[dragged]);
    // Video other: 4, 8 · audio: 4, 7 · playhead: 9.5 — sorted, deduped,
    // and none of the dragged clip's own edges.
    assert_eq!(candidates, vec![4.0, 7.0, 8.0, 9.5]);
}
