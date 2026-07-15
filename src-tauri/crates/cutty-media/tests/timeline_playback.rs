//! Timeline playback integration tests: the multi-cut acceptance
//! criteria, against real (generated) media through the real pipeline —
//! originals → proxies → TimelinePlayer.
//!
//! Sources are solid red/green/blue clips, so every presented JPEG can be
//! traced back to the source it must have come from. The timeline is 12
//! hard cuts across the 3 sources, including a same-source jump cut, a
//! pure split point, and a gap.

use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use cutty_engine::{
    Engine, MediaId, Project, ProjectSettings, TrackFlag, TrackId, TrackKind, Transform,
};
use cutty_media::{generate_proxy, PlayerEvent, TimelinePlayer};

const FPS: f64 = 30.0;
const FRAME: f64 = 1.0 / FPS;

// ---------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------

fn media_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("cutty-playback-tests");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A 10 s solid-color 640×360 H.264+AAC source (tone differs per color so
/// the mixer has real audio to chew on).
fn color_source(color: &str, tone_hz: u32) -> PathBuf {
    let file = media_dir().join(format!("src-{color}.mp4"));
    if file.is_file() {
        return file;
    }
    let status = Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
        .arg(format!("color=c={color}:size=640x360:rate=30:duration=10"))
        .args(["-f", "lavfi", "-i"])
        .arg(format!(
            "sine=frequency={tone_hz}:sample_rate=48000:duration=10"
        ))
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-g", "30"])
        .args(["-c:a", "aac", "-b:a", "96k", "-ac", "2", "-shortest"])
        .arg(&file)
        .status()
        .expect("system ffmpeg required");
    assert!(status.success());
    file
}

/// An 9 s WAV for the audio track (audio-only media plays from the
/// original file, no proxy).
fn wav_source() -> PathBuf {
    let file = media_dir().join("bed.wav");
    if file.is_file() {
        return file;
    }
    let status = Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
        .arg("sine=frequency=220:sample_rate=48000:duration=9")
        .args(["-c:a", "pcm_s16le"])
        .arg(&file)
        .status()
        .expect("system ffmpeg required");
    assert!(status.success());
    file
}

struct Fixture {
    project: Project,
    /// (timeline_in, timeline_out, color) per video segment, sorted.
    segments: Vec<(f64, f64, &'static str)>,
    /// [start, end) of the gap.
    gap: (f64, f64),
    end: f64,
}

/// Build the acceptance timeline: 12 cuts across 3 sources + a gap.
fn build_fixture() -> Fixture {
    let red = color_source("red", 300);
    let green = color_source("green", 440);
    let blue = color_source("blue", 660);
    // Real pipeline: playback decodes proxies, so generate them.
    for src in [&red, &green, &blue] {
        generate_proxy(src, Some(10.0), |_| {}).expect("proxy");
    }

    let mut engine = Engine::new(ProjectSettings::default());
    let r = engine
        .add_media(red.display().to_string(), 10.0, true, true)
        .unwrap();
    let g = engine
        .add_media(green.display().to_string(), 10.0, true, true)
        .unwrap();
    let b = engine
        .add_media(blue.display().to_string(), 10.0, true, true)
        .unwrap();
    let bed = engine
        .add_media(wav_source().display().to_string(), 9.0, false, true)
        .unwrap();

    let video = track_of(&engine, TrackKind::Video);
    let audio = track_of(&engine, TrackKind::Audio);

    // (media, source_in, duration, color) — placed back to back except
    // for the gap at 5.0..5.6. Includes a same-source jump cut (B 0.5→
    // then B 5.0→) and a pure split point (R 4.0..4.7 → R 4.7..5.4).
    let layout: &[(MediaId, f64, f64, &str)] = &[
        (r, 1.0, 0.8, "red"),
        (g, 2.0, 0.7, "green"),
        (b, 0.5, 0.8, "blue"),
        (b, 5.0, 0.6, "blue"), // same-source jump cut
        (r, 4.0, 0.7, "red"),
        (r, 4.7, 0.7, "red"), // split-point continuation
        (g, 6.0, 0.7, "green"),
    ];
    let mut t = 0.0;
    let mut segments = Vec::new();
    for &(media, s_in, dur, color) in layout {
        engine.add_clip(video, media, t, s_in, s_in + dur).unwrap();
        segments.push((t, t + dur, color));
        t += dur;
    }
    let gap = (t, t + 0.6);
    t += 0.6; // ---- the gap ----
    for &(media, s_in, dur, color) in &[
        (b, 2.0, 0.7, "blue"),
        (r, 0.0, 0.6, "red"),
        (g, 0.5, 0.6, "green"),
        (b, 7.0, 0.6, "blue"),
        (r, 8.0, 0.7, "red"),
    ] {
        engine.add_clip(video, media, t, s_in, s_in + dur).unwrap();
        segments.push((t, t + dur, color));
        t += dur;
    }
    // Music bed on the audio track for the whole piece.
    engine.add_clip(audio, bed, 0.0, 0.0, t.min(9.0)).unwrap();

    Fixture {
        project: engine.project().clone(),
        segments,
        gap,
        end: t,
    }
}

fn track_of(engine: &Engine, kind: TrackKind) -> TrackId {
    engine
        .project()
        .tracks
        .iter()
        .find(|t| t.kind == kind)
        .unwrap()
        .id
}

// ---------------------------------------------------------------------
// Event capture + frame classification
// ---------------------------------------------------------------------

struct FrameRec {
    pts: f64,
    clock: f64,
    arrived: Instant,
    color: &'static str,
}

enum Evt {
    Frame(FrameRec),
    Position(#[allow(dead_code)] f64, #[allow(dead_code)] bool),
    Eof,
    Error(String),
}

/// Classify a JPEG by its dominant primary. The gap frame is 16×16 black.
fn classify(jpeg: &[u8]) -> &'static str {
    let image: turbojpeg::Image<Vec<u8>> =
        turbojpeg::decompress(jpeg, turbojpeg::PixelFormat::RGB).expect("valid jpeg");
    let (mut r, mut g, mut b) = (0u64, 0u64, 0u64);
    let mut samples = 0u64;
    for y in (0..image.height).step_by(16.max(image.height / 8)) {
        for x in (0..image.width).step_by(16.max(image.width / 8)) {
            let i = y * image.pitch + x * 3;
            r += u64::from(image.pixels[i]);
            g += u64::from(image.pixels[i + 1]);
            b += u64::from(image.pixels[i + 2]);
            samples += 1;
        }
    }
    let (r, g, b) = (r / samples, g / samples, b / samples);
    if r < 40 && g < 40 && b < 40 {
        "black"
    } else if r > g && r > b {
        "red"
    } else if g > r && g > b {
        "green"
    } else {
        "blue"
    }
}

fn open_player(project: Project) -> (TimelinePlayer, mpsc::Receiver<Evt>) {
    let (tx, rx) = mpsc::channel();
    let player = TimelinePlayer::open(
        project,
        Box::new(move |e| {
            let evt = match e {
                PlayerEvent::Frame {
                    pts_sec,
                    clock_sec,
                    jpeg,
                    ..
                } => Evt::Frame(FrameRec {
                    pts: pts_sec,
                    clock: clock_sec,
                    arrived: Instant::now(),
                    color: classify(&jpeg),
                }),
                PlayerEvent::Position {
                    position_sec,
                    playing,
                } => Evt::Position(position_sec, playing),
                PlayerEvent::Eof => Evt::Eof,
                PlayerEvent::Error(e) => Evt::Error(e),
            };
            let _ = tx.send(evt);
        }),
    )
    .expect("player opens");
    (player, rx)
}

fn expected_color(fixture: &Fixture, pts: f64) -> &'static str {
    for &(t_in, t_out, color) in &fixture.segments {
        if t_in - 1e-6 <= pts && pts < t_out - 1e-6 {
            return color;
        }
    }
    "black"
}

/// Wait for the next Frame event (panicking on player errors that aren't
/// benign audio-device fallbacks).
fn recv_frame(rx: &mpsc::Receiver<Evt>, timeout: Duration) -> Option<FrameRec> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        match rx.recv_timeout(remaining) {
            Ok(Evt::Frame(f)) => return Some(f),
            Ok(Evt::Error(e)) if e.contains("audio unavailable") => continue,
            Ok(Evt::Error(e)) => panic!("player error: {e}"),
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

// ---------------------------------------------------------------------
// The acceptance tests
// ---------------------------------------------------------------------

/// These tests pace real playback against a real clock — running them
/// concurrently distorts each other's timing. One at a time.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Phase 2 acceptance: a 3-video-track project with opacity and
/// transforms sustains ≥30 fps at 720p-class preview (compositing all
/// three layers per output frame), with bounded A/V drift.
#[test]
fn three_track_composite_playback_sustains_30fps() {
    let _serial = serial();

    // 720p-class sources so each layer decodes a real 1280×720 proxy.
    let make_hd = |color: &str, tone: u32| -> PathBuf {
        let file = media_dir().join(format!("src-hd-{color}.mp4"));
        if !file.is_file() {
            let status = Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
                .arg(format!("color=c={color}:size=1280x720:rate=30:duration=8"))
                .args(["-f", "lavfi", "-i"])
                .arg(format!(
                    "sine=frequency={tone}:sample_rate=48000:duration=8"
                ))
                .args(["-c:v", "libx264", "-preset", "ultrafast", "-g", "30"])
                .args(["-c:a", "aac", "-b:a", "96k", "-ac", "2", "-shortest"])
                .arg(&file)
                .status()
                .expect("system ffmpeg required");
            assert!(status.success());
        }
        file
    };
    let base_src = make_hd("darkred", 300);
    let mid_src = make_hd("seagreen", 440);
    let top_src = make_hd("navy", 660);
    for src in [&base_src, &mid_src, &top_src] {
        generate_proxy(src, Some(8.0), |_| {}).expect("proxy");
    }

    const PLAY_SECS: f64 = 4.0;
    let mut engine = Engine::new(ProjectSettings::default());
    let base = engine
        .add_media(base_src.display().to_string(), 8.0, true, true)
        .unwrap();
    let mid = engine
        .add_media(mid_src.display().to_string(), 8.0, true, true)
        .unwrap();
    let top = engine
        .add_media(top_src.display().to_string(), 8.0, true, true)
        .unwrap();
    let video = track_of(&engine, TrackKind::Video);
    engine.add_clip(video, base, 0.0, 0.0, PLAY_SECS).unwrap();

    // Two overlay tracks above the base, both transformed + translucent —
    // built through the real track/property commands, exactly as the UI
    // drives them.
    for (media, x, scale, rotation, opacity) in
        [(mid, -320.0, 0.45, 0.0, 0.7), (top, 320.0, 0.35, 20.0, 0.5)]
    {
        let track = engine.add_track(TrackKind::Video, 0).unwrap();
        let clip = engine
            .add_clip(track, media, 0.0, 0.5, 0.5 + PLAY_SECS)
            .unwrap();
        engine
            .set_clip_transform(
                clip,
                Transform {
                    x,
                    y: -80.0,
                    scale,
                    rotation,
                },
            )
            .unwrap();
        engine.set_clip_opacity(clip, opacity).unwrap();
        engine.set_clip_volume(clip, 0.0).unwrap();
    }

    let (player, rx) = open_player(engine.project().clone());
    let first = recv_frame(&rx, Duration::from_secs(10)).expect("preview frame");
    assert_ne!(first.color, "black", "three layers must composite");

    player.play();
    let mut frames: Vec<FrameRec> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(PLAY_SECS as u64 + 5);
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Evt::Frame(f)) => frames.push(f),
            Ok(Evt::Eof) => break,
            Ok(Evt::Error(e)) if e.contains("audio unavailable") => {}
            Ok(Evt::Error(e)) => panic!("player error: {e}"),
            Ok(Evt::Position(..)) => {}
            Err(_) => panic!("no events for 2s during 3-track playback"),
        }
    }
    player.pause();

    // ≥30fps: essentially every grid frame presented (tolerate warmup).
    let expected = (PLAY_SECS * FPS) as usize;
    assert!(
        frames.len() >= expected - 6,
        "only {} of {} output frames presented (compositing too slow)",
        frames.len(),
        expected
    );

    // Sustained cadence: no hitch beyond 2.5 frame durations.
    let mut worst_gap = Duration::ZERO;
    for pair in frames.windows(2).skip(5) {
        let dt = pair[1].arrived.duration_since(pair[0].arrived);
        worst_gap = worst_gap.max(dt);
        assert!(
            dt < Duration::from_secs_f64(2.5 * FRAME),
            "hitch: {dt:?} at pts {:.3}",
            pair[0].pts
        );
    }

    // A/V drift bounded, same criterion as single-track playback.
    let max_drift = frames
        .iter()
        .skip(5)
        .map(|f| (f.pts - f.clock).abs())
        .fold(0.0f64, f64::max);
    println!(
        "3-track composite: {} frames, worst arrival gap {worst_gap:?}, max |pts-clock| {:.1} ms",
        frames.len(),
        max_drift * 1e3
    );
    assert!(max_drift < 0.040, "A/V drift {:.1} ms", max_drift * 1e3);
}

/// Phase 2 acceptance: playback through **five consecutive transitions**
/// holds the 30 fps cadence with no hitch — every span decodes two
/// 720p-class streams at once while the GPU runs the transition shader.
#[test]
fn five_consecutive_transitions_play_without_hitching() {
    let _serial = serial();

    // 720p sources so each side of a span decodes a real 1280×720 proxy.
    let make_hd = |color: &str, tone: u32| -> PathBuf {
        let file = media_dir().join(format!("src-hd-tr-{color}.mp4"));
        if !file.is_file() {
            let status = Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
                .arg(format!("color=c={color}:size=1280x720:rate=30:duration=8"))
                .args(["-f", "lavfi", "-i"])
                .arg(format!(
                    "sine=frequency={tone}:sample_rate=48000:duration=8"
                ))
                .args(["-c:v", "libx264", "-preset", "ultrafast", "-g", "30"])
                .args(["-c:a", "aac", "-b:a", "96k", "-ac", "2", "-shortest"])
                .arg(&file)
                .status()
                .expect("system ffmpeg required");
            assert!(status.success());
        }
        file
    };
    let sources = [
        ("red", make_hd("red", 300)),
        ("green", make_hd("green", 440)),
        ("blue", make_hd("blue", 660)),
    ];
    for (_, src) in &sources {
        generate_proxy(src, Some(8.0), |_| {}).expect("proxy");
    }

    // Six 1.2s clips back to back — five cuts, a different transition on
    // each (mixing cheap wipes with the heavy multi-tap shaders).
    let mut engine = Engine::new(ProjectSettings::default());
    let media: Vec<MediaId> = sources
        .iter()
        .map(|(_, src)| {
            engine
                .add_media(src.display().to_string(), 8.0, true, true)
                .unwrap()
        })
        .collect();
    let video = track_of(&engine, TrackKind::Video);
    let mut clips = Vec::new();
    let mut colors = Vec::new();
    for i in 0..6 {
        let t = i as f64 * 1.2;
        let clip = engine
            .add_clip(video, media[i % 3], t, 0.5, 1.7)
            .unwrap();
        clips.push(clip);
        colors.push(sources[i % 3].0);
    }
    for (i, kind) in ["fade", "wipeleft", "circleopen", "crosszoom", "linearblur"]
        .iter()
        .enumerate()
    {
        engine
            .set_transition(
                clips[i],
                Some(cutty_engine::Transition {
                    kind: (*kind).to_string(),
                    duration: 0.5,
                }),
            )
            .unwrap();
    }
    let end = 6.0 * 1.2;
    let spans: Vec<(f64, f64)> = (1..6)
        .map(|i| (i as f64 * 1.2 - 0.25, i as f64 * 1.2 + 0.25))
        .collect();

    let (player, rx) = open_player(engine.project().clone());
    let first = recv_frame(&rx, Duration::from_secs(10)).expect("preview frame");
    assert_eq!(first.color, "red");

    player.play();
    let mut frames: Vec<FrameRec> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(end as u64 + 6);
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Evt::Frame(f)) => frames.push(f),
            Ok(Evt::Eof) => break,
            Ok(Evt::Error(e)) if e.contains("audio unavailable") => {}
            Ok(Evt::Error(e)) => panic!("player error: {e}"),
            Ok(Evt::Position(..)) => {}
            Err(_) => panic!("no events for 2s during transition playback"),
        }
    }
    player.pause();

    // ≥30 fps: essentially every grid frame presented (tolerate warmup).
    let expected = (end * FPS) as usize;
    assert!(
        frames.len() >= expected - 8,
        "only {} of {} output frames presented (two-stream decode too slow)",
        frames.len(),
        expected
    );

    // No hitch anywhere — especially not at span entries, where the
    // second decoder starts streaming.
    let mut worst_gap = Duration::ZERO;
    for pair in frames.windows(2).skip(5) {
        let dt = pair[1].arrived.duration_since(pair[0].arrived);
        worst_gap = worst_gap.max(dt);
        assert!(
            dt < Duration::from_secs_f64(2.5 * FRAME),
            "hitch: {dt:?} at pts {:.3}",
            pair[0].pts
        );
    }
    println!("5-transition playback: {} frames, worst gap {worst_gap:?}", frames.len());

    // Content sanity: outside every span the presented frame shows its
    // clip's solid color; inside a span it shows one/both neighbors
    // (blends classify as either side), never black.
    let in_span = |pts: f64| spans.iter().any(|&(s, e)| pts >= s - 1e-6 && pts < e + 1e-6);
    for f in frames.iter().skip(3) {
        if in_span(f.pts) {
            assert_ne!(f.color, "black", "span frame at {:.3} went black", f.pts);
        } else if f.pts < end - 1e-6 {
            let idx = ((f.pts / 1.2).floor() as usize).min(5);
            assert_eq!(
                f.color, colors[idx],
                "frame at {:.3} showed {} (expected {})",
                f.pts, f.color, colors[idx]
            );
        }
    }

    // A/V drift stays bounded through the overlaps. Print the worst
    // offenders first — the pts values say which span (if any) lagged.
    let mut lags: Vec<(f64, f64)> = frames
        .iter()
        .skip(5)
        .map(|f| (f.pts, (f.pts - f.clock).abs()))
        .collect();
    lags.sort_by(|a, b| b.1.total_cmp(&a.1));
    for (pts, lag) in lags.iter().take(8) {
        println!("lag {:6.1} ms at pts {pts:.3}", lag * 1e3);
    }
    let max_drift = lags.first().map(|&(_, lag)| lag).unwrap_or(0.0);
    assert!(max_drift < 0.040, "A/V drift {:.1} ms", max_drift * 1e3);
}

/// Play the whole 12-cut timeline through and check every criterion that
/// doesn't need an hour of soak: correct source at every presented
/// frame, no hitch at any cut, bounded A/V drift, black in the gap, Eof.
#[test]
fn plays_through_twelve_cuts_without_hitching() {
    let _serial = serial();
    let fixture = build_fixture();
    let (player, rx) = open_player(fixture.project.clone());

    // Opening preview frame (frame 0 = red).
    let first = recv_frame(&rx, Duration::from_secs(10)).expect("preview frame");
    assert_eq!(first.color, "red", "opening frame must be the first clip");

    player.play();
    let start = Instant::now();
    let mut frames: Vec<FrameRec> = Vec::new();
    let mut saw_eof = false;
    while start.elapsed() < Duration::from_secs((fixture.end + 6.0) as u64) {
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Evt::Frame(f)) => frames.push(f),
            Ok(Evt::Eof) => {
                saw_eof = true;
                break;
            }
            Ok(Evt::Error(e)) if e.contains("audio unavailable") => {}
            Ok(Evt::Error(e)) => panic!("player error during playback: {e}"),
            Ok(Evt::Position(..)) => {}
            Err(_) => panic!("no events for 2s during playback"),
        }
    }
    assert!(saw_eof, "must reach Eof after the last clip");

    // ~8.8s at 30fps ⇒ ~264 content frames (+1 black gap frame).
    assert!(frames.len() > 220, "only {} frames presented", frames.len());

    // 1) Every frame shows the correct source's content.
    for f in &frames {
        let expected = expected_color(&fixture, f.pts);
        assert_eq!(
            f.color, expected,
            "frame at pts {:.3} showed {} (expected {expected})",
            f.pts, f.color
        );
    }

    // 2) Every segment (and the gap) was actually shown.
    for &(t_in, t_out, color) in &fixture.segments {
        let shown = frames
            .iter()
            .filter(|f| f.pts >= t_in - 1e-6 && f.pts < t_out - 1e-6)
            .count();
        assert!(
            shown >= 10,
            "segment [{t_in:.1}, {t_out:.1}) {color} only presented {shown} frames"
        );
    }
    assert!(
        frames.iter().any(|f| f.color == "black"),
        "the gap must present a black frame"
    );

    // 3) No hitch at any cut: wall-clock arrival gaps stay under 2.5
    //    frame durations, everywhere past warmup. (The gap's single
    //    black frame is followed by the next clip's first frame one
    //    gap-length later — skip pairs that span it.)
    let mut worst_gap = Duration::ZERO;
    for pair in frames.windows(2).skip(5) {
        let spans_gap = pair[0].pts < fixture.gap.1 && pair[1].pts >= fixture.gap.1 - 1e-6
            || pair[0].color == "black";
        if spans_gap {
            continue;
        }
        let dt = pair[1].arrived.duration_since(pair[0].arrived);
        worst_gap = worst_gap.max(dt);
        assert!(
            dt < Duration::from_secs_f64(2.5 * FRAME),
            "hitch: {dt:?} between pts {:.3} and {:.3}",
            pair[0].pts,
            pair[1].pts
        );
    }
    println!("worst inter-frame arrival gap: {worst_gap:?}");

    // 4) Presented-frame continuity: pts step ≈ one frame everywhere
    //    except across the gap (max one dropped frame tolerated).
    for pair in frames.windows(2).skip(5) {
        if pair[0].color == "black" || pair[1].color == "black" {
            continue;
        }
        let dpts = pair[1].pts - pair[0].pts;
        assert!(
            dpts < 2.5 * FRAME + 1e-6,
            "presented pts jumped {dpts:.3}s at {:.3} (>1 dropped frame)",
            pair[0].pts
        );
    }

    // 5) A/V sync: presented pts tracks the master clock.
    let max_drift = frames
        .iter()
        .skip(5)
        .filter(|f| f.color != "black")
        .map(|f| (f.pts - f.clock).abs())
        .fold(0.0f64, f64::max);
    println!("max |pts - clock|: {:.1} ms", max_drift * 1e3);
    assert!(
        max_drift < 0.040,
        "A/V drift {:.1} ms ≥ 40 ms",
        max_drift * 1e3
    );
}

/// Scrubbing while paused lands on the correct frame — including cold
/// positions in sources never decoded before — within the 100 ms budget.
#[test]
fn paused_scrubbing_shows_the_correct_frame_within_budget() {
    let _serial = serial();
    let fixture = build_fixture();
    let (player, rx) = open_player(fixture.project.clone());
    let _ = recv_frame(&rx, Duration::from_secs(10)).expect("preview");

    // Positions chosen to hop across all three sources, the gap, and a
    // never-visited region; all cold (fresh player, no playback yet).
    let targets = [
        4.05,                // red (split region)
        1.2,                 // green
        2.55,                // blue (jump-cut segment)
        fixture.gap.0 + 0.3, // gap → black
        6.5,                 // red near source start
        8.3,                 // red last clip
        0.4,                 // red first clip
    ];
    let mut latencies = Vec::new();
    for &t in &targets {
        while rx.try_recv().is_ok() {} // drain
        let t0 = Instant::now();
        player.seek(t);
        let f = recv_frame(&rx, Duration::from_secs(2)).expect("scrub frame");
        let latency = t0.elapsed();
        latencies.push(latency);
        let expected = expected_color(&fixture, t);
        assert_eq!(
            f.color, expected,
            "scrub to {t:.2} showed {} (expected {expected})",
            f.color
        );
        if expected != "black" {
            // Floor-frame accuracy: the frame under the playhead.
            assert!(
                f.pts <= t + 1e-6 && t - f.pts < FRAME + 1e-6,
                "scrub to {t:.3} landed on pts {:.3}",
                f.pts
            );
        }
        println!("scrub {t:5.2}s → {latency:?} ({})", f.color);
    }
    latencies.sort();
    let median = latencies[latencies.len() / 2];
    assert!(
        median < Duration::from_millis(100),
        "median scrub latency {median:?} ≥ 100 ms"
    );

    // Revisiting is a cache hit — must be far under budget.
    while rx.try_recv().is_ok() {}
    let t0 = Instant::now();
    player.seek(1.2);
    let f = recv_frame(&rx, Duration::from_secs(1)).expect("warm frame");
    let warm = t0.elapsed();
    println!("warm re-scrub → {warm:?}");
    assert_eq!(f.color, "green");
    assert!(warm < Duration::from_millis(50), "warm scrub took {warm:?}");
}

/// Phase 2 acceptance: editing transform/opacity while paused
/// re-composites the shown frame immediately (<100 ms median) — the
/// player gizmo's live-feedback path. A red overlay covers a green base;
/// shrinking it must reveal the green underneath, proving the frame was
/// re-rendered with the new transform rather than re-presented from
/// cache. Hide is exercised through the same path.
#[test]
fn paused_transform_edit_recomposites_live() {
    let _serial = serial();
    let red = color_source("red", 300);
    let green = color_source("green", 440);
    for src in [&red, &green] {
        generate_proxy(src, Some(10.0), |_| {}).expect("proxy");
    }

    let mut engine = Engine::new(ProjectSettings::default());
    let r = engine
        .add_media(red.display().to_string(), 10.0, true, true)
        .unwrap();
    let g = engine
        .add_media(green.display().to_string(), 10.0, true, true)
        .unwrap();
    let base_track = track_of(&engine, TrackKind::Video);
    let top_track = engine.add_track(TrackKind::Video, 0).unwrap();
    engine.add_clip(base_track, g, 0.0, 0.0, 3.0).unwrap();
    let top_clip = engine.add_clip(top_track, r, 0.0, 0.0, 3.0).unwrap();

    let (player, rx) = open_player(engine.project().clone());
    let first = recv_frame(&rx, Duration::from_secs(10)).expect("preview");
    assert_eq!(first.color, "red", "fullscreen top layer covers the base");

    // A gizmo drag: transient transform steps stream in while paused;
    // every snapshot must re-composite the paused frame.
    let mut latencies = Vec::new();
    let mut last_color = first.color;
    for scale in [0.8, 0.6, 0.4, 0.25, 0.15] {
        engine
            .set_clip_transform(
                top_clip,
                Transform {
                    x: 0.0,
                    y: 0.0,
                    scale,
                    rotation: 0.0,
                },
            )
            .unwrap();
        while rx.try_recv().is_ok() {} // drain stale frames
        let t0 = Instant::now();
        player.set_project(engine.project().clone());
        let f = recv_frame(&rx, Duration::from_secs(2)).expect("recomposited frame");
        let latency = t0.elapsed();
        latencies.push(latency);
        last_color = f.color;
        println!("scale {scale:.2} → {latency:?} ({})", f.color);
    }
    // At 0.15× the red overlay covers ~2% of the frame: green dominates.
    assert_eq!(
        last_color, "green",
        "the recomposite must reflect the new transform"
    );
    latencies.sort();
    let median = latencies[latencies.len() / 2];
    assert!(
        median < Duration::from_millis(100),
        "median edit→frame latency {median:?} ≥ 100 ms"
    );

    // Opacity through the same live path: fading the (restored) overlay
    // to zero leaves pure green.
    engine
        .set_clip_transform(top_clip, Transform::default())
        .unwrap();
    engine.set_clip_opacity(top_clip, 0.0).unwrap();
    while rx.try_recv().is_ok() {}
    player.set_project(engine.project().clone());
    let f = recv_frame(&rx, Duration::from_secs(2)).expect("opacity frame");
    assert_eq!(f.color, "green", "opacity 0 hides the top layer");

    // And hide: the hidden track drops out of the preview composite.
    engine.set_clip_opacity(top_clip, 1.0).unwrap();
    engine
        .set_track_flag(top_track, TrackFlag::Hidden, true)
        .unwrap();
    while rx.try_recv().is_ok() {}
    player.set_project(engine.project().clone());
    let f = recv_frame(&rx, Duration::from_secs(2)).expect("hidden-track frame");
    assert_eq!(f.color, "green", "hidden track is excluded from preview");
}

/// Pause mid-clip, then frame-step across a cut boundary: each step
/// shows the adjacent frame, crossing into the next clip's content.
#[test]
fn frame_stepping_crosses_cut_boundaries() {
    let _serial = serial();
    let fixture = build_fixture();
    let (player, rx) = open_player(fixture.project.clone());
    let _ = recv_frame(&rx, Duration::from_secs(10)).expect("preview");

    // Park two frames before the first cut (red → green at 0.8).
    let cut = fixture.segments[0].1;
    player.seek(cut - 2.0 * FRAME);
    let f = recv_frame(&rx, Duration::from_secs(2)).expect("park frame");
    assert_eq!(f.color, "red");

    // Step forward: still red (one frame left before the cut).
    player.step(1);
    let f = recv_frame(&rx, Duration::from_secs(2)).expect("step 1");
    assert_eq!(f.color, "red", "one frame before the cut is still red");

    // Step across the cut: green, at the incoming clip's first frame.
    player.step(1);
    let f = recv_frame(&rx, Duration::from_secs(2)).expect("step 2");
    assert_eq!(
        f.color, "green",
        "stepping across the cut shows the next clip"
    );
    assert!(
        (f.pts - cut).abs() < FRAME,
        "expected the incoming clip's first frame, got pts {:.3} (cut {cut:.3})",
        f.pts
    );

    // And back again.
    player.step(-1);
    let f = recv_frame(&rx, Duration::from_secs(2)).expect("step back");
    assert_eq!(f.color, "red", "stepping back re-crosses the cut");
    assert!(f.pts < cut);

    // Steps must be exact one-frame moves on the project grid.
    player.step(1);
    let a = recv_frame(&rx, Duration::from_secs(2)).expect("fwd again");
    player.step(1);
    let b = recv_frame(&rx, Duration::from_secs(2)).expect("fwd again 2");
    assert!(
        ((b.pts - a.pts) - FRAME).abs() < 0.004,
        "step moved {:.4}s, not one frame",
        b.pts - a.pts
    );
}

/// Playback through the gap: black + continued clock, then the next clip
/// starts on time (no stall waiting at the gap edge).
#[test]
fn gap_renders_black_and_playback_continues() {
    let _serial = serial();
    let fixture = build_fixture();
    let (player, rx) = open_player(fixture.project.clone());
    let _ = recv_frame(&rx, Duration::from_secs(10)).expect("preview");

    // Start just before the gap.
    player.seek(fixture.gap.0 - 0.3);
    let _ = recv_frame(&rx, Duration::from_secs(2)).expect("pre-gap frame");
    player.play();

    let mut saw_black = false;
    let mut post_gap: Option<FrameRec> = None;
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Evt::Frame(f)) => {
                if f.color == "black" {
                    saw_black = true;
                } else if f.pts >= fixture.gap.1 - 1e-6 {
                    post_gap = Some(f);
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    player.pause();
    assert!(saw_black, "gap must render black");
    let post = post_gap.expect("playback must continue past the gap");
    assert_eq!(post.color, "blue", "clip after the gap");
    // The first post-gap frame lands at the gap end, in sync with the
    // clock (no early entry, no stall).
    assert!(
        (post.pts - fixture.gap.1).abs() < 2.0 * FRAME,
        "post-gap frame at {:.3}, gap ends {:.3}",
        post.pts,
        fixture.gap.1
    );
    assert!(
        (post.pts - post.clock).abs() < 0.040,
        "post-gap frame off-clock by {:.0} ms",
        (post.pts - post.clock).abs() * 1e3
    );
}
