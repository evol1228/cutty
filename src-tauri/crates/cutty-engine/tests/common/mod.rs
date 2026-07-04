//! Shared fixtures for the engine integration tests.
#![allow(dead_code)]

use cutty_engine::{ClipId, Engine, MediaId, ProjectSettings, TrackId, TrackKind};

/// Duration of the fixture media file, seconds.
pub const MEDIA_DUR: f64 = 10.0;

pub struct Fixture {
    pub engine: Engine,
    pub media: MediaId,
    pub video: TrackId,
    pub audio: TrackId,
}

/// Fresh engine with default settings and one 10s A/V media file
/// registered.
pub fn engine_with_media() -> Fixture {
    let mut engine = Engine::new(ProjectSettings::default());
    let media = engine
        .add_media("/tmp/fixture.mp4", MEDIA_DUR, true, true)
        .expect("add media");
    let video = track_of_kind(&engine, TrackKind::Video);
    let audio = track_of_kind(&engine, TrackKind::Audio);
    engine.drain_events();
    Fixture {
        engine,
        media,
        video,
        audio,
    }
}

pub fn track_of_kind(engine: &Engine, kind: TrackKind) -> TrackId {
    engine
        .project()
        .tracks
        .iter()
        .find(|t| t.kind == kind)
        .map(|t| t.id)
        .expect("track of kind")
}

/// Canonical serialization of the current project, for state-identity
/// assertions.
pub fn serialized(engine: &Engine) -> String {
    serde_json::to_string(engine.project()).expect("serialize project")
}

/// (timeline_in, timeline_out, source_in, source_out) of a clip.
pub fn span(engine: &Engine, id: ClipId) -> (f64, f64, f64, f64) {
    let (_, clip) = engine.project().find_clip(id).expect("clip exists");
    (
        clip.timeline_in,
        clip.timeline_out,
        clip.source_in,
        clip.source_out,
    )
}
