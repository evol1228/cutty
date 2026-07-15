//! Phase 1 export acceptance (PLAN.md §5 gate): import a 10-minute
//! 1080p screen recording, cut it to a 60 s short with music on the audio
//! track, export with the Shorts preset — verify duration/resolution/fps
//! exactly, verify the exported audio matches the preview mix, verify the
//! file plays in mpv, verify hardware encode is used when available, and
//! verify cancel-mid-export leaves nothing behind.
//!
//! Run explicitly (generates a 10-minute source on first run):
//! ```sh
//! cargo test -p cutty-media --test export_acceptance -- --ignored --nocapture
//! ```
//! Sources default to the dev machine's test set; override with
//! `CUTTY_SCREENREC` / `CUTTY_MUSIC`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use cutty_audio::{render_timeline_to_wav, AudioSegment, MixerTimeline, EXPORT_SAMPLE_RATE};
use cutty_engine::{Engine, ProjectSettings, TrackKind};
use cutty_media::{
    generate_proxy, probe, proxy_path_for, run_export, CancelToken, ExportQuality, ExportSpec,
    ExportStage,
};
use ffmpeg_sidecar::paths::ffmpeg_path;

const SHORT_SECS: f64 = 60.0;
const FPS: f64 = 30.0;

fn env_path(var: &str, default: &str) -> PathBuf {
    std::env::var(var).unwrap_or_else(|_| default.into()).into()
}

/// The 10-minute 1080p30 "screen recording" (generated once, cached in
/// the shared test-media directory).
fn screen_recording() -> PathBuf {
    let path = env_path(
        "CUTTY_SCREENREC",
        "/home/love/Videos/cutty-test-media/screen-recording-10min.mp4",
    );
    if path.is_file() {
        return path;
    }
    println!("generating 10-minute 1080p source at {} …", path.display());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let status = Command::new(ffmpeg_path())
        .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
        .arg("testsrc2=size=1920x1080:rate=30:duration=600")
        .args(["-f", "lavfi", "-i"])
        .arg("sine=frequency=440:sample_rate=48000:duration=600")
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-g", "60"])
        .args(["-c:a", "aac", "-b:a", "160k", "-shortest"])
        .arg(&path)
        .status()
        .expect("system ffmpeg required");
    assert!(status.success(), "source generation failed");
    path
}

fn music_bed() -> PathBuf {
    let path = env_path(
        "CUTTY_MUSIC",
        "/home/love/Videos/cutty-test-media/music-bed.mp3",
    );
    assert!(path.is_file(), "missing music bed {}", path.display());
    path
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

/// Decode a file's audio to interleaved stereo f32 with the app's stack.
fn decoded_audio(path: &Path) -> (u32, Vec<f32>) {
    use cutty_audio::{AudioSource, SymphoniaSource};
    let mut src = SymphoniaSource::open(path).expect("open audio");
    let rate = src.sample_rate();
    assert_eq!(src.channels(), 2);
    let mut all = Vec::new();
    let mut buf = vec![0f32; 8192];
    loop {
        let n = src.read(&mut buf).expect("read audio");
        if n == 0 {
            break;
        }
        all.extend_from_slice(&buf[..n]);
    }
    (rate, all)
}

/// RMS envelope in fixed-size bins (mono-folded).
fn rms_envelope(samples: &[f32], rate: u32, bin_secs: f64) -> Vec<f64> {
    let bin_frames = (bin_secs * f64::from(rate)) as usize;
    samples
        .chunks(bin_frames * 2)
        .map(|bin| {
            (bin.iter()
                .map(|&s| f64::from(s) * f64::from(s))
                .sum::<f64>()
                / bin.len() as f64)
                .sqrt()
        })
        .collect()
}

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
    std::fs::read_dir(cache.join("cutty").join("export"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect()
}

/// Build the acceptance project: eight 7.5 s cuts spread across the
/// 10-minute recording (60 s total) + the music bed on the audio track at
/// reduced volume. Returns (project, engine kept alive for ids).
fn build_short(recording: &Path, music: &Path) -> cutty_engine::Project {
    let rec_info = probe(recording).expect("probe recording");
    let rec_video = rec_info.video.as_ref().expect("recording has video");
    assert!(
        rec_video.width >= 1920 && rec_video.height >= 1080,
        "acceptance needs a 1080p+ source, got {}x{}",
        rec_video.width,
        rec_video.height
    );
    assert!(
        rec_info.duration_sec >= 595.0,
        "acceptance needs a ~10-minute source, got {:.1}s",
        rec_info.duration_sec
    );
    let music_info = probe(music).expect("probe music");

    let mut engine = Engine::new(ProjectSettings::default());
    let rec = engine
        .add_media(
            recording.display().to_string(),
            rec_info.duration_sec,
            true,
            rec_info.audio.is_some(),
        )
        .unwrap();
    let mus = engine
        .add_media(
            music.display().to_string(),
            music_info.duration_sec,
            false,
            true,
        )
        .unwrap();
    let video_track = engine
        .project()
        .tracks
        .iter()
        .find(|t| t.kind == TrackKind::Video)
        .unwrap()
        .id;
    let audio_track = engine
        .project()
        .tracks
        .iter()
        .find(|t| t.kind == TrackKind::Audio)
        .unwrap()
        .id;

    // Eight cuts of 7.5 s from minutes 0..8 of the recording ("cut the
    // 10-minute recording down to a 60 s short").
    for k in 0..8u32 {
        let t = f64::from(k) * 7.5;
        let source_in = 30.0 + f64::from(k) * 65.0;
        engine
            .add_clip(video_track, rec, t, source_in, source_in + 7.5)
            .unwrap();
    }
    // Music bed (30 s file) looped twice across the short, ducked to 0.5.
    let m1 = engine.add_clip(audio_track, mus, 0.0, 0.0, 29.9).unwrap();
    let m2 = engine.add_clip(audio_track, mus, 29.9, 0.0, 29.9).unwrap();
    engine.set_clip_volume(m1, 0.5).unwrap();
    engine.set_clip_volume(m2, 0.5).unwrap();

    let project = engine.project().clone();
    let end = cutty_engine::timeline_end(&project);
    assert!((end - SHORT_SECS).abs() < 0.31, "short is {end:.2}s");
    project
}

/// The preview mix for this project, rendered by the same offline path
/// the export uses — decoded from the same files playback decodes (proxy
/// audio for the recording, the original for the music).
fn preview_mix_timeline(project: &cutty_engine::Project) -> MixerTimeline {
    let mut segments = Vec::new();
    for track in project.tracks.iter().filter(|t| !t.muted) {
        for clip in &track.clips {
            let media = project.media(clip.media_id).unwrap();
            if !media.has_audio {
                continue;
            }
            let path = if media.has_video {
                let (proxy, exists) = proxy_path_for(Path::new(&media.path)).unwrap();
                assert!(exists, "proxy must exist after import");
                proxy
            } else {
                PathBuf::from(&media.path)
            };
            segments.push(AudioSegment {
                path,
                timeline_in: clip.timeline_in,
                timeline_out: clip.timeline_out,
                source_in: clip.source_in,
                speed: clip.speed,
                volume: clip.volume,
            });
        }
    }
    MixerTimeline { segments }
}

/// Phase 2 acceptance: a 3-video-track project (opacity + transforms)
/// exports correctly through the GPU compositor at 1080p and 4K.
/// 1080p must hold the ≥1×-realtime budget when hardware encode is
/// available; readback throughput is printed for the session summary.
#[test]
#[ignore = "heavy: full compositor exports at 1080p and 4K; run with --ignored"]
fn phase2_compositor_export_acceptance() {
    let recording = screen_recording();
    let rec_info = probe(&recording).expect("probe recording");

    const LEN: f64 = 20.0;
    let mut engine = Engine::new(ProjectSettings::default());
    let rec = engine
        .add_media(
            recording.display().to_string(),
            rec_info.duration_sec,
            true,
            rec_info.audio.is_some(),
        )
        .unwrap();
    let video_track = engine
        .project()
        .tracks
        .iter()
        .find(|t| t.kind == TrackKind::Video)
        .unwrap()
        .id;
    engine
        .add_clip(video_track, rec, 0.0, 30.0, 30.0 + LEN)
        .unwrap();

    // Two overlay tracks: the same recording at different offsets,
    // scaled/positioned/rotated with opacity — a picture-in-picture stack.
    let mut project = engine.project().clone();
    let overlay = |track_id: u64,
                   clip_id: u64,
                   source_in: f64,
                   x: f64,
                   y: f64,
                   scale: f64,
                   rotation: f64,
                   opacity: f64| {
        cutty_engine::Track {
            id: cutty_engine::TrackId(track_id),
            kind: TrackKind::Video,
            name: format!("V{track_id}"),
            locked: false,
            muted: false,
            hidden: false,
            clips: vec![cutty_engine::Clip {
                id: cutty_engine::ClipId(clip_id),
                media_id: rec,
                timeline_in: 0.0,
                timeline_out: LEN,
                source_in,
                source_out: source_in + LEN,
                transform: cutty_engine::Transform {
                    x,
                    y,
                    scale,
                    rotation,
                },
                opacity,
                blend_mode: cutty_engine::BlendMode::Normal,
                speed: 1.0,
                volume: 0.0,
            }],
        }
    };
    project
        .tracks
        .insert(0, overlay(700, 701, 120.0, -480.0, -270.0, 0.3, 0.0, 0.85));
    project
        .tracks
        .insert(0, overlay(702, 703, 240.0, 480.0, 270.0, 0.25, 12.0, 0.6));
    project.validate().expect("fixture is valid");

    for (label, w, h, realtime_gate) in
        [("1080p", 1920u32, 1080u32, true), ("4K", 3840, 2160, false)]
    {
        let dst = std::env::temp_dir().join(format!("cutty-media-tests/phase2-{label}.mp4"));
        let _ = std::fs::remove_file(&dst);
        let spec = ExportSpec {
            width: w,
            height: h,
            fps: FPS,
            quality: ExportQuality::Medium,
            dst: dst.clone(),
        };
        let cancel = CancelToken::new();
        let t0 = Instant::now();
        let summary = run_export(&project, &spec, &cancel, &mut |_| {}).expect("compositor export");
        let wall = t0.elapsed();
        let speed = LEN / wall.as_secs_f64();
        println!(
            "{label}: {wall:.1?} for {LEN:.0}s ({speed:.2}x realtime) via {} — renderer {}",
            summary.encoder, summary.renderer
        );
        assert_eq!(summary.renderer, "gpu-compositor");
        if realtime_gate && summary.hardware_encode {
            assert!(
                speed >= 1.0,
                "{label} compositor export below realtime: {speed:.2}x"
            );
        }

        let info = ffprobe_json(&dst);
        let duration: f64 = info["format"]["duration"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!((duration - LEN).abs() < 0.05, "{label} duration {duration}");
        let streams = info["streams"].as_array().unwrap();
        let video = streams
            .iter()
            .find(|s| s["codec_type"] == "video")
            .expect("video stream");
        assert_eq!(video["width"], w);
        assert_eq!(video["height"], h);
        assert_eq!(video["r_frame_rate"], "30/1");
        let frames: u64 = video["nb_read_frames"].as_str().unwrap().parse().unwrap();
        assert_eq!(frames, (LEN * FPS) as u64, "{label} exact frame count");
        assert!(
            streams.iter().any(|s| s["codec_type"] == "audio"),
            "{label} audio muxed"
        );

        // Plays in a real player.
        let mpv = Command::new("mpv")
            .args([
                "--no-config",
                "--ao=null",
                "--vo=null",
                "--frames=30",
                "--quiet",
            ])
            .arg(&dst)
            .output()
            .expect("mpv must be installed for acceptance");
        assert!(mpv.status.success(), "{label}: mpv failed to decode");
    }
    assert!(our_ffmpeg_children().is_empty(), "no ffmpeg left behind");
}

#[test]
#[ignore = "heavy: generates a 10-minute source and runs a full export; run with --ignored"]
fn phase1_export_acceptance() {
    // --- "Import": probe + proxy generation (cached across runs) ---
    let recording = screen_recording();
    let music = music_bed();
    let t0 = Instant::now();
    generate_proxy(&recording, None, |_| {}).expect("proxy");
    println!("import (proxy) ready in {:.1?}", t0.elapsed());

    let project = build_short(&recording, &music);

    // --- Export with the Shorts preset ---
    let dst = std::env::temp_dir().join("cutty-media-tests/acceptance-short.mp4");
    let _ = std::fs::remove_file(&dst);
    let spec = ExportSpec {
        width: 1080,
        height: 1920,
        fps: FPS,
        quality: ExportQuality::High,
        dst: dst.clone(),
    };
    let cancel = CancelToken::new();
    let t0 = Instant::now();
    let mut last_percent = -1.0f32;
    let summary = run_export(&project, &spec, &cancel, &mut |p| {
        assert!(
            p.percent >= last_percent,
            "progress went backwards: {} -> {}",
            last_percent,
            p.percent
        );
        last_percent = p.percent;
    })
    .expect("export succeeds");
    let export_wall = t0.elapsed();

    // Hardware-encode verification "via logs": detection logged its
    // choice at startup; the summary records what was actually used.
    println!(
        "export: {:.1?} wall for {:.0}s of 1080x1920@30 — encoder {} (hardware: {})",
        export_wall, SHORT_SECS, summary.encoder, summary.hardware_encode
    );
    let detected = cutty_media::detected_h264_encoder();
    assert_eq!(summary.encoder, detected.ffmpeg_name());
    assert_eq!(summary.hardware_encode, detected.is_hardware());
    if summary.hardware_encode {
        // Performance budget: export ≥ 1× realtime at 1080-class output
        // with hardware encode.
        assert!(
            export_wall.as_secs_f64() < SHORT_SECS,
            "hw export slower than realtime: {export_wall:?}"
        );
    }

    // --- Exactly the right duration, resolution, fps ---
    let info = ffprobe_json(&dst);
    let duration: f64 = info["format"]["duration"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        (duration - SHORT_SECS).abs() < 0.05,
        "duration {duration} ≠ {SHORT_SECS} ± 0.05"
    );
    let streams = info["streams"].as_array().unwrap();
    let video = streams
        .iter()
        .find(|s| s["codec_type"] == "video")
        .expect("video stream");
    assert_eq!(video["width"], 1080);
    assert_eq!(video["height"], 1920);
    assert_eq!(video["r_frame_rate"], "30/1");
    assert_eq!(video["codec_name"], "h264");
    let frames: u64 = video["nb_read_frames"].as_str().unwrap().parse().unwrap();
    assert_eq!(frames, (SHORT_SECS * FPS) as u64, "exact frame count");
    let audio = streams
        .iter()
        .find(|s| s["codec_type"] == "audio")
        .expect("audio stream");
    assert_eq!(audio["codec_name"], "aac");
    assert_eq!(audio["sample_rate"], "48000");

    // --- Audio in sync and == the preview mix ---
    // Render the preview mix through the same mixer playback uses, then
    // compare 50 ms RMS envelopes against the exported AAC. An offset of
    // even one bin (music entry, cut points) or a wrong gain would blow
    // the tolerance.
    let wav = std::env::temp_dir().join("cutty-media-tests/acceptance-preview-mix.wav");
    render_timeline_to_wav(
        preview_mix_timeline(&project),
        EXPORT_SAMPLE_RATE,
        (SHORT_SECS * f64::from(EXPORT_SAMPLE_RATE)) as u64,
        &wav,
        &|| false,
        &mut |_, _| {},
    )
    .expect("preview mix renders");

    let (rate_a, exported) = decoded_audio(&dst);
    let (rate_b, preview) = decoded_audio(&wav);
    assert_eq!(rate_a, 48_000);
    assert_eq!(rate_b, 48_000);
    let env_exported = rms_envelope(&exported, rate_a, 0.05);
    let env_preview = rms_envelope(&preview, rate_b, 0.05);
    let bins = env_exported.len().min(env_preview.len());
    assert!(bins > 1100, "expected ~1200 bins over 60s, got {bins}");
    let mut worst = 0.0f64;
    let mut worst_bin = 0usize;
    // Skip the first/last bins (AAC priming/tail edges).
    for i in 2..bins - 2 {
        let diff = (env_exported[i] - env_preview[i]).abs();
        if diff > worst {
            worst = diff;
            worst_bin = i;
        }
    }
    let mean_level = env_preview.iter().sum::<f64>() / env_preview.len() as f64;
    println!(
        "mix comparison: mean level {mean_level:.4}, worst bin diff {worst:.4} at {:.2}s",
        worst_bin as f64 * 0.05
    );
    assert!(mean_level > 0.02, "preview mix must not be silent");
    assert!(
        worst < 0.03,
        "exported audio diverges from the preview mix: {worst:.4} at bin {worst_bin}"
    );

    // --- Output opens in a real player (headless mpv decode) ---
    let mpv = Command::new("mpv")
        .args([
            "--no-config",
            "--ao=null",
            "--vo=null",
            "--frames=60",
            "--quiet",
        ])
        .arg(&dst)
        .output()
        .expect("mpv must be installed for acceptance");
    assert!(
        mpv.status.success(),
        "mpv failed to play the export: {}",
        String::from_utf8_lossy(&mpv.stderr)
    );
    println!("mpv decoded the export cleanly");

    // --- Cancel mid-export: no zombies, no temp files ---
    let dst2 = std::env::temp_dir().join("cutty-media-tests/acceptance-cancelled.mp4");
    let _ = std::fs::remove_file(&dst2);
    let dirs_before = export_job_dirs();
    let cancel = std::sync::Arc::new(CancelToken::new());
    let (tx, rx) = std::sync::mpsc::channel();
    let job = {
        let cancel = cancel.clone();
        let project = project.clone();
        let spec = ExportSpec {
            dst: dst2.clone(),
            ..spec
        };
        std::thread::spawn(move || {
            run_export(&project, &spec, &cancel, &mut |p| {
                let _ = tx.send(p);
            })
        })
    };
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match rx.recv_timeout(deadline - Instant::now()) {
            Ok(p) if p.stage == ExportStage::Video && p.percent > 20.0 => break,
            Ok(_) => continue,
            Err(e) => panic!("no mid-export progress to cancel at: {e}"),
        }
    }
    cancel.cancel();
    let result = job.join().expect("export thread");
    assert!(
        matches!(result, Err(cutty_media::MediaError::ExportCancelled)),
        "{result:?}"
    );
    assert!(!dst2.exists() && !dst2.with_extension("mp4.part").exists());
    assert_eq!(export_job_dirs(), dirs_before, "temp job dir removed");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !our_ffmpeg_children().is_empty() {
        assert!(Instant::now() < deadline, "ffmpeg children left behind");
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("cancel: clean (no zombies, no temp files, no partial output)");
}
