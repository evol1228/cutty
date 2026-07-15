//! Integration tests for the playback resolver and snapping.

mod common;

use common::{engine_with_media, Fixture};
use cutty_engine::{
    active_video_clip, next_boundary_after, resolve, snap, snap_candidates, snap_clip_move,
    snap_time, timeline_end, ClipId, EngineError, TrackKind,
};

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

#[test]
fn active_video_clip_prefers_video_tracks_and_skips_gaps() {
    let Fixture {
        mut engine,
        media,
        video,
        audio,
    } = engine_with_media();
    let v = engine.add_clip(video, media, 1.0, 0.0, 3.0).expect("v");
    engine.add_clip(audio, media, 0.0, 0.0, 8.0).expect("a");

    // Audio-only coverage at t=0.5: no video clip.
    assert_eq!(active_video_clip(engine.project(), 0.5), None);

    let active = active_video_clip(engine.project(), 2.0).expect("active");
    assert_eq!(active.clip_id, v);
    assert_eq!(active.source_time, 1.0);

    // Muted video track hides its clips.
    let mut project = engine.project().clone();
    project
        .tracks
        .iter_mut()
        .find(|t| t.kind == TrackKind::Video)
        .expect("video track")
        .muted = true;
    assert_eq!(active_video_clip(&project, 2.0), None);
    assert_eq!(active_video_clip(&project, f64::NAN), None);
}

#[test]
fn timeline_end_is_the_latest_out_point_across_tracks() {
    let Fixture {
        mut engine,
        media,
        video,
        audio,
    } = engine_with_media();
    assert_eq!(timeline_end(engine.project()), 0.0, "empty project");

    engine.add_clip(video, media, 0.0, 0.0, 2.0).expect("v");
    engine.add_clip(audio, media, 5.0, 0.0, 4.0).expect("a");
    assert_eq!(timeline_end(engine.project()), 9.0);

    // Muted tracks still count — muting hides, it doesn't shorten.
    let mut project = engine.project().clone();
    project
        .tracks
        .iter_mut()
        .find(|t| t.kind == TrackKind::Audio)
        .expect("audio track")
        .muted = true;
    assert_eq!(timeline_end(&project), 9.0);
}

#[test]
fn next_boundary_after_walks_clip_edges_in_order() {
    let Fixture {
        mut engine,
        media,
        video,
        audio,
    } = engine_with_media();
    engine.add_clip(video, media, 1.0, 0.0, 2.0).expect("v"); // edges 1, 3
    engine.add_clip(audio, media, 2.0, 0.0, 3.0).expect("a"); // edges 2, 5

    let project = engine.project();
    assert_eq!(next_boundary_after(project, 0.0), Some(1.0));
    assert_eq!(
        next_boundary_after(project, 1.0),
        Some(2.0),
        "strictly after"
    );
    assert_eq!(next_boundary_after(project, 2.5), Some(3.0));
    assert_eq!(next_boundary_after(project, 3.0), Some(5.0));
    assert_eq!(next_boundary_after(project, 5.0), None, "nothing after end");
    assert_eq!(next_boundary_after(project, f64::NAN), None);

    // Muted tracks contribute no boundaries (nothing to wait for there).
    let mut muted = project.clone();
    muted
        .tracks
        .iter_mut()
        .find(|t| t.kind == TrackKind::Audio)
        .expect("audio track")
        .muted = true;
    assert_eq!(next_boundary_after(&muted, 1.0), Some(3.0));
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

#[test]
fn snap_time_composes_candidates_and_snap() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    let clip = engine.add_clip(video, media, 4.0, 0.0, 2.0).expect("add");

    // Near the clip's start edge.
    assert_eq!(snap_time(engine.project(), 3.9, 0.2, None, &[]), Some(4.0));
    // Playhead is a candidate too.
    assert_eq!(
        snap_time(engine.project(), 1.05, 0.2, Some(1.0), &[]),
        Some(1.0)
    );
    // Excluding the clip removes its edges.
    assert_eq!(snap_time(engine.project(), 3.9, 0.2, None, &[clip]), None);
    // Out of range.
    assert_eq!(snap_time(engine.project(), 2.0, 0.2, None, &[]), None);
}

#[test]
fn snap_clip_move_snaps_the_nearer_edge() {
    let Fixture {
        mut engine,
        media,
        video,
        audio,
    } = engine_with_media();
    // Dragged: 2s long. Anchor clip on the audio track at [5, 8).
    let dragged = engine.add_clip(video, media, 0.0, 0.0, 2.0).expect("drag");
    engine
        .add_clip(audio, media, 5.0, 0.0, 3.0)
        .expect("anchor");

    // Start edge near 5.0 → snaps start to 5.0.
    let m = snap_clip_move(engine.project(), dragged, 4.92, 0.2, None).expect("snap");
    assert_eq!(m.timeline_in, 5.0);
    assert_eq!(m.snap_point, Some(5.0));

    // End edge near 5.0 (start would be 2.95, end 4.95) → end snaps to
    // 5.0, so the clip lands at 3.0.
    let m = snap_clip_move(engine.project(), dragged, 2.95, 0.2, None).expect("snap");
    assert_eq!(m.timeline_in, 3.0);
    assert_eq!(m.snap_point, Some(5.0));

    // Both edges in range of different candidates (start near 5.0, end
    // near 8.0): 5.95 → start dist 0.95 vs end dist 0.05 — end wins.
    let m = snap_clip_move(engine.project(), dragged, 5.95, 1.0, None).expect("snap");
    assert_eq!(m.timeline_in, 6.0);
    assert_eq!(m.snap_point, Some(8.0));

    // Nothing in range: position passes through untouched.
    let m = snap_clip_move(engine.project(), dragged, 20.0, 0.2, None).expect("snap");
    assert_eq!(m.timeline_in, 20.0);
    assert_eq!(m.snap_point, None);
}

#[test]
fn snap_clip_move_ignores_own_edges_and_rejects_unknown_clips() {
    let Fixture {
        mut engine,
        media,
        video,
        ..
    } = engine_with_media();
    let only = engine.add_clip(video, media, 3.0, 0.0, 2.0).expect("add");

    // The only candidates would be the dragged clip's own edges — excluded.
    let m = snap_clip_move(engine.project(), only, 3.05, 0.2, None).expect("snap");
    assert_eq!(m.snap_point, None, "must not snap to itself");

    assert_eq!(
        snap_clip_move(engine.project(), ClipId(999), 0.0, 0.2, None),
        Err(EngineError::UnknownClip(ClipId(999)))
    );
}
