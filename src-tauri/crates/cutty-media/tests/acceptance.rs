//! Timeline playback acceptance harness against real media (PLAN.md §5 +
//! the Phase 1 playback prompt): 10+ cuts across 3 real source files,
//! 60 s continuous playback, drift, scrub latency, stepping across cuts.
//!
//! Run explicitly (plays ~60 s in real time):
//! ```sh
//! cargo test -p cutty-media --test acceptance -- --ignored --nocapture
//! ```
//! Sources default to the dev machine's test set; override with
//! `CUTTY_4K_CLIP`, `CUTTY_SRC_B`, `CUTTY_SRC_C`.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use cutty_engine::{Engine, ProjectSettings, TrackKind};
use cutty_media::{generate_proxy, probe, PlayerEvent, TimelinePlayer};

fn source_paths() -> [PathBuf; 3] {
    let get =
        |var: &str, default: &str| std::env::var(var).unwrap_or_else(|_| default.into()).into();
    [
        get("CUTTY_4K_CLIP", "/home/love/Videos/cutty-test-4k30.mp4"),
        get(
            "CUTTY_SRC_B",
            "/home/love/Videos/cutty-test-media/beach-broll.mp4",
        ),
        get(
            "CUTTY_SRC_C",
            "/home/love/Videos/cutty-test-media/drone-pass.mkv",
        ),
    ]
}

struct FrameRec {
    pts: f64,
    clock: f64,
    arrived: Instant,
}

enum Evt {
    Frame(FrameRec),
    Eof,
    Error(String),
}

#[test]
#[ignore = "real-time 60s playback on real media; run with --ignored"]
fn timeline_playback_acceptance() {
    let sources = source_paths();
    for s in &sources {
        assert!(s.is_file(), "missing source {}", s.display());
    }

    // --- Probe + proxies (cached across runs) ---
    let mut media = Vec::new();
    for s in &sources {
        let info = probe(s).expect("probe");
        let t0 = Instant::now();
        generate_proxy(s, Some(info.duration_sec), |_| {}).expect("proxy");
        println!(
            "{}: {:.1}s source, proxy ready in {:.1?}",
            s.file_name().unwrap().to_string_lossy(),
            info.duration_sec,
            t0.elapsed()
        );
        media.push(info);
    }

    // --- Timeline: 15 cuts cycling the three sources over ~64 s ---
    let mut engine = Engine::new(ProjectSettings::default());
    let ids: Vec<_> = sources
        .iter()
        .zip(&media)
        .map(|(path, info)| {
            engine
                .add_media(
                    path.display().to_string(),
                    info.duration_sec,
                    info.video.is_some(),
                    info.audio.is_some(),
                )
                .unwrap()
        })
        .collect();
    let video = engine
        .project()
        .tracks
        .iter()
        .find(|t| t.kind == TrackKind::Video)
        .unwrap()
        .id;

    let mut t = 0.0;
    let mut cuts = 0;
    let mut i = 0usize;
    while t < 64.0 {
        let media_idx = i % 3;
        let dur = 4.0_f64.min(media[media_idx].duration_sec - 1.0);
        // Vary the in-point so repeated visits to a source aren't
        // contiguous (forces real re-seeks, not continuations).
        let source_in = (i as f64 * 1.7) % (media[media_idx].duration_sec - dur - 0.5).max(0.5);
        engine
            .add_clip(video, ids[media_idx], t, source_in, source_in + dur)
            .unwrap();
        t += dur;
        cuts += 1;
        i += 1;
    }
    println!("timeline: {cuts} segments, {t:.1}s total");
    assert!(cuts >= 10, "need 10+ cuts for acceptance");

    // --- Open and play through 60 s ---
    let (tx, rx) = mpsc::channel();
    let player = TimelinePlayer::open(
        engine.project().clone(),
        Box::new(move |e| {
            let evt = match e {
                PlayerEvent::Frame {
                    pts_sec, clock_sec, ..
                } => Evt::Frame(FrameRec {
                    pts: pts_sec,
                    clock: clock_sec,
                    arrived: Instant::now(),
                }),
                PlayerEvent::Eof => Evt::Eof,
                PlayerEvent::Error(e) => Evt::Error(e),
                PlayerEvent::Position { .. } => return,
            };
            let _ = tx.send(evt);
        }),
    )
    .expect("player opens");

    let first = rx.recv_timeout(Duration::from_secs(10));
    assert!(
        matches!(first, Ok(Evt::Frame(_))),
        "must show a preview frame on open"
    );

    println!("playing 60s across {cuts} cuts…");
    player.play();
    let start = Instant::now();
    let mut frames: Vec<FrameRec> = Vec::new();
    while start.elapsed() < Duration::from_secs(61) {
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Evt::Frame(f)) => frames.push(f),
            Ok(Evt::Eof) => break,
            Ok(Evt::Error(e)) if e.contains("audio unavailable") => {}
            Ok(Evt::Error(e)) => panic!("player error: {e}"),
            Err(_) => panic!("no frames for 2s during playback"),
        }
    }
    player.pause();

    let steady = &frames[10.min(frames.len())..];
    assert!(steady.len() > 1500, "collected only {}", steady.len());

    let span = steady
        .last()
        .unwrap()
        .arrived
        .duration_since(steady.first().unwrap().arrived)
        .as_secs_f64();
    let eff_fps = (steady.len() - 1) as f64 / span;

    let max_drift = steady
        .iter()
        .map(|f| (f.pts - f.clock).abs())
        .fold(0.0, f64::max);

    // Hitch detector: worst wall-clock gap between consecutive frames.
    let frame_dur = 1.0 / 30.0;
    let mut worst_gap = Duration::ZERO;
    let mut hitches = 0;
    for pair in steady.windows(2) {
        let dt = pair[1].arrived.duration_since(pair[0].arrived);
        worst_gap = worst_gap.max(dt);
        if dt.as_secs_f64() > 2.5 * frame_dur {
            hitches += 1;
            println!(
                "  hitch {dt:?} between pts {:.3} and {:.3}",
                pair[0].pts, pair[1].pts
            );
        }
    }

    println!(
        "playback: {} frames in {span:.1}s → {eff_fps:.2} fps | drift max {:.1} ms | worst gap {worst_gap:?} | {hitches} hitches",
        steady.len(),
        max_drift * 1e3,
    );
    assert!(eff_fps >= 29.5, "fps {eff_fps:.2} < 29.5");
    assert!(max_drift < 0.040, "drift {:.1} ms", max_drift * 1e3);
    assert_eq!(hitches, 0, "visible hitches during cut playback");

    // --- Scrub latency (cold + warm) across all sources ---
    while rx.try_recv().is_ok() {}
    let seek_check = |target: f64, label: &str, bound_ms: u64| {
        let t0 = Instant::now();
        player.seek(target);
        let deadline = Instant::now() + Duration::from_secs(2);
        // Frames already in flight when the seek was issued (e.g. the
        // pause snap) can still arrive first — wait for the one that
        // answers *this* seek.
        let frame = loop {
            match rx.recv_timeout(deadline - Instant::now()) {
                Ok(Evt::Frame(f)) if f.pts <= target + 1e-6 && target - f.pts < 2.0 * frame_dur => {
                    break f;
                }
                Ok(_) => continue,
                Err(_) => panic!("no frame at {target} after seek"),
            }
        };
        let latency = t0.elapsed();
        println!(
            "seek {label} → {target:5.1}s: {latency:8.1?} (pts {:.3})",
            frame.pts
        );
        assert!(
            latency < Duration::from_millis(bound_ms),
            "{label} seek took {latency:?} (≥{bound_ms} ms)"
        );
    };
    seek_check(30.2, "cold", 100);
    seek_check(9.7, "cold", 100);
    seek_check(51.3, "cold", 100);
    seek_check(17.44, "cold", 100);
    seek_check(30.2, "warm", 50);

    // --- Frame stepping across a cut boundary ---
    while rx.try_recv().is_ok() {}
    let cut = 4.0; // first cut (all segments are 4s here)
    player.seek(cut - frame_dur);
    let _ = rx.recv_timeout(Duration::from_secs(2));
    for (dir, expect_side) in [(1i64, "after"), (-1, "before")] {
        player.step(dir);
        let deadline = Instant::now() + Duration::from_secs(2);
        let frame = loop {
            match rx.recv_timeout(deadline - Instant::now()) {
                Ok(Evt::Frame(f)) => break f,
                Ok(_) => continue,
                Err(_) => panic!("no frame after step {dir}"),
            }
        };
        match expect_side {
            "after" => assert!(
                frame.pts >= cut - 1e-6,
                "step +1 across cut landed at {:.3}",
                frame.pts
            ),
            _ => assert!(
                frame.pts < cut,
                "step -1 back across cut landed at {:.3}",
                frame.pts
            ),
        }
        println!("step {dir:+} across cut → pts {:.3}", frame.pts);
    }
}
