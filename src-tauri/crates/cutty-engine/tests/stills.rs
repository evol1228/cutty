//! Still-image and looping-GIF media: the unbounded-source invariants.
//!
//! Stills have no intrinsic duration (stored as 0) and GIFs loop to fill
//! their clip, so neither bounds its clips' source range. These tests pin
//! the exemption and every operation that touches it: add (default
//! length), trim (free extension both ways, loop-phase folding), split
//! (source continuity), transitions (unlimited handles), and validation
//! (bounded media still reject out-of-range sources).

mod common;

use common::{engine_with_media, span, Fixture};
use cutty_engine::{
    project_file, EngineError, MediaId, MediaKind, Transition, TrimEdge,
    DEFAULT_STILL_CLIP_DURATION, EPS,
};

/// Register a still image in the fixture project.
fn add_image(f: &mut Fixture) -> MediaId {
    f.engine
        .add_media_with_kind("/tmp/poster.png", 0.0, true, false, true, MediaKind::Image)
        .expect("add image media")
}

/// Register a 2s looping GIF in the fixture project.
fn add_gif(f: &mut Fixture) -> MediaId {
    f.engine
        .add_media_with_kind("/tmp/sticker.gif", 2.0, true, false, true, MediaKind::Gif)
        .expect("add gif media")
}

#[test]
fn image_clip_gets_the_default_still_duration() {
    let mut f = engine_with_media();
    let image = add_image(&mut f);
    // The natural caller shape: source range derived from duration 0.
    let clip = f.engine.add_clip(f.video, image, 1.0, 0.0, 0.0).unwrap();
    assert_eq!(
        span(&f.engine, clip),
        (1.0, 1.0 + DEFAULT_STILL_CLIP_DURATION, 0.0, DEFAULT_STILL_CLIP_DURATION)
    );
}

#[test]
fn image_clip_honors_an_explicit_source_range() {
    let mut f = engine_with_media();
    let image = add_image(&mut f);
    let clip = f.engine.add_clip(f.video, image, 0.0, 0.0, 12.5).unwrap();
    assert_eq!(span(&f.engine, clip), (0.0, 12.5, 0.0, 12.5));
}

#[test]
fn image_media_stores_zero_duration_regardless_of_probe() {
    let mut f = engine_with_media();
    let image = f
        .engine
        .add_media_with_kind("/tmp/p.png", 0.04, true, false, true, MediaKind::Image)
        .unwrap();
    assert_eq!(f.engine.project().media(image).unwrap().duration, 0.0);
}

#[test]
fn image_clip_extends_freely_at_both_edges() {
    let mut f = engine_with_media();
    let image = add_image(&mut f);
    let clip = f.engine.add_clip(f.video, image, 10.0, 0.0, 5.0).unwrap();

    // End: far past any media duration.
    let applied = f.engine.trim_clip(clip, TrimEdge::End, 40.0).unwrap();
    assert_eq!(applied, 40.0);
    // Start: extend left, well before the original in-point.
    let applied = f.engine.trim_clip(clip, TrimEdge::Start, 2.0).unwrap();
    assert_eq!(applied, 2.0);
    // Source range stays normalized to the clip duration.
    assert_eq!(span(&f.engine, clip), (2.0, 40.0, 0.0, 38.0));
}

#[test]
fn gif_clip_extends_past_media_duration_and_keeps_loop_phase() {
    let mut f = engine_with_media();
    let gif = add_gif(&mut f);
    let clip = f.engine.add_clip(f.video, gif, 5.0, 0.0, 2.0).unwrap();

    // Extend the end to loop 4x.
    f.engine.trim_clip(clip, TrimEdge::End, 13.0).unwrap();
    assert_eq!(span(&f.engine, clip), (5.0, 13.0, 0.0, 8.0));

    // Trim the start 0.5s in: phase advances with the drag.
    f.engine.trim_clip(clip, TrimEdge::Start, 5.5).unwrap();
    assert_eq!(span(&f.engine, clip), (5.5, 13.0, 0.5, 8.0));

    // Extend the start left past the loop origin: phase folds into
    // [0, period) instead of going negative, span linkage intact.
    f.engine.trim_clip(clip, TrimEdge::Start, 4.25).unwrap();
    let (tin, tout, sin, sout) = span(&f.engine, clip);
    assert_eq!((tin, tout), (4.25, 13.0));
    // Requested phase would be 0.5 - 1.25 = -0.75 → folds to 1.25.
    assert!((sin - 1.25).abs() < EPS, "sin = {sin}");
    assert!((sout - sin - (tout - tin)).abs() < EPS, "span linkage");
}

#[test]
fn split_still_and_gif_clips_keeps_source_continuity() {
    let mut f = engine_with_media();
    let gif = add_gif(&mut f);
    let clip = f.engine.add_clip(f.video, gif, 0.0, 0.0, 9.0).unwrap();

    let right = f.engine.split_clip(clip, 4.0).unwrap();
    assert_eq!(span(&f.engine, clip), (0.0, 4.0, 0.0, 4.0));
    // The right half keeps the source phase where the left ended — the
    // loop plays seamlessly across the cut.
    assert_eq!(span(&f.engine, right), (4.0, 9.0, 4.0, 9.0));

    // And validation accepts the halves (source_out > media duration).
    assert!(f.engine.project().validate().is_ok());
}

#[test]
fn bounded_media_still_rejects_source_overrun() {
    let mut f = engine_with_media();
    // The fixture's 10s video: a 12s source range must fail.
    let err = f
        .engine
        .add_clip(f.video, f.media, 0.0, 0.0, 12.0)
        .unwrap_err();
    assert!(matches!(err, EngineError::SourceOutOfBounds { .. }));

    // And trims clamp to the media end exactly as before.
    let clip = f.engine.add_clip(f.video, f.media, 0.0, 0.0, 10.0).unwrap();
    let applied = f.engine.trim_clip(clip, TrimEdge::End, 25.0).unwrap();
    assert!((applied - 10.0).abs() < EPS);
}

#[test]
fn image_and_gif_transition_handles_are_unlimited() {
    let mut f = engine_with_media();
    let image = add_image(&mut f);
    let gif = add_gif(&mut f);
    // Two 4s clips back to back; neither has any "handle" in the bounded
    // sense (image full-window, gif at loop origin), but both sides are
    // unbounded so the limit is the clip-length bound alone.
    let a = f.engine.add_clip(f.video, image, 0.0, 0.0, 4.0).unwrap();
    let _b = f.engine.add_clip(f.video, gif, 4.0, 0.0, 4.0).unwrap();
    f.engine
        .set_transition(
            a,
            Some(Transition {
                kind: "fade".into(),
                duration: 3.0,
            }),
        )
        .expect("transition binds");
    let spans = cutty_engine::transition_spans(f.engine.project());
    assert_eq!(spans.len(), 1);
    assert!((spans[0].max_duration - 4.0).abs() < EPS, "clip-length bound only");
    assert!((spans[0].end - spans[0].start - 3.0).abs() < EPS, "full requested span");
}

#[test]
fn kind_mismatches_are_rejected() {
    let mut f = engine_with_media();
    // An "image" that claims an audio stream is malformed.
    let err = f
        .engine
        .add_media_with_kind("/tmp/x.png", 0.0, true, true, true, MediaKind::Image)
        .unwrap_err();
    assert!(matches!(err, EngineError::InvalidMedia { .. }));
    // A gif must have a positive intrinsic duration (it is a loop period).
    let err = f
        .engine
        .add_media_with_kind("/tmp/x.gif", 0.0, true, false, true, MediaKind::Gif)
        .unwrap_err();
    assert!(matches!(err, EngineError::InvalidMedia { .. }));
    // Audio kind with a video stream is malformed.
    let err = f
        .engine
        .add_media_with_kind("/tmp/x.mp3", 3.0, true, true, false, MediaKind::Audio)
        .unwrap_err();
    assert!(matches!(err, EngineError::InvalidMedia { .. }));
}

#[test]
fn image_clips_cannot_land_on_audio_tracks() {
    let mut f = engine_with_media();
    let image = add_image(&mut f);
    let err = f
        .engine
        .add_clip(f.audio, image, 0.0, 0.0, 5.0)
        .unwrap_err();
    assert!(matches!(err, EngineError::IncompatibleMedia { .. }));
}

#[test]
fn undo_redo_round_trips_still_edits() {
    let mut f = engine_with_media();
    let image = add_image(&mut f);
    let clip = f.engine.add_clip(f.video, image, 0.0, 0.0, 0.0).unwrap();
    f.engine.trim_clip(clip, TrimEdge::End, 20.0).unwrap();
    let extended = common::serialized(&f.engine);

    f.engine.undo().unwrap();
    assert_eq!(
        span(&f.engine, clip),
        (0.0, DEFAULT_STILL_CLIP_DURATION, 0.0, DEFAULT_STILL_CLIP_DURATION)
    );
    f.engine.redo().unwrap();
    assert_eq!(common::serialized(&f.engine), extended);
}

#[test]
fn still_projects_save_and_load() {
    let mut f = engine_with_media();
    let image = add_image(&mut f);
    let gif = add_gif(&mut f);
    let a = f.engine.add_clip(f.video, image, 0.0, 0.0, 0.0).unwrap();
    let b = f.engine.add_clip(f.video, gif, 5.0, 0.0, 2.0).unwrap();
    f.engine.trim_clip(b, TrimEdge::End, 11.0).unwrap();

    let json = project_file::serialize(f.engine.project(), None);
    assert!(json.contains("\"kind\": \"image\""));
    assert!(json.contains("\"kind\": \"gif\""));
    let loaded = project_file::deserialize(&json, None).expect("loads");
    assert_eq!(&loaded, f.engine.project());
    assert!(loaded.find_clip(a).is_some() && loaded.find_clip(b).is_some());
}
