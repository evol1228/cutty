//! Versioned `.cutty` project files: serialization, migration, and
//! media-path relativization.
//!
//! ## Format
//!
//! A `.cutty` file is pretty-printed JSON with a top-level `version`
//! integer. Loading parses the version first, then dispatches to the
//! matching schema arm in [`migrate`] — the migration scaffold. **If the
//! model's serde shape ever changes, bump [`CURRENT_VERSION`] and add a
//! migration arm**; the golden-fixture test in this module fails when the
//! v1 shape drifts silently.
//!
//! ## Media paths
//!
//! Each media entry stores its absolute path plus, when computable, a path
//! relative to the project file's directory. On load the relative path
//! wins when it points at an existing file — so a project folder moved or
//! renamed together with its media keeps working — and the absolute path
//! is the fallback. When neither exists the absolute path is kept so the
//! UI can mark the media missing (the project still opens).

use std::io::Write;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::EngineError;
use crate::model::{MediaId, MediaRef, Project, ProjectSettings, Track};

/// The newest project-file schema version this build reads and writes.
pub const CURRENT_VERSION: u32 = 1;

/// Errors from saving or loading `.cutty` files.
#[derive(Debug, thiserror::Error)]
pub enum ProjectFileError {
    #[error("could not read project file: {0}")]
    Read(#[source] std::io::Error),
    #[error("could not write project file: {0}")]
    Write(#[source] std::io::Error),
    #[error("not a valid .cutty project file: {0}")]
    Parse(#[source] serde_json::Error),
    #[error("not a valid .cutty project file: missing `version` field")]
    MissingVersion,
    #[error(
        "project file version {found} is newer than this build of Cutty \
         supports (up to {max}) — please update Cutty"
    )]
    UnsupportedVersion { found: u64, max: u32 },
    #[error("project file contains an invalid project: {0}")]
    Invalid(#[from] EngineError),
}

/// On-disk form of a [`MediaRef`]: the absolute path as registered, plus
/// an optional path relative to the project file's directory.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MediaEntryV1 {
    id: MediaId,
    path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    relative_path: Option<String>,
    duration: f64,
    has_video: bool,
    has_audio: bool,
}

/// Schema v1. Tracks and clips reuse the model's serde shape directly;
/// the golden-fixture test pins that shape.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectFileV1 {
    version: u32,
    settings: ProjectSettings,
    media: Vec<MediaEntryV1>,
    tracks: Vec<Track>,
}

/// Migration scaffold: one arm per historical schema version, each lifting
/// the raw JSON one step toward [`CURRENT_VERSION`]. v1 is the only arm
/// today; when v2 lands, the `1` arm becomes a `v1_to_v2` transform and
/// every old file keeps loading.
fn migrate(value: Value, version: u64) -> Result<ProjectFileV1, ProjectFileError> {
    match version {
        1 => serde_json::from_value(value).map_err(ProjectFileError::Parse),
        v => Err(ProjectFileError::UnsupportedVersion {
            found: v,
            max: CURRENT_VERSION,
        }),
    }
}

/// Serialize a project to `.cutty` JSON. `project_dir` is the directory
/// the file will live in — media paths are additionally stored relative to
/// it when computable. Pass `None` for location-independent output
/// (autosaves): absolute paths only.
///
/// Output is deterministic: the same project and directory always produce
/// identical bytes (save → load → save round-trips byte-for-byte).
pub fn serialize(project: &Project, project_dir: Option<&Path>) -> String {
    let media = project
        .media
        .iter()
        .map(|m| MediaEntryV1 {
            id: m.id,
            path: m.path.clone(),
            relative_path: project_dir
                .and_then(|dir| relative_to(Path::new(&m.path), dir))
                .map(|p| p.to_string_lossy().into_owned()),
            duration: m.duration,
            has_video: m.has_video,
            has_audio: m.has_audio,
        })
        .collect();
    let file = ProjectFileV1 {
        version: CURRENT_VERSION,
        settings: project.settings.clone(),
        media,
        tracks: project.tracks.clone(),
    };
    let mut json = serde_json::to_string_pretty(&file).expect("project model serializes");
    json.push('\n');
    json
}

/// Parse `.cutty` JSON (any supported version) into a validated
/// [`Project`]. `project_dir` is the directory the file was read from,
/// used to resolve relative media paths; see the module docs for the
/// resolution order.
pub fn deserialize(json: &str, project_dir: Option<&Path>) -> Result<Project, ProjectFileError> {
    let value: Value = serde_json::from_str(json).map_err(ProjectFileError::Parse)?;
    let version = value
        .get("version")
        .and_then(Value::as_u64)
        .ok_or(ProjectFileError::MissingVersion)?;
    let file = migrate(value, version)?;

    let media = file
        .media
        .into_iter()
        .map(|entry| {
            let path = resolve_media_path(&entry.path, entry.relative_path.as_deref(), project_dir);
            MediaRef {
                id: entry.id,
                path,
                duration: entry.duration,
                has_video: entry.has_video,
                has_audio: entry.has_audio,
            }
        })
        .collect();
    let project = Project {
        settings: file.settings,
        media,
        tracks: file.tracks,
    };
    project.validate()?;
    Ok(project)
}

/// Save a project to `path` (atomic: temp file + rename, fsynced so a
/// crash never leaves a truncated file). Media paths are relativized
/// against the file's directory.
pub fn save(project: &Project, path: &Path) -> Result<(), ProjectFileError> {
    let json = serialize(project, parent_dir(path));
    atomic_write(path, json.as_bytes()).map_err(ProjectFileError::Write)
}

/// Load a project from `path`, resolving relative media paths against the
/// file's directory.
pub fn load(path: &Path) -> Result<Project, ProjectFileError> {
    let json = std::fs::read_to_string(path).map_err(ProjectFileError::Read)?;
    deserialize(&json, parent_dir(path))
}

/// `path.parent()`, treating the empty parent of bare filenames as absent.
fn parent_dir(path: &Path) -> Option<&Path> {
    path.parent().filter(|p| !p.as_os_str().is_empty())
}

/// Pick the on-disk location for a media entry: relative resolution first
/// (project moved with its media), then the stored absolute path. Both
/// missing → keep the absolute path for the missing-media UI.
fn resolve_media_path(
    absolute: &str,
    relative: Option<&str>,
    project_dir: Option<&Path>,
) -> String {
    if let (Some(rel), Some(dir)) = (relative, project_dir) {
        let joined = normalize_lexical(&dir.join(rel));
        if joined.is_file() {
            return joined.to_string_lossy().into_owned();
        }
    }
    absolute.to_string()
}

/// Express `target` relative to the directory `base`. Both must be
/// absolute; returns `None` otherwise. Purely lexical (no filesystem
/// access) — a brittle `../..`-heavy result is fine because loading falls
/// back to the absolute path.
fn relative_to(target: &Path, base: &Path) -> Option<PathBuf> {
    if !target.is_absolute() || !base.is_absolute() {
        return None;
    }
    let target: Vec<Component> = target.components().collect();
    let base: Vec<Component> = base.components().collect();
    let common = target
        .iter()
        .zip(base.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut out = PathBuf::new();
    for _ in common..base.len() {
        out.push("..");
    }
    for component in &target[common..] {
        out.push(component.as_os_str());
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    Some(out)
}

/// Resolve `.` and `..` components lexically (no symlink traversal, no
/// filesystem access). `/a/b/../c` → `/a/c`.
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                // `/..` == `/` per POSIX.
                Some(Component::RootDir) => {}
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                // Empty or already `..`-leading relative path: keep
                // accumulating `..`s it can't consume.
                _ => out.push(".."),
            },
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Write `bytes` to `path` atomically: temp file in the same directory,
/// fsync, rename over the target. Readers never observe a partial file
/// and a crash mid-write leaves the previous version intact.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = parent_dir(path).unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".to_string());
    let tmp = dir.join(format!(".{}.{}.tmp", file_name, std::process::id()));
    let result = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_to_computes_sibling_and_parent_paths() {
        let rel = |t: &str, b: &str| relative_to(Path::new(t), Path::new(b));
        assert_eq!(rel("/a/b/c.mp4", "/a/b"), Some(PathBuf::from("c.mp4")));
        assert_eq!(
            rel("/a/media/c.mp4", "/a/projects"),
            Some(PathBuf::from("../media/c.mp4"))
        );
        assert_eq!(
            rel("/x/c.mp4", "/a/b"),
            Some(PathBuf::from("../../x/c.mp4"))
        );
        assert_eq!(rel("relative.mp4", "/a"), None);
        assert_eq!(rel("/a/b", "/a/b"), Some(PathBuf::from(".")));
    }

    #[test]
    fn normalize_resolves_dots_lexically() {
        assert_eq!(
            normalize_lexical(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
        assert_eq!(normalize_lexical(Path::new("/..")), PathBuf::from("/"));
        assert_eq!(normalize_lexical(Path::new("../x")), PathBuf::from("../x"));
    }

    #[test]
    fn unsupported_and_missing_versions_are_rejected() {
        let missing = deserialize(r#"{"settings":{}}"#, None);
        assert!(matches!(missing, Err(ProjectFileError::MissingVersion)));

        let future = deserialize(r#"{"version": 999, "settings": {}}"#, None);
        match future {
            Err(ProjectFileError::UnsupportedVersion { found, max }) => {
                assert_eq!(found, 999);
                assert_eq!(max, CURRENT_VERSION);
            }
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    /// Golden v1 fixture: this exact JSON must keep loading forever. If
    /// this test fails, the model's serde shape changed — bump
    /// [`CURRENT_VERSION`] and add a migration arm instead of editing the
    /// fixture.
    #[test]
    fn golden_v1_fixture_loads() {
        let fixture = r#"{
          "version": 1,
          "settings": { "width": 1920, "height": 1080, "fps": 30.0 },
          "media": [
            {
              "id": 3,
              "path": "/media/clip.mp4",
              "relativePath": "../media/clip.mp4",
              "duration": 10.0,
              "hasVideo": true,
              "hasAudio": true
            }
          ],
          "tracks": [
            {
              "id": 1,
              "kind": "video",
              "name": "V1",
              "locked": false,
              "muted": false,
              "clips": [
                {
                  "id": 4,
                  "mediaId": 3,
                  "timelineIn": 0.0,
                  "timelineOut": 5.0,
                  "sourceIn": 2.0,
                  "sourceOut": 7.0,
                  "transform": { "x": 0.0, "y": 0.0, "scale": 1.0, "rotation": 0.0 },
                  "opacity": 1.0,
                  "speed": 1.0,
                  "volume": 1.0
                }
              ]
            },
            {
              "id": 2,
              "kind": "audio",
              "name": "A1",
              "locked": false,
              "muted": false,
              "clips": []
            }
          ]
        }"#;
        let project = deserialize(fixture, Some(Path::new("/projects"))).expect("fixture loads");
        assert_eq!(project.media.len(), 1);
        // Neither path exists on this machine → absolute is kept.
        assert_eq!(project.media[0].path, "/media/clip.mp4");
        assert_eq!(project.tracks[0].clips.len(), 1);
        assert_eq!(project.tracks[0].clips[0].source_in, 2.0);
    }
}
