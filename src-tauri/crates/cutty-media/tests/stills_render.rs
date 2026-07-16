//! Render tests for the Phase 2 sticker primitives: still images hold
//! one frame, GIFs loop to fill their clip, WebM-alpha overlays keep
//! their transparency through the compositor, and a translucent
//! transition side never takes the opaque direct fast path.
//!
//! All sources are generated with lavfi (cached per test dir); every
//! test skips visibly when no GPU is present, like the golden suite.

use std::path::PathBuf;
use std::process::Command;

use cutty_engine::{
    Clip, ClipId, Engine, MediaKind, Project, ProjectSettings, Track, TrackId, TrackKind,
    Transform, Transition,
};
use cutty_media::{for_each_composited_frame, FrameSlice, TimelineRenderer};

fn test_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("cutty-stills-tests");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn ffmpeg(args: &[&str], out: &PathBuf) {
    if out.is_file() {
        return;
    }
    let status = Command::new("ffmpeg")
        .args(["-y", "-v", "error"])
        .args(args)
        .arg(out)
        .status()
        .expect("system ffmpeg required");
    assert!(status.success(), "ffmpeg failed for {}", out.display());
}

/// Opaque PNG with the testsrc2 pattern.
fn make_png() -> PathBuf {
    let file = test_dir().join("still.png");
    ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x360:rate=1:duration=1",
            "-frames:v",
            "1",
        ],
        &file,
    );
    file
}

/// 2s animated GIF (10 fps, moving testsrc pattern).
fn make_gif() -> PathBuf {
    let file = test_dir().join("loop.gif");
    ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=160x120:rate=10:duration=2",
            "-loop",
            "0",
        ],
        &file,
    );
    file
}

/// 4s 480×480 VP9 WebM with real alpha: an opaque circle (testsrc2
/// pattern) over fully transparent corners.
fn make_alpha_webm() -> PathBuf {
    let file = test_dir().join("overlay-alpha.webm");
    ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=480x480:rate=30:duration=4",
            "-vf",
            "format=rgba,geq=r='r(X,Y)':g='g(X,Y)':b='b(X,Y)':a='if(lt(hypot(X-W/2,Y-H/2),W/3),255,0)'",
            "-c:v",
            "libvpx-vp9",
            "-pix_fmt",
            "yuva420p",
            "-b:v",
            "500k",
        ],
        &file,
    );
    file
}

/// Opaque H.264 source.
fn make_video(name: &str, size: &str) -> PathBuf {
    let file = test_dir().join(name);
    ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc2=size={size}:rate=30:duration=4"),
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-g",
            "30",
            "-pix_fmt",
            "yuv420p",
        ],
        &file,
    );
    file
}

fn gpu_available() -> bool {
    match TimelineRenderer::new(8, 8, false) {
        Ok(_) => true,
        Err(e) => {
            eprintln!("stills tests: skipping, no GPU ({e})");
            false
        }
    }
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

fn hash_frame(frame: &FrameSlice) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    let row_bytes = frame.width as usize * 4;
    for row in 0..frame.height as usize {
        hasher.update(&frame.data[row * frame.stride..row * frame.stride + row_bytes]);
    }
    hasher.finalize()
}

fn pixel(frame: &FrameSlice, x: usize, y: usize) -> [u8; 4] {
    let o = y * frame.stride + x * 4;
    [
        frame.data[o],
        frame.data[o + 1],
        frame.data[o + 2],
        frame.data[o + 3],
    ]
}

fn channel_close(a: [u8; 4], b: [u8; 4], tol: i32) -> bool {
    a.iter()
        .zip(b.iter())
        .take(3)
        .all(|(x, y)| (i32::from(*x) - i32::from(*y)).abs() <= tol)
}

#[test]
fn still_image_renders_and_holds_forever() {
    if !gpu_available() {
        return;
    }
    let png = make_png();
    let mut engine = Engine::new(ProjectSettings {
        width: 1280,
        height: 720,
        fps: 30.0,
    });
    let media = engine
        .add_media_with_kind(png.display().to_string(), 0.0, true, false, false, MediaKind::Image)
        .unwrap();
    let video = engine.project().tracks[0].id;
    // A 20s clip of a still — way past any "media duration".
    engine.add_clip(video, media, 0.0, 0.0, 20.0).unwrap();
    let project = engine.project().clone();

    let mut renderer = TimelineRenderer::new(1280, 720, true).expect("gpu");
    let resolver = originals_resolver(&project);
    let early = renderer
        .render_with(&project, 0.1, &resolver, |f| {
            (hash_frame(&f), pixel(&f, 640, 360))
        })
        .expect("early frame");
    let late = renderer
        .render_with(&project, 19.9, &resolver, |f| {
            (hash_frame(&f), pixel(&f, 640, 360))
        })
        .expect("late frame");
    assert_eq!(early.0, late.0, "a still never changes");
    assert_ne!(early.1, [0, 0, 0, 255], "the picture actually shows");
    // Stills sample for free: exactly one cold open, no per-frame decodes.
    assert_eq!(renderer.stats().cold_opens, 1);
    assert_eq!(renderer.stats().seeks, 0);
}

#[test]
fn gif_clip_loops_to_fill_its_window() {
    if !gpu_available() {
        return;
    }
    let gif = make_gif();
    let mut engine = Engine::new(ProjectSettings {
        width: 640,
        height: 480,
        fps: 30.0,
    });
    let media = engine
        .add_media_with_kind(gif.display().to_string(), 2.0, true, false, true, MediaKind::Gif)
        .unwrap();
    let video = engine.project().tracks[0].id;
    // 6s clip of a 2s GIF: three full loops.
    engine.add_clip(video, media, 0.0, 0.0, 6.0).unwrap();
    let project = engine.project().clone();

    let mut renderer = TimelineRenderer::new(640, 480, true).expect("gpu");
    let resolver = originals_resolver(&project);
    let mut at = |t: f64| {
        renderer
            .render_with(&project, t, &resolver, |f| hash_frame(&f))
            .expect("frame renders")
    };
    let first_pass = at(0.5);
    let mid_pass = at(1.5);
    assert_ne!(first_pass, mid_pass, "the GIF animates within a period");
    let second_loop = at(2.5);
    let third_loop = at(4.5);
    assert_eq!(first_pass, second_loop, "one period later: same frame");
    assert_eq!(first_pass, third_loop, "two periods later: same frame");
}

#[test]
fn webm_alpha_overlay_composites_transparently() {
    if !gpu_available() {
        return;
    }
    let base_src = make_video("base.mp4", "640x360");
    let webm = make_alpha_webm();

    let mut engine = Engine::new(ProjectSettings {
        width: 1280,
        height: 720,
        fps: 30.0,
    });
    let base = engine
        .add_media(base_src.display().to_string(), 4.0, true, false)
        .unwrap();
    let overlay = engine
        .add_media_with_kind(webm.display().to_string(), 4.0, true, false, true, MediaKind::Video)
        .unwrap();
    let video = engine.project().tracks[0].id;
    engine.add_clip(video, base, 0.0, 0.0, 4.0).unwrap();
    let mut project = engine.project().clone();

    // Overlay lane above the base with the webm at default placement
    // (contain-fit: 720×720 centered, x ∈ [280, 1000]).
    project.tracks.insert(
        0,
        Track {
            id: TrackId(100),
            kind: TrackKind::Video,
            name: "V2".into(),
            locked: false,
            muted: false,
            hidden: false,
            clips: vec![Clip {
                id: ClipId(200),
                media_id: Some(overlay),
                timeline_in: 0.0,
                timeline_out: 4.0,
                source_in: 0.0,
                source_out: 4.0,
                transform: Transform::default(),
                opacity: 1.0,
                blend_mode: cutty_engine::BlendMode::Normal,
                speed: 1.0,
                volume: 1.0,
                transition_out: None,
                text: None,
                keyframes: Default::default(),
            }],
        },
    );
    project.validate().expect("fixture valid");

    let base_only = {
        let mut p = project.clone();
        p.tracks[0].hidden = true;
        p
    };

    let t = 1.0;
    let mut renderer = TimelineRenderer::new(1280, 720, true).expect("gpu");
    // Overlay quad corner (inside the quad, outside the alpha circle) and
    // circle center.
    let corner = (300usize, 20usize);
    let center = (640usize, 360usize);
    let resolver = originals_resolver(&project);
    let (with_overlay_corner, with_overlay_center) = renderer
        .render_with(&project, t, &resolver, |f| {
            (pixel(&f, corner.0, corner.1), pixel(&f, center.0, center.1))
        })
        .expect("overlay frame");
    let resolver = originals_resolver(&base_only);
    let (base_corner, base_center) = renderer
        .render_with(&base_only, t, &resolver, |f| {
            (pixel(&f, corner.0, corner.1), pixel(&f, center.0, center.1))
        })
        .expect("base frame");

    assert!(
        channel_close(with_overlay_corner, base_corner, 2),
        "transparent overlay region must show the base: {with_overlay_corner:?} vs {base_corner:?}"
    );
    assert!(
        !channel_close(with_overlay_center, base_center, 8),
        "opaque overlay region must cover the base: {with_overlay_center:?} vs {base_center:?}"
    );
}

/// The transition direct fast path assumes opaque full-frame sources; a
/// translucent side must keep its premultiply pass. Rig the exact
/// direct-path geometry (source dims == target dims, identity placement)
/// with an alpha source and check the transparent region composites to
/// black, not to the source's straight RGB.
#[test]
fn translucent_transition_side_skips_the_direct_path() {
    if !gpu_available() {
        return;
    }
    let opaque = make_video("square.mp4", "480x480");
    let webm = make_alpha_webm();

    let mut engine = Engine::new(ProjectSettings {
        width: 480,
        height: 480,
        fps: 30.0,
    });
    let a = engine
        .add_media(opaque.display().to_string(), 4.0, true, false)
        .unwrap();
    let b = engine
        .add_media_with_kind(webm.display().to_string(), 4.0, true, false, true, MediaKind::Video)
        .unwrap();
    let video = engine.project().tracks[0].id;
    let clip_a = engine.add_clip(video, a, 0.0, 0.0, 2.0).unwrap();
    engine.add_clip(video, b, 2.0, 0.0, 2.0).unwrap();
    engine
        .set_transition(
            clip_a,
            Some(Transition {
                kind: "fade".into(),
                duration: 1.0,
            }),
        )
        .expect("transition binds");
    let project = engine.project().clone();

    let mut renderer = TimelineRenderer::new(480, 480, true).expect("gpu");
    let resolver = originals_resolver(&project);
    // End of the span: progress ≈ 1, output ≈ the webm side alone. Its
    // corner is fully transparent → over the black base it must be
    // (near) black. A direct-path misread would show the corner's
    // straight RGB (testsrc2 pattern, far from black).
    let corner = renderer
        .render_with(&project, 2.49, &resolver, |f| pixel(&f, 10, 10))
        .expect("transition frame");
    assert!(
        corner[0] < 12 && corner[1] < 12 && corner[2] < 12,
        "transparent corner must composite to black, got {corner:?}"
    );
}

/// Preview and export frontends must stay welded for the new source
/// kinds too: a project mixing a still, a looping GIF overlay and a
/// WebM-alpha overlay renders bit-identically through both.
#[test]
fn stills_project_preview_equals_export() {
    if !gpu_available() {
        return;
    }
    let png = make_png();
    let gif = make_gif();
    let webm = make_alpha_webm();

    let mut engine = Engine::new(ProjectSettings {
        width: 1280,
        height: 720,
        fps: 30.0,
    });
    let still = engine
        .add_media_with_kind(png.display().to_string(), 0.0, true, false, false, MediaKind::Image)
        .unwrap();
    let sticker = engine
        .add_media_with_kind(gif.display().to_string(), 2.0, true, false, true, MediaKind::Gif)
        .unwrap();
    let overlay = engine
        .add_media_with_kind(webm.display().to_string(), 4.0, true, false, true, MediaKind::Video)
        .unwrap();
    let video = engine.project().tracks[0].id;
    engine.add_clip(video, still, 0.0, 0.0, 3.0).unwrap();
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
            clips: vec![
                Clip {
                    id: ClipId(200),
                    media_id: Some(sticker),
                    timeline_in: 0.0,
                    timeline_out: 1.4,
                    source_in: 0.0,
                    source_out: 1.4,
                    transform: Transform {
                        x: -300.0,
                        y: -160.0,
                        scale: 0.5,
                        rotation: -10.0,
                    },
                    opacity: 0.9,
                    blend_mode: cutty_engine::BlendMode::Normal,
                    speed: 1.0,
                    volume: 1.0,
                    transition_out: None,
                    text: None,
                    keyframes: Default::default(),
                },
                Clip {
                    id: ClipId(201),
                    media_id: Some(overlay),
                    timeline_in: 1.4,
                    timeline_out: 3.0,
                    source_in: 0.2,
                    source_out: 1.8,
                    transform: Transform {
                        x: 240.0,
                        y: 100.0,
                        scale: 0.6,
                        rotation: 15.0,
                    },
                    opacity: 1.0,
                    blend_mode: cutty_engine::BlendMode::Normal,
                    speed: 1.0,
                    volume: 1.0,
                    transition_out: None,
                    text: None,
                    keyframes: Default::default(),
                },
            ],
        },
    );
    project.validate().expect("fixture valid");
    let total_frames = (3.0 * 30.0) as i64;

    let mut preview = TimelineRenderer::new(1280, 720, false).expect("gpu");
    let resolver = originals_resolver(&project);
    let mut preview_hashes = Vec::new();
    for idx in 0..total_frames {
        let t = idx as f64 / 30.0;
        preview_hashes.push(
            preview
                .render_with(&project, t, &resolver, |f| hash_frame(&f))
                .expect("preview frame"),
        );
    }
    drop(preview);

    let mut export_hashes = Vec::new();
    for_each_composited_frame(
        &project,
        1280,
        720,
        30.0,
        total_frames,
        &|| false,
        &mut |_, data, stride| {
            let frame = FrameSlice {
                width: 1280,
                height: 720,
                stride,
                data,
            };
            export_hashes.push(hash_frame(&frame));
            Ok(())
        },
    )
    .expect("export frames render");

    assert_eq!(preview_hashes.len(), export_hashes.len());
    let mismatches: Vec<usize> = preview_hashes
        .iter()
        .zip(&export_hashes)
        .enumerate()
        .filter(|(_, (p, e))| p != e)
        .map(|(i, _)| i)
        .collect();
    assert!(
        mismatches.is_empty(),
        "{} of {} frames differ between frontends: {mismatches:?}",
        mismatches.len(),
        total_frames
    );
}
