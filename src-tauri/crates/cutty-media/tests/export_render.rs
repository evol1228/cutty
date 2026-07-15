//! End-to-end export pipeline tests against real ffmpeg: mixed source
//! formats through cuts and gaps into one MP4, with duration / fps /
//! resolution / frame-count / audio-structure verification, plus the
//! cancel contract (no zombie processes, no temp litter).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use cutty_engine::{Engine, Project, ProjectSettings, TrackKind};
use cutty_media::{
    generate_proxy, probe, run_export, CancelToken, ExportQuality, ExportSpec, ExportStage,
    SourceDecoder,
};
use ffmpeg_sidecar::paths::ffmpeg_path;

/// The e2e tests spawn ffmpeg children and inspect /proc for leaks; run
/// them one at a time so they don't see each other's processes.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn test_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("cutty-media-tests");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A/V test clip in an arbitrary container/rate (mixed-format inputs).
fn make_clip(name: &str, size: &str, fps: u32, secs: u32, tone_hz: u32) -> PathBuf {
    let file = test_dir().join(name);
    if file.is_file() {
        return file;
    }
    let status = Command::new(ffmpeg_path())
        .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
        .arg(format!("testsrc2=size={size}:rate={fps}:duration={secs}"))
        .args(["-f", "lavfi", "-i"])
        .arg(format!(
            "sine=frequency={tone_hz}:sample_rate=48000:duration={secs}"
        ))
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-g", "30"])
        .args(["-c:a", "aac", "-b:a", "128k", "-shortest"])
        .arg(&file)
        .status()
        .expect("system ffmpeg required");
    assert!(status.success());
    file
}

/// Audio-only music bed (WAV so symphonia decodes the original directly).
fn make_music(name: &str, secs: u32) -> PathBuf {
    let file = test_dir().join(name);
    if file.is_file() {
        return file;
    }
    let status = Command::new(ffmpeg_path())
        .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
        .arg(format!(
            "sine=frequency=220:sample_rate=44100:duration={secs}"
        ))
        .args(["-c:a", "pcm_s16le"])
        .arg(&file)
        .status()
        .expect("system ffmpeg required");
    assert!(status.success());
    file
}

struct TrackIds {
    video: cutty_engine::TrackId,
    audio: cutty_engine::TrackId,
}

fn track_ids(engine: &Engine) -> TrackIds {
    let find = |kind| {
        engine
            .project()
            .tracks
            .iter()
            .find(|t| t.kind == kind)
            .unwrap()
            .id
    };
    TrackIds {
        video: find(TrackKind::Video),
        audio: find(TrackKind::Audio),
    }
}

/// Register a media file (probing it) and generate its proxy if it has
/// video (export audio resolves video-media audio from the proxy).
fn add_media(engine: &mut Engine, path: &Path) -> cutty_engine::MediaId {
    let info = probe(path).expect("probe");
    if info.video.is_some() {
        generate_proxy(path, Some(info.duration_sec), |_| {}).expect("proxy");
    }
    engine
        .add_media(
            path.display().to_string(),
            info.duration_sec,
            info.video.is_some(),
            info.audio.is_some(),
        )
        .expect("add media")
}

/// Decode the exported file's audio into interleaved stereo f32 at its
/// native rate using the same decoder stack the app uses.
fn decoded_audio(path: &Path) -> (u32, Vec<f32>) {
    use cutty_audio::{AudioSource, SymphoniaSource};
    let mut src = SymphoniaSource::open(path).expect("open exported audio");
    let rate = src.sample_rate();
    assert_eq!(src.channels(), 2, "exported audio must be stereo");
    let mut all = Vec::new();
    let mut buf = vec![0f32; 8192];
    loop {
        let n = src.read(&mut buf).expect("read exported audio");
        if n == 0 {
            break;
        }
        all.extend_from_slice(&buf[..n]);
    }
    (rate, all)
}

fn rms(samples: &[f32], rate: u32, from_sec: f64, to_sec: f64) -> f64 {
    let s = (from_sec * f64::from(rate)) as usize * 2;
    let e = ((to_sec * f64::from(rate)) as usize * 2).min(samples.len());
    assert!(e > s, "window [{from_sec}, {to_sec}] out of range");
    let win = &samples[s..e];
    (win.iter()
        .map(|&x| f64::from(x) * f64::from(x))
        .sum::<f64>()
        / win.len() as f64)
        .sqrt()
}

/// Mean RGB value (0–255) of the exported frame at `t`, decoded with the
/// app's own decoder (RGBA frames — alpha is skipped).
fn mean_luma_at(path: &Path, t: f64) -> f64 {
    let mut dec = SourceDecoder::open(path).expect("open exported video");
    let frame = dec.seek_to(t).expect("seek").expect("frame");
    let mut sum = 0u64;
    let mut count = 0u64;
    for row in 0..frame.height as usize {
        let line = &frame.data[row * frame.stride..row * frame.stride + frame.width as usize * 4];
        for px in line.chunks_exact(4) {
            sum += u64::from(px[0]) + u64::from(px[1]) + u64::from(px[2]);
            count += 3;
        }
    }
    sum as f64 / count as f64
}

/// Mean RGB inside a centered box covering `frac` of each dimension.
fn mean_luma_in_center(path: &Path, t: f64, frac: f64) -> f64 {
    let mut dec = SourceDecoder::open(path).expect("open exported video");
    let frame = dec.seek_to(t).expect("seek").expect("frame");
    let (w, h) = (frame.width as usize, frame.height as usize);
    let (x0, x1) = (
        (w as f64 * (0.5 - frac / 2.0)) as usize,
        (w as f64 * (0.5 + frac / 2.0)) as usize,
    );
    let (y0, y1) = (
        (h as f64 * (0.5 - frac / 2.0)) as usize,
        (h as f64 * (0.5 + frac / 2.0)) as usize,
    );
    let mut sum = 0u64;
    let mut count = 0u64;
    for row in y0..y1 {
        let line = &frame.data[row * frame.stride..row * frame.stride + w * 4];
        for px in line[x0 * 4..x1 * 4].chunks_exact(4) {
            sum += u64::from(px[0]) + u64::from(px[1]) + u64::from(px[2]);
            count += 3;
        }
    }
    sum as f64 / count as f64
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

/// ffmpeg children (live or zombie) of this test process.
fn our_ffmpeg_children() -> Vec<u32> {
    let me = std::process::id();
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // "pid (comm) state ppid ..." — comm may contain spaces, parse
        // from the closing paren.
        let Some(close) = stat.rfind(')') else {
            continue;
        };
        let comm = &stat[stat.find('(').map(|i| i + 1).unwrap_or(0)..close];
        let rest: Vec<&str> = stat[close + 1..].split_whitespace().collect();
        let ppid: u32 = rest.get(1).and_then(|p| p.parse().ok()).unwrap_or(0);
        if ppid == me && comm.contains("ffmpeg") {
            found.push(pid);
        }
    }
    found
}

fn export_job_dirs() -> Vec<PathBuf> {
    let Some(cache) = dirs::cache_dir() else {
        return Vec::new();
    };
    let dir = cache.join("cutty").join("export");
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect()
}

/// The full pipeline: mixed formats (mp4 + mkv, 30fps + 24fps, different
/// resolutions), a cut, a gap, and a music bed → one Shorts-shaped MP4
/// with exact duration/fps/resolution/frame count and the audio structure
/// in the right places.
#[test]
fn export_mixed_sources_end_to_end() {
    let _guard = serial();

    // Timeline (3.0 s @ 30fps → 90 frames):
    //   video: A[0.0,1.5) from 0.5s · gap [1.5,2.0) · B[2.0,3.0) from 0s
    //   audio: music [2.0,3.0) at volume 0.8
    // A is 320×180@30 mp4; B is 640×360@24 mkv (mixed formats).
    let src_a = make_clip("export-src-a.mp4", "320x180", 30, 3, 440);
    let src_b_mp4 = make_clip("export-src-b-tmp.mp4", "640x360", 24, 2, 660);
    let src_b = {
        // Remux B into Matroska so the timeline really mixes containers.
        let mkv = test_dir().join("export-src-b.mkv");
        if !mkv.is_file() {
            let status = Command::new(ffmpeg_path())
                .args(["-y", "-v", "error", "-i"])
                .arg(&src_b_mp4)
                .args(["-c", "copy"])
                .arg(&mkv)
                .status()
                .unwrap();
            assert!(status.success());
        }
        mkv
    };
    let music = make_music("export-music.wav", 5);

    let mut engine = Engine::new(ProjectSettings::default());
    let a = add_media(&mut engine, &src_a);
    let b = add_media(&mut engine, &src_b);
    let m = add_media(&mut engine, &music);
    let tracks = track_ids(&engine);
    engine.add_clip(tracks.video, a, 0.0, 0.5, 2.0).unwrap();
    engine.add_clip(tracks.video, b, 2.0, 0.0, 1.0).unwrap();
    let music_clip = engine.add_clip(tracks.audio, m, 2.0, 0.0, 1.0).unwrap();
    engine.set_clip_volume(music_clip, 0.8).unwrap();
    let project: Project = engine.project().clone();

    let dst = test_dir().join("export-e2e-out.mp4");
    let _ = std::fs::remove_file(&dst);
    let spec = ExportSpec {
        width: 640,
        height: 360,
        fps: 30.0,
        quality: ExportQuality::Medium,
        dst: dst.clone(),
    };

    let cancel = CancelToken::new();
    let mut stages = Vec::new();
    let summary = run_export(&project, &spec, &cancel, &mut |p| {
        stages.push(p.stage);
        assert!((0.0..=100.0).contains(&p.percent));
    })
    .expect("export succeeds");

    println!(
        "exported {} via {} (hardware: {}, renderer: {})",
        summary.path.display(),
        summary.encoder,
        summary.hardware_encode,
        summary.renderer
    );
    assert_eq!(summary.path, dst);
    assert!((summary.duration_sec - 3.0).abs() < 1e-9);
    assert_eq!(
        summary.renderer, "segment-concat",
        "a plain single-track timeline must take the fast path"
    );
    assert!(stages.contains(&ExportStage::Audio));
    assert!(stages.contains(&ExportStage::Video));
    assert!(stages.contains(&ExportStage::Finalize));

    // --- Container-level checks ---
    let info = ffprobe_json(&dst);
    let duration: f64 = info["format"]["duration"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        (duration - 3.0).abs() < 0.05,
        "duration {duration} ≠ 3.0 ± 0.05"
    );

    let streams = info["streams"].as_array().unwrap();
    let video = streams
        .iter()
        .find(|s| s["codec_type"] == "video")
        .expect("video stream");
    assert_eq!(video["codec_name"], "h264");
    assert_eq!(video["width"], 640);
    assert_eq!(video["height"], 360);
    assert_eq!(video["r_frame_rate"], "30/1");
    let frames: u64 = video["nb_read_frames"].as_str().unwrap().parse().unwrap();
    assert_eq!(frames, 90, "exactly 3.0s × 30fps frames");

    let audio = streams
        .iter()
        .find(|s| s["codec_type"] == "audio")
        .expect("audio stream");
    assert_eq!(audio["codec_name"], "aac");
    assert_eq!(audio["sample_rate"], "48000");
    assert_eq!(audio["channels"], 2);

    // --- Picture structure: content where clips are, black in the gap ---
    let luma_a = mean_luma_at(&dst, 0.75);
    let luma_gap = mean_luma_at(&dst, 1.75);
    let luma_b = mean_luma_at(&dst, 2.5);
    println!("luma A {luma_a:.1} | gap {luma_gap:.1} | B {luma_b:.1}");
    assert!(luma_a > 30.0, "clip A frames must have content");
    assert!(luma_gap < 10.0, "gap must be black");
    assert!(luma_b > 30.0, "clip B frames must have content");

    // --- Audio structure == the preview mix's structure ---
    // A's tone through [0,1.5), silence in the gap [1.5,2.0), B's tone +
    // music from 2.0. A misplaced mix (even by ~100 ms) moves the silent
    // window and fails these.
    let (rate, samples) = decoded_audio(&dst);
    let a_rms = rms(&samples, rate, 0.2, 1.3);
    let gap_rms = rms(&samples, rate, 1.6, 1.9);
    let tail_rms = rms(&samples, rate, 2.1, 2.9);
    println!("rms A {a_rms:.4} | gap {gap_rms:.4} | B+music {tail_rms:.4}");
    assert!(a_rms > 0.02, "clip A audio must be audible");
    assert!(gap_rms < 0.005, "gap must be silent");
    assert!(tail_rms > 0.02, "clip B + music must be audible");
    assert!(
        tail_rms > a_rms * 1.2,
        "two summed sources must outweigh one"
    );

    // --- Hygiene ---
    assert!(our_ffmpeg_children().is_empty(), "no ffmpeg left behind");
    assert!(
        !dst.with_extension("mp4.part").exists(),
        "no .part litter next to the output"
    );
}

/// The compositor path end-to-end: a second video track with a
/// transformed, half-opacity overlay forces the GPU pipeline, which must
/// produce a correct container (duration/fps/resolution/frame count),
/// video where the overlay is visibly composited, and the same audio mix
/// structure as the fast path.
#[test]
fn export_composited_timeline_end_to_end() {
    let _guard = serial();

    // Base: dark-ish testsrc2. Overlay: white frames scaled to the
    // center at 50% opacity → the center of the composite brightens.
    let src_base = make_clip("export-comp-base.mp4", "640x360", 30, 3, 440);
    let src_overlay = {
        let file = test_dir().join("export-comp-overlay.mp4");
        if !file.is_file() {
            let status = Command::new(ffmpeg_path())
                .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
                .arg("color=c=white:size=320x180:rate=30:duration=3")
                .args(["-c:v", "libx264", "-preset", "ultrafast", "-g", "30"])
                .arg(&file)
                .status()
                .unwrap();
            assert!(status.success());
        }
        file
    };

    let mut engine = Engine::new(ProjectSettings::default());
    let base = add_media(&mut engine, &src_base);
    let overlay = add_media(&mut engine, &src_overlay);
    let tracks = track_ids(&engine);
    engine.add_clip(tracks.video, base, 0.0, 0.0, 2.0).unwrap();

    // Second video track above the base, with a transformed overlay —
    // multi-track construction is manual until the AddTrack command
    // lands (Phase 2, later prompt).
    let mut project = engine.project().clone();
    project.tracks.insert(
        0,
        cutty_engine::Track {
            id: cutty_engine::TrackId(500),
            kind: TrackKind::Video,
            name: "V2".into(),
            locked: false,
            muted: false,
            clips: vec![cutty_engine::Clip {
                id: cutty_engine::ClipId(501),
                media_id: overlay,
                timeline_in: 0.5,
                timeline_out: 1.5,
                source_in: 0.0,
                source_out: 1.0,
                transform: cutty_engine::Transform {
                    x: 0.0,
                    y: 0.0,
                    scale: 0.4,
                    rotation: 0.0,
                },
                opacity: 0.5,
                blend_mode: cutty_engine::BlendMode::Normal,
                speed: 1.0,
                volume: 1.0,
            }],
        },
    );
    project.validate().expect("fixture is valid");

    let dst = test_dir().join("export-comp-out.mp4");
    let _ = std::fs::remove_file(&dst);
    let spec = ExportSpec {
        width: 1280,
        height: 720,
        fps: 30.0,
        quality: ExportQuality::Medium,
        dst: dst.clone(),
    };
    let cancel = CancelToken::new();
    let mut stages = Vec::new();
    let summary = run_export(&project, &spec, &cancel, &mut |p| {
        stages.push(p.stage);
        assert!((0.0..=100.0).contains(&p.percent));
    })
    .expect("compositor export succeeds");

    assert_eq!(
        summary.renderer, "gpu-compositor",
        "transforms/opacity/multi-track must take the compositor path"
    );
    assert!(stages.contains(&ExportStage::Audio));
    assert!(stages.contains(&ExportStage::Video));
    assert!(stages.contains(&ExportStage::Finalize));

    // Container shape.
    let info = ffprobe_json(&dst);
    let duration: f64 = info["format"]["duration"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((duration - 2.0).abs() < 0.05, "duration {duration} ≠ 2.0");
    let streams = info["streams"].as_array().unwrap();
    let video = streams
        .iter()
        .find(|s| s["codec_type"] == "video")
        .expect("video stream");
    assert_eq!(video["codec_name"], "h264");
    assert_eq!(video["width"], 1280);
    assert_eq!(video["height"], 720);
    assert_eq!(video["r_frame_rate"], "30/1");
    let frames: u64 = video["nb_read_frames"].as_str().unwrap().parse().unwrap();
    assert_eq!(frames, 60, "exactly 2.0s × 30fps frames");
    assert!(
        streams.iter().any(|s| s["codec_type"] == "audio"),
        "audio mix must be muxed in"
    );

    // Picture: during the overlay window the center is brighter than the
    // same region before the overlay starts (white at 50% over the base).
    let before = mean_luma_in_center(&dst, 0.25, 0.3);
    let during = mean_luma_in_center(&dst, 1.0, 0.3);
    println!("center luma before overlay {before:.1} | during {during:.1}");
    assert!(
        during > before + 25.0,
        "overlay must brighten the center (before {before:.1}, during {during:.1})"
    );

    // Hygiene.
    assert!(our_ffmpeg_children().is_empty(), "no ffmpeg left behind");
    assert!(!dst.with_extension("mp4.part").exists());
}

/// Cancel mid-encode: the export returns `ExportCancelled` promptly, the
/// ffmpeg child is gone (no zombies), the temp job directory and the
/// `.part` output are removed.
#[test]
fn cancel_mid_export_kills_ffmpeg_and_cleans_up() {
    let _guard = serial();

    // 12 s of 320×180 upscaled to 4K: slow enough that cancel lands
    // mid-encode even on fast machines.
    let src = make_clip("export-cancel-src.mp4", "320x180", 30, 12, 440);
    let mut engine = Engine::new(ProjectSettings::default());
    let a = add_media(&mut engine, &src);
    let tracks = track_ids(&engine);
    engine.add_clip(tracks.video, a, 0.0, 0.0, 12.0).unwrap();
    let project = engine.project().clone();

    let dst = test_dir().join("export-cancel-out.mp4");
    let _ = std::fs::remove_file(&dst);
    let spec = ExportSpec {
        width: 3840,
        height: 2160,
        fps: 30.0,
        quality: ExportQuality::High,
        dst: dst.clone(),
    };

    let dirs_before = export_job_dirs();
    let cancel = std::sync::Arc::new(CancelToken::new());
    let (tx, rx) = std::sync::mpsc::channel();

    let job = {
        let cancel = cancel.clone();
        let project = project.clone();
        let spec = spec.clone();
        std::thread::spawn(move || {
            run_export(&project, &spec, &cancel, &mut |p| {
                let _ = tx.send(p);
            })
        })
    };

    // Wait until video encoding is actually underway, then cancel.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match rx.recv_timeout(deadline - Instant::now()) {
            Ok(p) if p.stage == ExportStage::Video && p.percent > 5.0 => break,
            Ok(_) => continue,
            Err(e) => panic!("no video progress before cancel: {e}"),
        }
    }
    let t0 = Instant::now();
    cancel.cancel();
    let result = job.join().expect("export thread");
    let latency = t0.elapsed();
    println!("cancel → return in {latency:?}");

    assert!(
        matches!(result, Err(cutty_media::MediaError::ExportCancelled)),
        "{result:?}"
    );
    assert!(
        latency < Duration::from_secs(3),
        "cancel must be prompt, took {latency:?}"
    );
    assert!(!dst.exists(), "no output file after cancel");
    assert!(
        !dst.with_extension("mp4.part").exists(),
        "no partial output after cancel"
    );
    assert_eq!(
        export_job_dirs(),
        dirs_before,
        "job temp directory must be removed"
    );

    // Poll briefly: the child must be fully reaped (no zombie state).
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let children = our_ffmpeg_children();
        if children.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "ffmpeg children still present after cancel: {children:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
