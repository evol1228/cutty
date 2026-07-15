//! Golden-frame tests: **preview == export**, enforced.
//!
//! A fixture project (two overlapping video tracks, transforms, opacity,
//! every blend mode) renders through BOTH frontends at the same
//! resolution from the same sources:
//!
//! - the preview frontend's path: [`cutty_media::TimelineRenderer`] with
//!   synchronous per-frame readback (exactly what playback/scrub run),
//! - the export frontend's path: [`cutty_media::for_each_composited_frame`]
//!   (the literal export frame generator, double-buffered readback and
//!   all).
//!
//! Raw RGBA composites are hashed per frame and compared **bit-exactly**
//! (no JPEG, no encoder — those sit after the compared boundary). This is
//! the mechanism that keeps the two frontends welded together; Phase 3's
//! "exports pixel-identical to preview" acceptance rides on it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use cutty_engine::{
    BlendMode, Clip, ClipId, Engine, Project, ProjectSettings, Track, TrackId, TrackKind, Transform,
};
use cutty_media::{for_each_composited_frame, FrameSlice, TimelineRenderer};

const OUT_W: u32 = 1280;
const OUT_H: u32 = 720;
const FPS: f64 = 30.0;

fn test_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("cutty-golden-tests");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Deterministic source clip (fixed lavfi pattern, fixed encode).
fn make_source(name: &str, pattern: &str, size: &str, fps: u32) -> PathBuf {
    let file = test_dir().join(name);
    if file.is_file() {
        return file;
    }
    let status = Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
        .arg(format!("{pattern}=size={size}:rate={fps}:duration=4"))
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

/// The golden fixture: base track (V1) playing a gradient source, overlay
/// track (V2, above it) playing a second source in five back-to-back
/// windows — one per blend mode — each transformed (offset, scaled,
/// rotated) and at 70% opacity. A 25 fps overlay over a 30 fps output
/// also exercises the floor-frame sampling rule.
fn fixture_project() -> (Project, i64) {
    let src_a = make_source("golden-a.mp4", "testsrc2", "640x360", 30);
    let src_b = make_source("golden-b.mp4", "testsrc", "320x240", 25);

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
    let video = engine
        .project()
        .tracks
        .iter()
        .find(|t| t.kind == TrackKind::Video)
        .unwrap()
        .id;
    // Base layer: [0, 3.2) from source 0.4.
    engine.add_clip(video, a, 0.0, 0.4, 3.6).unwrap();

    // Overlay track above the base (index 0 = top of the panel).
    let mut project = engine.project().clone();
    project.tracks.insert(
        0,
        Track {
            id: TrackId(100),
            kind: TrackKind::Video,
            name: "V2".into(),
            locked: false,
            muted: false,
            hidden: false,
            clips: Vec::new(),
        },
    );
    let modes = [
        BlendMode::Normal,
        BlendMode::Multiply,
        BlendMode::Screen,
        BlendMode::Overlay,
        BlendMode::Add,
    ];
    for (i, mode) in modes.iter().enumerate() {
        let t_in = 0.5 + i as f64 * 0.5;
        project.tracks[0].clips.push(Clip {
            id: ClipId(200 + i as u64),
            media_id: b,
            timeline_in: t_in,
            timeline_out: t_in + 0.5,
            source_in: 0.2,
            source_out: 0.7,
            transform: Transform {
                x: 220.0,
                y: -120.0,
                scale: 0.45,
                rotation: 25.0,
            },
            opacity: 0.7,
            blend_mode: *mode,
            speed: 1.0,
            volume: 1.0,
                transition_out: None,
        });
    }
    project.validate().expect("fixture is valid");

    let total_frames = (3.2 * FPS).round() as i64;
    (project, total_frames)
}

/// Hash the packed pixel rows of a composited frame (padding stripped —
/// the compared bytes are exactly the picture).
fn hash_frame(frame: &FrameSlice) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    let row_bytes = frame.width as usize * 4;
    for row in 0..frame.height as usize {
        hasher.update(&frame.data[row * frame.stride..row * frame.stride + row_bytes]);
    }
    hasher.finalize()
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
            eprintln!("golden tests: skipping, no GPU ({e})");
            false
        }
    }
}

/// The enforcement test: every output frame of the fixture, rendered by
/// the preview frontend and by the export frame generator, hashes
/// identically.
#[test]
fn preview_and_export_frontends_composite_bit_identically() {
    if !gpu_available() {
        return;
    }
    let (project, total_frames) = fixture_project();

    // Preview frontend: sequential render_with, like playback does.
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
        "preview must render every layer"
    );

    // Export frontend: the literal export frame loop (double-buffered).
    let mut export_hashes: HashMap<i64, blake3::Hash> = HashMap::new();
    for_each_composited_frame(
        &project,
        OUT_W,
        OUT_H,
        FPS,
        total_frames,
        &|| false,
        &mut |idx, data, stride| {
            let frame = FrameSlice {
                width: OUT_W,
                height: OUT_H,
                stride,
                data,
            };
            export_hashes.insert(idx, hash_frame(&frame));
            Ok(())
        },
    )
    .expect("export frames render");

    assert_eq!(export_hashes.len() as i64, total_frames);
    let mut mismatches = 0;
    for (idx, preview_hash) in preview_hashes.iter().enumerate() {
        let export_hash = export_hashes[&(idx as i64)];
        if *preview_hash != export_hash {
            eprintln!("frame {idx}: preview {preview_hash} != export {export_hash}");
            mismatches += 1;
        }
    }
    assert_eq!(
        mismatches, 0,
        "{mismatches} of {total_frames} frames differ between preview and export"
    );
}

/// Each blend mode must actually change the picture (a shader that
/// ignores the blend uniform would still pass the identity test above).
#[test]
fn blend_modes_produce_distinct_composites() {
    if !gpu_available() {
        return;
    }
    let (project, _) = fixture_project();
    let resolver_project = project.clone();
    let mut renderer = TimelineRenderer::new(OUT_W, OUT_H, false).expect("gpu");

    // One frame inside each overlay window; alter every overlay clip to a
    // single mode per pass so the *only* variable is the blend mode.
    let t_probe = 0.75; // inside the first overlay window
    let modes = [
        BlendMode::Normal,
        BlendMode::Multiply,
        BlendMode::Screen,
        BlendMode::Overlay,
        BlendMode::Add,
    ];
    let mut hashes = Vec::new();
    for mode in modes {
        let mut variant = resolver_project.clone();
        for clip in &mut variant.tracks[0].clips {
            clip.blend_mode = mode;
        }
        let resolver = originals_resolver(&resolver_project);
        let hash = renderer
            .render_with(&variant, t_probe, &resolver, |frame| hash_frame(&frame))
            .expect("frame renders");
        hashes.push((mode, hash));
    }
    for i in 0..hashes.len() {
        for j in (i + 1)..hashes.len() {
            assert_ne!(
                hashes[i].1, hashes[j].1,
                "{:?} and {:?} composited identically",
                hashes[i].0, hashes[j].0
            );
        }
    }
}

/// Transform and opacity must change the picture too — this is the
/// "transform/opacity live" guarantee of the session.
#[test]
fn transform_and_opacity_change_the_composite() {
    if !gpu_available() {
        return;
    }
    let (project, _) = fixture_project();
    let mut renderer = TimelineRenderer::new(OUT_W, OUT_H, false).expect("gpu");
    let t_probe = 0.75;

    let base_hash = {
        let resolver = originals_resolver(&project);
        renderer
            .render_with(&project, t_probe, &resolver, |frame| hash_frame(&frame))
            .expect("renders")
    };

    let mut moved = project.clone();
    moved.tracks[0].clips[0].transform.x = -220.0;
    let resolver = originals_resolver(&project);
    let moved_hash = renderer
        .render_with(&moved, t_probe, &resolver, |frame| hash_frame(&frame))
        .expect("renders");
    assert_ne!(base_hash, moved_hash, "transform.x must move the overlay");

    let mut faded = project.clone();
    faded.tracks[0].clips[0].opacity = 0.2;
    let faded_hash = renderer
        .render_with(&faded, t_probe, &resolver, |frame| hash_frame(&frame))
        .expect("renders");
    assert_ne!(base_hash, faded_hash, "opacity must change the blend");

    // And identical inputs are deterministic (fresh renderer, same hash).
    let mut renderer2 = TimelineRenderer::new(OUT_W, OUT_H, false).expect("gpu");
    let again = renderer2
        .render_with(&project, t_probe, &resolver, |frame| hash_frame(&frame))
        .expect("renders");
    assert_eq!(base_hash, again, "compositing must be deterministic");
}

/// Preview-path throughput probe (numbers for the perf log, not a gate):
/// times the full per-frame preview pipeline — decode + upload +
/// composite + synchronous readback — at 1280×720 across the fixture.
#[test]
#[ignore = "perf probe; run with --ignored --nocapture for numbers"]
fn preview_renderer_throughput_probe() {
    if !gpu_available() {
        return;
    }
    let (project, total_frames) = fixture_project();
    let mut renderer = TimelineRenderer::new(OUT_W, OUT_H, false).expect("gpu");
    let resolver = originals_resolver(&project);

    // Warm decoders on frame 0, then time a sequential run.
    renderer
        .render_with(&project, 0.0, &resolver, |_| ())
        .expect("warmup");
    let mut readback = std::time::Duration::ZERO;
    let started = std::time::Instant::now();
    let mut bytes = 0u64;
    for idx in 0..total_frames {
        let t = idx as f64 / FPS;
        renderer
            .begin_frame(&project, t, &resolver, 0)
            .expect("frame");
        let t0 = std::time::Instant::now();
        renderer
            .read_frame(0, |frame| {
                bytes += (frame.width as u64 * 4) * frame.height as u64;
            })
            .expect("readback");
        readback += t0.elapsed();
    }
    let elapsed = started.elapsed();
    let stats = renderer.stats();
    println!(
        "preview pipeline @{OUT_W}x{OUT_H}: {total_frames} frames in {elapsed:.2?} \
         ({:.1} fps sustained), readback {:.2} ms/frame ({:.2} GB/s), stats {stats:?}",
        total_frames as f64 / elapsed.as_secs_f64(),
        readback.as_secs_f64() * 1e3 / total_frames as f64,
        (bytes as f64 / 1e9) / readback.as_secs_f64(),
    );
}

/// Hidden overlay tracks drop out of the composite in BOTH frontends
/// (hide is a resolver-level exclusion, so preview and export respect it
/// via the same code path — observed here through pixels). Muting, by
/// contrast, only silences audio: the picture must not change.
#[test]
fn hidden_overlay_track_is_invisible_in_both_frontends() {
    if !gpu_available() {
        return;
    }
    let (project, _) = fixture_project();
    let mut renderer = TimelineRenderer::new(OUT_W, OUT_H, false).expect("gpu");
    // On the output frame grid (the export loop renders grid frames only),
    // inside the first overlay window.
    let probe_frame: i64 = 22;
    let t_probe = probe_frame as f64 / FPS;
    let resolver = originals_resolver(&project);

    let with_overlay = renderer
        .render_with(&project, t_probe, &resolver, |frame| hash_frame(&frame))
        .expect("renders");

    // Muted ≠ hidden: the muted composite is pixel-identical to the full one.
    let mut muted = project.clone();
    muted.tracks[0].muted = true;
    let muted_hash = renderer
        .render_with(&muted, t_probe, &resolver, |frame| hash_frame(&frame))
        .expect("renders");
    assert_eq!(muted_hash, with_overlay, "mute must not change the picture");

    let mut base_only = project.clone();
    base_only.tracks[0].hidden = true;
    let hidden_hash = renderer
        .render_with(&base_only, t_probe, &resolver, |frame| hash_frame(&frame))
        .expect("renders");
    assert_ne!(hidden_hash, with_overlay, "overlay must be visible");

    // The hidden composite equals a project that never had the track.
    let mut without = project.clone();
    without.tracks.remove(0);
    let without_hash = renderer
        .render_with(&without, t_probe, &resolver, |frame| hash_frame(&frame))
        .expect("renders");
    assert_eq!(hidden_hash, without_hash);

    // And the export frontend agrees frame-for-frame: the hidden-track
    // project renders bit-identically to the track-less one at the probe
    // frame through the literal export loop.
    let export_hash_of = |p: &Project| {
        let mut hash = None;
        for_each_composited_frame(
            p,
            OUT_W,
            OUT_H,
            FPS,
            probe_frame + 1,
            &|| false,
            &mut |idx, data, stride| {
                if idx == probe_frame {
                    hash = Some(hash_frame(&FrameSlice {
                        width: OUT_W,
                        height: OUT_H,
                        stride,
                        data,
                    }));
                }
                Ok(())
            },
        )
        .expect("export frames render");
        hash.expect("probe frame rendered")
    };
    assert_eq!(
        export_hash_of(&base_only),
        export_hash_of(&without),
        "export must exclude hidden tracks exactly like preview"
    );
    assert_eq!(
        export_hash_of(&base_only),
        hidden_hash,
        "preview and export agree on the hidden composite"
    );
}
