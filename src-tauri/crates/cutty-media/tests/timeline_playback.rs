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

use cutty_engine::{Engine, MediaId, Project, ProjectSettings, TrackId, TrackKind};
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
