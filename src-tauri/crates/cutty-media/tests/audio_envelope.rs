//! Acceptance: volume automation drawn in the editor exports with the
//! exact preview envelope. The full chain runs for real — engine fade
//! handles → volume keyframes → `export_audio_timeline` → the offline
//! mixer decoding an actual WAV through symphonia — and the exported
//! samples are compared against the analytic fade curve, the same way
//! the Phase 1 acceptance compared RMS envelopes.

use std::io::Write;
use std::path::{Path, PathBuf};

use cutty_audio::{render_timeline_to_wav, EXPORT_SAMPLE_RATE};
use cutty_engine::{Engine, FadeSide, ProjectSettings, TrackKind};
use cutty_media::export_audio_timeline;

/// Write a stereo IEEE-float WAV of `secs` seconds holding `value` in
/// both channels — a decodable constant source, so every exported
/// sample directly reveals the gain applied to it.
fn write_const_wav(path: &Path, secs: f64, value: f32) {
    let frames = (secs * f64::from(EXPORT_SAMPLE_RATE)) as u32;
    let data_bytes = frames * 8;
    let mut out = Vec::with_capacity(58 + data_bytes as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(50 + data_bytes).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&18u32.to_le_bytes());
    out.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
    out.extend_from_slice(&2u16.to_le_bytes()); // stereo
    out.extend_from_slice(&EXPORT_SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&(EXPORT_SAMPLE_RATE * 8).to_le_bytes());
    out.extend_from_slice(&8u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(b"fact");
    out.extend_from_slice(&4u32.to_le_bytes());
    out.extend_from_slice(&frames.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_bytes.to_le_bytes());
    for _ in 0..frames * 2 {
        out.extend_from_slice(&value.to_le_bytes());
    }
    let mut f = std::fs::File::create(path).unwrap();
    f.write_all(&out).unwrap();
}

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("cutty-media-tests");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// Interleaved stereo f32 samples of a WAV this suite wrote.
fn read_wav_f32(path: &Path) -> Vec<f32> {
    let bytes = std::fs::read(path).unwrap();
    let data_off = 58; // RIFF(12) + fmt(8+18) + fact(8+4) + data hdr(8)
    assert_eq!(&bytes[data_off - 8..data_off - 4], b"data");
    bytes[data_off..]
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

#[test]
fn fades_drawn_with_handles_export_the_exact_envelope() {
    let src = tmp("envelope-source.wav");
    write_const_wav(&src, 3.0, 0.5);

    // The editing session: a 2 s music clip at 80% volume with the fade
    // handles dragged to 0.5 s in / 0.75 s out.
    let mut engine = Engine::new(ProjectSettings::default());
    let media = engine
        .add_media(src.to_string_lossy(), 3.0, false, true)
        .unwrap();
    let audio_track = engine
        .project()
        .tracks
        .iter()
        .find(|t| t.kind == TrackKind::Audio)
        .unwrap()
        .id;
    let clip = engine.add_clip(audio_track, media, 0.0, 0.5, 2.5).unwrap();
    engine.set_clip_volume(clip, 0.8).unwrap();
    assert_eq!(engine.set_clip_fade(clip, FadeSide::In, 0.5).unwrap(), 0.5);
    assert_eq!(
        engine.set_clip_fade(clip, FadeSide::Out, 0.75).unwrap(),
        0.75
    );

    // The export path: the same timeline builder `run_export` uses.
    let timeline = export_audio_timeline(engine.project()).unwrap();
    let dst = tmp("envelope-export.wav");
    let total = 2 * u64::from(EXPORT_SAMPLE_RATE);
    render_timeline_to_wav(
        timeline,
        EXPORT_SAMPLE_RATE,
        total,
        &dst,
        &|| false,
        &mut |_, _| {},
    )
    .unwrap();

    let samples = read_wav_f32(&dst);
    assert_eq!(samples.len() as u64, total * 2);
    let rate = f64::from(EXPORT_SAMPLE_RATE);
    let mut worst = 0.0f32;
    for k in 0..total as usize {
        let t = k as f64 / rate;
        let ramp = if t < 0.5 {
            t / 0.5
        } else if t < 1.25 {
            1.0
        } else {
            (2.0 - t) / 0.75
        };
        let expected = (0.5 * 0.8 * ramp) as f32; // source × volume × fade
        let diff = (samples[k * 2] - expected).abs();
        worst = worst.max(diff);
        assert!(
            diff < 1e-5,
            "sample {k} (t={t:.4}): {} vs {expected}",
            samples[k * 2]
        );
        assert_eq!(samples[k * 2], samples[k * 2 + 1], "stereo symmetry");
    }
    println!("envelope export parity: worst per-sample deviation {worst:.2e}");
}
