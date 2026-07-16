//! Transition rendering: **preview == export** through both frontends
//! (the acceptance golden-frame fixtures for three transition kinds),
//! plus pixel-level checks of the two hard semantics — freeze-frame on a
//! zero-handle side, and two simultaneous decode sessions when a
//! transition joins two windows of the *same* file.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use cutty_engine::{Engine, MediaId, Project, ProjectSettings, TrackKind, Transition};
use cutty_media::{for_each_composited_frame, FrameSlice, TimelineRenderer};

const OUT_W: u32 = 1280;
const OUT_H: u32 = 720;
const FPS: f64 = 30.0;

fn test_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("cutty-transition-tests");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Deterministic time-varying source (fixed lavfi pattern + encode).
fn make_source(name: &str, pattern: &str, fps: u32, secs: u32) -> PathBuf {
    let file = test_dir().join(name);
    if file.is_file() {
        return file;
    }
    let status = Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
        .arg(format!("{pattern}=size=640x360:rate={fps}:duration={secs}"))
        .args([
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-g",
            "30",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&file)
        .status()
        .expect("system ffmpeg required");
    assert!(status.success());
    file
}

fn hash_frame(frame: &FrameSlice) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    let row_bytes = frame.width as usize * 4;
    for row in 0..frame.height as usize {
        hasher.update(&frame.data[row * frame.stride..row * frame.stride + row_bytes]);
    }
    hasher.finalize()
}

/// Copy a column range `[x0, x1)` of the packed pixels (region compares
/// for wipe transitions, staying clear of the wipe edge).
fn region_pixels(frame: &FrameSlice, x0: usize, x1: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity((x1 - x0) * 4 * frame.height as usize);
    for row in 0..frame.height as usize {
        let base = row * frame.stride;
        out.extend_from_slice(&frame.data[base + x0 * 4..base + x1 * 4]);
    }
    out
}

/// Regions must match within one 8-bit quantization step: the transition
/// path and the plain-layer path compute sampling coordinates through
/// differently-ordered float math (a v-flip round trip vs one affine),
/// which legitimately moves bilinear results by ±1 LSB on scattered
/// pixels. Preview == export stays bit-exact (same path on both sides);
/// this tolerance is only for comparing *different* paths.
fn assert_regions_match(got: &[u8], want: &[u8], what: &str) {
    assert_eq!(got.len(), want.len(), "{what}: region sizes differ");
    let worst = got
        .iter()
        .zip(want.iter())
        .map(|(a, b)| (i16::from(*a) - i16::from(*b)).abs())
        .max()
        .unwrap_or(0);
    assert!(worst <= 1, "{what}: pixels differ by up to {worst} LSB");
}

fn regions_differ(a: &[u8], b: &[u8]) -> bool {
    a.iter()
        .zip(b.iter())
        .any(|(x, y)| (i16::from(*x) - i16::from(*y)).abs() > 1)
}

fn originals_resolver(project: &Project) -> impl Fn(u64) -> Option<PathBuf> + '_ {
    move |media_id| {
        project
            .media
            .iter()
            .find(|m| m.id.0 == media_id)
            .map(|m| PathBuf::from(&m.path))
    }
}

fn gpu_available() -> bool {
    match TimelineRenderer::new(8, 8, false) {
        Ok(_) => true,
        Err(e) => {
            eprintln!("transition render tests: skipping, no GPU ({e})");
            false
        }
    }
}

fn video_track(engine: &Engine) -> cutty_engine::TrackId {
    engine
        .project()
        .tracks
        .iter()
        .find(|t| t.kind == TrackKind::Video)
        .unwrap()
        .id
}

fn tr(kind: &str, duration: f64) -> Option<Transition> {
    Some(Transition {
        kind: kind.into(),
        duration,
    })
}

/// The golden fixture: four clips over two sources (25 fps overlay
/// exercises the floor-sampling rule under a 30 fps grid), with three
/// different transitions on the three cuts.
fn fixture_project() -> (Project, i64) {
    let src_a = make_source("trans-a.mp4", "testsrc2", 30, 8);
    let src_b = make_source("trans-b.mp4", "testsrc", 25, 8);

    let mut engine = Engine::new(ProjectSettings {
        width: 1280,
        height: 720,
        fps: FPS,
    });
    let a = engine
        .add_media(src_a.display().to_string(), 8.0, true, false)
        .unwrap();
    let b = engine
        .add_media(src_b.display().to_string(), 8.0, true, false)
        .unwrap();
    let video = video_track(&engine);

    let c1 = engine.add_clip(video, a, 0.0, 0.5, 2.5).unwrap();
    let c2 = engine.add_clip(video, b, 2.0, 3.0, 5.0).unwrap();
    let c3 = engine.add_clip(video, a, 4.0, 4.0, 6.0).unwrap();
    let _c4 = engine.add_clip(video, b, 6.0, 1.0, 2.5).unwrap();

    engine.set_transition(c1, tr("fade", 0.6)).unwrap();
    engine.set_transition(c2, tr("wiperight", 0.8)).unwrap();
    engine.set_transition(c3, tr("circleopen", 0.5)).unwrap();

    let project = engine.project().clone();
    project.validate().expect("fixture is valid");
    let total_frames = (7.5 * FPS).round() as i64;
    (project, total_frames)
}

/// Acceptance: golden-frame fixtures for three transition types pass
/// preview == export, bit-exactly, over every output frame.
#[test]
fn transition_fixture_composites_identically_in_both_frontends() {
    if !gpu_available() {
        return;
    }
    let (project, total_frames) = fixture_project();

    // Preview frontend: sequential render_with, like playback/scrub.
    let mut preview = TimelineRenderer::new(OUT_W, OUT_H, false).expect("gpu");
    let resolver = originals_resolver(&project);
    let mut preview_hashes: Vec<blake3::Hash> = Vec::new();
    for idx in 0..total_frames {
        let t = idx as f64 / FPS;
        let hash = preview
            .render_with(&project, t, &resolver, |frame| hash_frame(&frame))
            .expect("preview frame renders");
        preview_hashes.push(hash);
    }
    assert!(
        preview.take_issues().is_empty(),
        "preview must render every layer of every transition"
    );

    // Export frontend: the literal export frame loop.
    let mut export_hashes: HashMap<i64, blake3::Hash> = HashMap::new();
    for_each_composited_frame(
        &project,
        OUT_W,
        OUT_H,
        FPS,
        total_frames,
        &|| false,
        &mut |idx, data, stride| {
            export_hashes.insert(
                idx,
                hash_frame(&FrameSlice {
                    width: OUT_W,
                    height: OUT_H,
                    stride,
                    data,
                }),
            );
            Ok(())
        },
    )
    .expect("export frames render");

    assert_eq!(export_hashes.len() as i64, total_frames);
    let mut mismatches = 0;
    for (idx, preview_hash) in preview_hashes.iter().enumerate() {
        if *preview_hash != export_hashes[&(idx as i64)] {
            eprintln!("frame {idx}: preview != export");
            mismatches += 1;
        }
    }
    assert_eq!(
        mismatches, 0,
        "{mismatches} of {total_frames} frames differ between preview and export"
    );

    // And the transitions actually change the picture: mid-span frames
    // must differ from the same project with the transitions removed.
    let mut without = project.clone();
    for track in &mut without.tracks {
        for clip in &mut track.clips {
            clip.transition_out = None;
        }
    }
    let mut renderer = TimelineRenderer::new(OUT_W, OUT_H, false).expect("gpu");
    for cut in [2.0, 4.0, 6.0] {
        let probe = ((cut - 0.1) * FPS).round() / FPS; // inside each span
        let with_hash = renderer
            .render_with(&project, probe, &resolver, |f| hash_frame(&f))
            .expect("renders");
        let without_hash = renderer
            .render_with(&without, probe, &resolver, |f| hash_frame(&f))
            .expect("renders");
        assert_ne!(
            with_hash, without_hash,
            "transition at cut {cut} must change the mid-span frame"
        );
    }
}

/// Acceptance: a transition on a clip with **no outgoing handle** (its
/// source ends exactly at the cut) freeze-frames that side across the
/// overlap. Verified pixel-exactly: the FROM region of a wipe, past the
/// cut, equals the source's final frame.
#[test]
fn zero_handle_outgoing_side_freezes_on_its_last_frame() {
    if !gpu_available() {
        return;
    }
    let src_a = make_source("trans-freeze-a.mp4", "testsrc2", 30, 4);
    let src_b = make_source("trans-freeze-b.mp4", "testsrc", 30, 4);

    let mut engine = Engine::new(ProjectSettings {
        width: 1280,
        height: 720,
        fps: FPS,
    });
    let a = engine
        .add_media(src_a.display().to_string(), 4.0, true, false)
        .unwrap();
    let b = engine
        .add_media(src_b.display().to_string(), 4.0, true, false)
        .unwrap();
    let video = video_track(&engine);
    // A's source range ends exactly at the media end: zero handle.
    let c1 = engine.add_clip(video, a, 0.0, 2.0, 4.0).unwrap();
    let _c2 = engine.add_clip(video, b, 2.0, 0.5, 2.5).unwrap();
    engine.set_transition(c1, tr("wipeleft", 0.5)).unwrap();
    let project = engine.project().clone();

    // Span [1.75, 2.25). Probe past the cut at t = 2.1 (grid frame 63):
    // A's extended source time is 4.1s — beyond the 4s file — so the
    // FROM side must hold the file's final frame. wipeleft at progress
    // 0.7 keeps FROM on x/W < 0.3.
    let t_probe = 63.0 / FPS;
    let resolver = originals_resolver(&project);
    let mut renderer = TimelineRenderer::new(OUT_W, OUT_H, false).expect("gpu");
    let x1 = (OUT_W as f64 * 0.28) as usize;
    let frozen_region = renderer
        .render_with(&project, t_probe, &resolver, |f| region_pixels(&f, 0, x1))
        .expect("transition frame renders");

    // Reference: the same file full-length, at its final frame (119).
    let mut ref_engine = Engine::new(ProjectSettings {
        width: 1280,
        height: 720,
        fps: FPS,
    });
    let ra = ref_engine
        .add_media(src_a.display().to_string(), 4.0, true, false)
        .unwrap();
    let rv = video_track(&ref_engine);
    ref_engine.add_clip(rv, ra, 0.0, 0.0, 4.0).unwrap();
    let ref_project = ref_engine.project().clone();
    let ref_resolver = originals_resolver(&ref_project);
    let last_frame_region = renderer
        .render_with(&ref_project, 119.0 / FPS, &ref_resolver, |f| {
            region_pixels(&f, 0, x1)
        })
        .expect("reference renders");

    assert_regions_match(
        &frozen_region,
        &last_frame_region,
        "the zero-handle FROM side must hold the source's final frame",
    );

    // Sanity: the freeze really engaged (the held frame differs from the
    // frame at the cut itself, one frame earlier in source time).
    let at_cut_region = renderer
        .render_with(&ref_project, 110.0 / FPS, &ref_resolver, |f| {
            region_pixels(&f, 0, x1)
        })
        .expect("reference renders");
    assert!(
        regions_differ(&frozen_region, &at_cut_region),
        "testsrc2 frames must vary"
    );
}

/// A transition joining two windows of the *same file* needs two decode
/// sessions at once. Each side must show its own (extended) source time —
/// verified against single-clip reference renders of the same file.
#[test]
fn same_media_transition_streams_two_offsets_at_once() {
    if !gpu_available() {
        return;
    }
    let src = make_source("trans-samemedia.mp4", "testsrc2", 30, 8);

    let mut engine = Engine::new(ProjectSettings {
        width: 1280,
        height: 720,
        fps: FPS,
    });
    let a = engine
        .add_media(src.display().to_string(), 8.0, true, false)
        .unwrap();
    let video = video_track(&engine);
    let c1 = engine.add_clip(video, a, 0.0, 1.0, 3.0).unwrap();
    let _c2 = engine.add_clip(video, a, 2.0, 5.0, 7.0).unwrap();
    engine.set_transition(c1, tr("wipeleft", 1.0)).unwrap();
    let project = engine.project().clone();

    // Probe t = 2.2 (frame 66), progress 0.7: FROM (source 3.2s) on
    // x/W < 0.3, TO (source 5.2s) on x/W ≥ 0.3.
    let t_probe = 66.0 / FPS;
    let resolver = originals_resolver(&project);
    let mut renderer = TimelineRenderer::new(OUT_W, OUT_H, false).expect("gpu");
    let left_end = (OUT_W as f64 * 0.28) as usize;
    let right_start = (OUT_W as f64 * 0.32) as usize;
    let (from_region, to_region) = renderer
        .render_with(&project, t_probe, &resolver, |f| {
            (
                region_pixels(&f, 0, left_end),
                region_pixels(&f, right_start, OUT_W as usize),
            )
        })
        .expect("transition frame renders");
    assert!(
        renderer.take_issues().is_empty(),
        "both sessions must decode"
    );

    // References: the whole file as one clip, at each side's source time.
    let mut ref_engine = Engine::new(ProjectSettings {
        width: 1280,
        height: 720,
        fps: FPS,
    });
    let ra = ref_engine
        .add_media(src.display().to_string(), 8.0, true, false)
        .unwrap();
    let rv = video_track(&ref_engine);
    ref_engine.add_clip(rv, ra, 0.0, 0.0, 8.0).unwrap();
    let ref_project = ref_engine.project().clone();
    let ref_resolver = originals_resolver(&ref_project);
    let want_from = renderer
        .render_with(&ref_project, 3.2, &ref_resolver, |f| {
            region_pixels(&f, 0, left_end)
        })
        .expect("reference renders");
    let want_to = renderer
        .render_with(&ref_project, 5.2, &ref_resolver, |f| {
            region_pixels(&f, right_start, OUT_W as usize)
        })
        .expect("reference renders");

    assert_regions_match(&from_region, &want_from, "FROM side shows source 3.2s");
    assert_regions_match(&to_region, &want_to, "TO side shows source 5.2s");
    let common = from_region.len().min(to_region.len());
    assert!(
        regions_differ(&from_region[..common], &to_region[..common]),
        "the two offsets differ"
    );
}

/// Trimming the incoming clip shorter than the transition clamps the
/// effective span — the frame just outside the clamped span renders
/// exactly like a project with no transition at all (no residue).
#[test]
fn trimmed_neighbor_clamps_the_rendered_span() {
    if !gpu_available() {
        return;
    }
    let src_a = make_source("trans-clamp-a.mp4", "testsrc2", 30, 4);
    let src_b = make_source("trans-clamp-b.mp4", "testsrc", 30, 4);

    let mut engine = Engine::new(ProjectSettings {
        width: 1280,
        height: 720,
        fps: FPS,
    });
    let a = engine
        .add_media(src_a.display().to_string(), 4.0, true, false)
        .unwrap();
    let b = engine
        .add_media(src_b.display().to_string(), 4.0, true, false)
        .unwrap();
    let video = video_track(&engine);
    let c1 = engine.add_clip(video, a, 0.0, 0.5, 2.5).unwrap();
    let c2 = engine.add_clip(video, b, 2.0, 1.0, 3.0).unwrap();
    engine.set_transition(c1, tr("fade", 1.0)).unwrap();

    // Shrink B to 0.4s: effective span clamps to [1.8, 2.2).
    engine
        .trim_clip(c2, cutty_engine::TrimEdge::End, 2.4)
        .unwrap();
    let project = engine.project().clone();
    let mut without = project.clone();
    without.tracks[0].clips[0].transition_out = None;

    let resolver = originals_resolver(&project);
    let mut renderer = TimelineRenderer::new(OUT_W, OUT_H, false).expect("gpu");
    // Frame 52 (t ≈ 1.733) is inside the *requested* 1s span but outside
    // the clamped 0.4s span: it must render as a plain A frame.
    let outside = renderer
        .render_with(&project, 52.0 / FPS, &resolver, |f| hash_frame(&f))
        .expect("renders");
    let plain = renderer
        .render_with(&without, 52.0 / FPS, &resolver, |f| hash_frame(&f))
        .expect("renders");
    assert_eq!(outside, plain, "outside the clamped span: no transition");

    // Frame 59 (t ≈ 1.967) is inside the clamped span: pair rendering.
    let inside = renderer
        .render_with(&project, 59.0 / FPS, &resolver, |f| hash_frame(&f))
        .expect("renders");
    let plain_inside = renderer
        .render_with(&without, 59.0 / FPS, &resolver, |f| hash_frame(&f))
        .expect("renders");
    assert_ne!(inside, plain_inside, "inside the clamped span: blended");
}

/// Unknown transition kinds (a project from a newer build) degrade to a
/// crossfade with a reported issue instead of failing the render.
#[test]
fn unknown_transition_kind_falls_back_to_fade() {
    if !gpu_available() {
        return;
    }
    let src_a = make_source("trans-a.mp4", "testsrc2", 30, 8);
    let src_b = make_source("trans-b.mp4", "testsrc", 25, 8);
    let mut engine = Engine::new(ProjectSettings {
        width: 1280,
        height: 720,
        fps: FPS,
    });
    let a = engine
        .add_media(src_a.display().to_string(), 8.0, true, false)
        .unwrap();
    let b = engine
        .add_media(src_b.display().to_string(), 8.0, true, false)
        .unwrap();
    let video = video_track(&engine);
    let c1 = engine.add_clip(video, a, 0.0, 0.5, 2.5).unwrap();
    let _c2 = engine.add_clip(video, b, 2.0, 3.0, 5.0).unwrap();
    engine.set_transition(c1, tr("fade", 0.6)).unwrap();
    let fade_project = engine.project().clone();

    let mut future_project = fade_project.clone();
    future_project.tracks[0].clips[0]
        .transition_out
        .as_mut()
        .unwrap()
        .kind = "quantum-teleport".into();

    let resolver = originals_resolver(&fade_project);
    let mut renderer = TimelineRenderer::new(OUT_W, OUT_H, false).expect("gpu");
    let t_probe = 59.0 / FPS; // mid-span
    let as_fade = renderer
        .render_with(&fade_project, t_probe, &resolver, |f| hash_frame(&f))
        .expect("renders");
    let as_unknown = renderer
        .render_with(&future_project, t_probe, &resolver, |f| hash_frame(&f))
        .expect("renders");
    assert_eq!(as_unknown, as_fade, "unknown kind renders as fade");
    let issues = renderer.take_issues();
    assert!(
        issues.iter().any(|i| i.contains("quantum-teleport")),
        "must report the unknown kind once: {issues:?}"
    );
}

// Keep MediaId import used even when GPU tests skip.
#[allow(dead_code)]
fn _keep(_: MediaId) {}
