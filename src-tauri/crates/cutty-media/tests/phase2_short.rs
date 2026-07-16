//! **The Phase 2 acceptance gate** (PLAN.md §5): replicate a real
//! CapCut-style short end to end without leaving Cutty.
//!
//! The project is a 24 s vertical 1080×1920 short built exclusively
//! through engine commands (exactly what the UI sends):
//! - track 1: six b-roll cuts,
//! - track 2: a WebM-alpha picture-in-picture and a looping GIF sticker,
//! - two styled text overlays (big title + lower third),
//! - transitions on three cuts (fade / slideleft / circleopen),
//! - music with a fade-in and fade-out under extracted clip audio (the
//!   b-roll lane itself is muted).
//!
//! Asserted here: it previews smoothly (real `TimelinePlayer`, ≥30 fps
//! cadence, bounded A/V drift, through the transition + PiP + text
//! stretch), it exports with the Shorts preset (1080×1920@30, verified
//! with ffprobe), the export composite matches the preview composite
//! bit-exactly at scene-probe times, and the exported audio carries the
//! drawn fade envelope.
//!
//! Slow (a full 720-frame vertical export): `#[ignore]`-gated like the
//! other acceptance suites —
//! `cargo test -p cutty-media --test phase2_short -- --ignored --nocapture`

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use cutty_engine::{
    ClipId, Engine, FadeSide, MediaKind, Project, ProjectSettings, TrackFlag, TrackKind,
    Transform, Transition,
};
use cutty_media::{
    for_each_composited_frame, generate_proxy, run_export, CancelToken, ExportQuality,
    ExportSpec, FrameSlice, PlayerEvent, TimelinePlayer, TimelineRenderer,
};

const FPS: f64 = 30.0;
const SHORT_SECS: f64 = 24.0;

fn media_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("cutty-phase2-short");
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

/// Vertical 720×1280 b-roll with audio (distinct pattern + tone per id).
fn broll(name: &str, pattern: &str, tone: u32) -> PathBuf {
    let file = media_dir().join(format!("broll-{name}.mp4"));
    ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            &format!("{pattern}=size=720x1280:rate=30:duration=10"),
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency={tone}:sample_rate=48000:duration=10"),
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-g",
            "30",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-ac",
            "2",
            "-shortest",
        ],
        &file,
    );
    file
}

/// WebM VP9 alpha sticker: opaque moving pattern inside a circle,
/// transparent corners.
fn alpha_webm() -> PathBuf {
    let file = media_dir().join("pip-alpha.webm");
    ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=480x480:rate=30:duration=8",
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

/// 2 s animated GIF sticker.
fn gif_sticker() -> PathBuf {
    let file = media_dir().join("sticker.gif");
    ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=200x150:rate=10:duration=2",
            "-loop",
            "0",
        ],
        &file,
    );
    file
}

/// 30 s music bed (amplitude-modulated tone, WAV).
fn music() -> PathBuf {
    let file = media_dir().join("music.wav");
    ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "aevalsrc='0.4*sin(2*PI*220*t)*(0.6+0.4*sin(2*PI*0.5*t))':s=48000:d=30",
            "-c:a",
            "pcm_f32le",
        ],
        &file,
    );
    file
}

struct Short {
    project: Project,
}

/// Build the short through engine commands only.
fn build_short() -> Short {
    let a_src = broll("a", "testsrc2", 300);
    let b_src = broll("b", "smptebars", 440);
    let c_src = broll("c", "testsrc", 550);
    let pip_src = alpha_webm();
    let gif_src = gif_sticker();
    let music_src = music();

    let mut engine = Engine::new(ProjectSettings {
        width: 1080,
        height: 1920,
        fps: FPS,
    });
    let a = engine
        .add_media(a_src.display().to_string(), 10.0, true, true)
        .unwrap();
    let b = engine
        .add_media(b_src.display().to_string(), 10.0, true, true)
        .unwrap();
    let c = engine
        .add_media(c_src.display().to_string(), 10.0, true, true)
        .unwrap();
    let pip = engine
        .add_media_with_kind(pip_src.display().to_string(), 8.0, true, false, true, MediaKind::Video)
        .unwrap();
    let gif = engine
        .add_media_with_kind(gif_src.display().to_string(), 2.0, true, false, true, MediaKind::Gif)
        .unwrap();
    let music_m = engine
        .add_media(music_src.display().to_string(), 30.0, false, true)
        .unwrap();

    let v1 = engine
        .project()
        .tracks
        .iter()
        .find(|t| t.kind == TrackKind::Video)
        .unwrap()
        .id;
    let a1 = engine
        .project()
        .tracks
        .iter()
        .find(|t| t.kind == TrackKind::Audio)
        .unwrap()
        .id;

    // Track 1: six b-roll cuts on the 4 s grid.
    let sources = [a, b, c, a, c, b];
    let mut cut_clips: Vec<ClipId> = Vec::new();
    for (i, m) in sources.iter().enumerate() {
        let t = i as f64 * 4.0;
        let s = 1.0 + (i as f64) * 0.7; // varied in-points, handles both sides
        let clip = engine.add_clip(v1, *m, t, s, s + 4.0).unwrap();
        cut_clips.push(clip);
    }
    // Transitions on three cuts.
    for (clip, kind, duration) in [
        (cut_clips[0], "fade", 0.8),
        (cut_clips[2], "slideleft", 0.6),
        (cut_clips[3], "circleopen", 0.8),
    ] {
        engine
            .set_transition(
                clip,
                Some(Transition {
                    kind: kind.into(),
                    duration,
                }),
            )
            .unwrap();
    }

    // Track 2 (overlays): PiP alpha WebM top-left for the first 8 s,
    // GIF sticker looping bottom-right for 10-16 s.
    let v2 = engine.add_track(TrackKind::Video, 0).unwrap();
    let pip_clip = engine.add_clip(v2, pip, 0.5, 0.0, 7.5).unwrap();
    engine
        .set_clip_transform(
            pip_clip,
            Transform {
                x: -280.0,
                y: -600.0,
                scale: 0.45,
                rotation: 0.0,
            },
        )
        .unwrap();
    let gif_clip = engine.add_clip(v2, gif, 10.0, 0.0, 6.0).unwrap(); // 3 loops
    engine
        .set_clip_transform(
            gif_clip,
            Transform {
                x: 300.0,
                y: 640.0,
                scale: 0.8,
                rotation: -8.0,
            },
        )
        .unwrap();

    // Two styled text overlays.
    let title = cutty_engine::TextSpec {
        content: "GOLDEN HOUR".into(),
        style: cutty_engine::TextStyle {
            font_size: 110.0,
            stroke_width: 8.0,
            ..Default::default()
        },
    };
    engine
        .add_text_clip(
            0.8,
            3.4,
            title,
            Transform {
                x: 0.0,
                y: -420.0,
                scale: 1.0,
                rotation: 0.0,
            },
            None,
        )
        .unwrap();
    let lower_third = cutty_engine::TextSpec {
        content: "shot on nothing\nedited in Cutty".into(),
        style: cutty_engine::TextStyle {
            font_size: 54.0,
            weight: 400,
            fill: "#fef3c7".into(),
            stroke_width: 0.0,
            shadow_alpha: 0.6,
            align: cutty_engine::TextAlign::Left,
            ..Default::default()
        },
    };
    engine
        .add_text_clip(
            16.0,
            5.5,
            lower_third,
            Transform {
                x: -180.0,
                y: 700.0,
                scale: 1.0,
                rotation: 0.0,
            },
            None,
        )
        .unwrap();

    // Music bed on A1 with drawn fades.
    let music_clip = engine.add_clip(a1, music_m, 0.0, 2.0, 2.0 + SHORT_SECS).unwrap();
    engine.set_clip_volume(music_clip, 0.8).unwrap();
    assert_eq!(
        engine.set_clip_fade(music_clip, FadeSide::In, 1.5).unwrap(),
        1.5
    );
    assert_eq!(
        engine.set_clip_fade(music_clip, FadeSide::Out, 2.0).unwrap(),
        2.0
    );

    // Extract clip 2's audio (lands on a fresh audio lane — A1 is full of
    // music), then mute the b-roll lane: its sound now comes only through
    // the extracted clip, sitting on top of the music bed.
    engine.extract_audio(cut_clips[1]).unwrap();
    engine.set_track_flag(v1, TrackFlag::Muted, true).unwrap();

    let project = engine.project().clone();
    project.validate().expect("short is valid");
    Short { project }
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

fn gpu_available() -> bool {
    match TimelineRenderer::new(8, 8, false) {
        Ok(_) => true,
        Err(e) => {
            eprintln!("phase2 short: skipping, no GPU ({e})");
            false
        }
    }
}

fn ffprobe_json(path: &Path) -> serde_json::Value {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            "-count_frames",
        ])
        .arg(path)
        .output()
        .expect("ffprobe");
    assert!(out.status.success());
    serde_json::from_slice(&out.stdout).expect("ffprobe json")
}

/// Preview smoothness: play the busiest stretch (transition at 12 s +
/// GIF sticker + approach to the lower third) through the real player.
#[test]
#[ignore = "acceptance: slow full pipeline; run with --ignored"]
fn short_previews_smoothly() {
    let short = build_short();
    // Preview decodes b-roll through proxies — generate like import does.
    for m in &short.project.media {
        if m.has_video && !m.unbounded_source() && !m.has_alpha {
            generate_proxy(Path::new(&m.path), Some(m.duration), |_| {}).expect("proxy");
        }
    }

    let (tx, rx) = mpsc::channel();
    let player = TimelinePlayer::open(
        short.project.clone(),
        Box::new(move |e| {
            let _ = tx.send(e);
        }),
    )
    .expect("player opens");

    player.seek(9.5);
    // Drain until the seek's frame lands.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(PlayerEvent::Frame { .. }) => break,
            Ok(_) => {}
            Err(_) => assert!(Instant::now() < deadline, "no frame after seek"),
        }
    }

    const PLAY_SECS: f64 = 6.0;
    player.play();
    struct Rec {
        pts: f64,
        clock: f64,
        arrived: Instant,
    }
    let mut frames: Vec<Rec> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(PLAY_SECS as u64 + 6);
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(PlayerEvent::Frame {
                pts_sec, clock_sec, ..
            }) => {
                frames.push(Rec {
                    pts: pts_sec,
                    clock: clock_sec,
                    arrived: Instant::now(),
                });
                if frames.last().is_some_and(|f| f.pts >= 9.5 + PLAY_SECS) {
                    break;
                }
            }
            Ok(PlayerEvent::Eof) => break,
            Ok(PlayerEvent::Error(e)) if e.contains("audio unavailable") => {}
            Ok(PlayerEvent::Error(e)) => panic!("player error: {e}"),
            Ok(_) => {}
            Err(_) => panic!("no events for 2 s mid-playback"),
        }
    }
    player.pause();

    let expected = (PLAY_SECS * FPS) as usize;
    assert!(
        frames.len() >= expected - 8,
        "only {} of ~{} frames presented",
        frames.len(),
        expected
    );
    for pair in frames.windows(2).skip(5) {
        let dt = pair[1].arrived.duration_since(pair[0].arrived);
        assert!(
            dt < Duration::from_secs_f64(2.5 / FPS),
            "hitch {dt:?} at pts {:.2}",
            pair[0].pts
        );
    }
    let max_drift = frames
        .iter()
        .skip(5)
        .map(|f| (f.pts - f.clock).abs())
        .fold(0.0f64, f64::max);
    println!(
        "short preview: {} frames over {PLAY_SECS}s, max |pts-clock| {:.1} ms",
        frames.len(),
        max_drift * 1e3
    );
    assert!(max_drift < 0.040, "A/V drift {:.1} ms", max_drift * 1e3);
}

/// Export with the Shorts preset; verify the file, the preview==export
/// composite at scene probes, and the audio fade envelope.
#[test]
#[ignore = "acceptance: slow full pipeline; run with --ignored"]
fn short_exports_with_the_shorts_preset_and_matches_preview() {
    if !gpu_available() {
        return;
    }
    let short = build_short();

    // --- Export (Shorts preset: 1080×1920@30) ---
    let dst = media_dir().join("short-out.mp4");
    let _ = std::fs::remove_file(&dst);
    let spec = ExportSpec {
        width: 1080,
        height: 1920,
        fps: FPS,
        quality: ExportQuality::Medium,
        dst: dst.clone(),
    };
    let summary = run_export(&short.project, &spec, &CancelToken::new(), &mut |_| {})
        .expect("export runs");
    assert!((summary.duration_sec - SHORT_SECS).abs() < 0.05);

    let probe = ffprobe_json(&dst);
    let streams = probe["streams"].as_array().unwrap();
    let video = streams.iter().find(|s| s["codec_type"] == "video").unwrap();
    assert_eq!(video["codec_name"], "h264");
    assert_eq!(video["width"], 1080);
    assert_eq!(video["height"], 1920);
    assert_eq!(video["r_frame_rate"], "30/1");
    let frames: i64 = video["nb_read_frames"].as_str().unwrap().parse().unwrap();
    assert_eq!(frames, (SHORT_SECS * FPS) as i64, "exact frame count");
    let audio = streams.iter().find(|s| s["codec_type"] == "audio").unwrap();
    assert_eq!(audio["codec_name"], "aac");
    assert_eq!(audio["sample_rate"], "48000");

    // --- Preview == export at scene probes (raw composite, bit-exact) ---
    // One probe per distinctive scene: title text, transition span
    // midpoint, PiP alpha, GIF loop (2nd repeat), lower third + fade-out.
    let probe_times = [2.0, 4.0, 6.2, 12.9, 17.5, 23.0];
    let probe_frames: Vec<i64> = probe_times.iter().map(|t| (t * FPS).round() as i64).collect();

    let mut export_hashes = std::collections::HashMap::new();
    for_each_composited_frame(
        &short.project,
        1080,
        1920,
        FPS,
        (SHORT_SECS * FPS) as i64,
        &|| false,
        &mut |idx, data, stride| {
            if probe_frames.contains(&idx) {
                export_hashes.insert(
                    idx,
                    hash_frame(&FrameSlice {
                        width: 1080,
                        height: 1920,
                        stride,
                        data,
                    }),
                );
            }
            Ok(())
        },
    )
    .expect("export frames");

    let mut preview = TimelineRenderer::new(1080, 1920, false).expect("gpu");
    let resolver = originals_resolver(&short.project);
    for (&t, &idx) in probe_times.iter().zip(&probe_frames) {
        let hash = preview
            .render_with(&short.project, idx as f64 / FPS, &resolver, |f| hash_frame(&f))
            .expect("preview frame");
        assert_eq!(
            Some(&hash),
            export_hashes.get(&idx),
            "preview != export at t={t}s (frame {idx})"
        );
    }

    // --- Audio fade envelope sanity on the exported file ---
    let wav = media_dir().join("short-audio.wav");
    let status = Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-i"])
        .arg(&dst)
        .args(["-vn", "-c:a", "pcm_f32le"])
        .arg(&wav)
        .status()
        .unwrap();
    assert!(status.success());
    let bytes = std::fs::read(&wav).unwrap();
    let data_off = bytes.windows(4).position(|w| w == b"data").unwrap() + 8;
    let samples: Vec<f32> = bytes[data_off..]
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let rms = |from: f64, to: f64| -> f64 {
        let s = (from * 48_000.0) as usize * 2;
        let e = ((to * 48_000.0) as usize * 2).min(samples.len());
        let win = &samples[s..e];
        (win.iter().map(|&x| f64::from(x) * f64::from(x)).sum::<f64>() / win.len() as f64).sqrt()
    };
    let start = rms(0.05, 0.4); // inside the 1.5s fade-in
    let mid = rms(3.0, 5.0);
    let tail = rms(23.6, 23.95); // inside the 2s fade-out
    println!("audio envelope: start {start:.4}, mid {mid:.4}, tail {tail:.4}");
    assert!(start < mid * 0.6, "fade-in must attenuate the start");
    assert!(tail < mid * 0.5, "fade-out must attenuate the tail");
    assert!(mid > 0.05, "music must be audible mid-short");

    println!(
        "shorts export: {:.2}s @ 1080x1920 via {} (hw: {}) — preview==export at {} probes",
        summary.duration_sec,
        summary.encoder,
        summary.hardware_encode,
        probe_times.len()
    );
}
