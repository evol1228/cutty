//! Content-keyed cache paths shared by proxy and thumbnail generation.
//!
//! Entries are keyed on source path + size + mtime, so an edited or
//! replaced source file automatically gets fresh derived artifacts.

use std::path::{Path, PathBuf};

use crate::error::MediaError;

/// A cache subdirectory under `$XDG_CACHE_HOME/cutty/` (e.g. `proxies`,
/// `thumbs`).
pub(crate) fn cache_dir(kind: &str) -> Result<PathBuf, MediaError> {
    let root = dirs::cache_dir().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "could not determine the XDG cache directory",
        )
    })?;
    Ok(root.join("cutty").join(kind))
}

/// Deterministic cache file name for a source file's identity.
pub(crate) fn cache_filename(path: &str, size: u64, mtime_nanos: u128, ext: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(path.as_bytes());
    hasher.update(&size.to_le_bytes());
    hasher.update(&mtime_nanos.to_le_bytes());
    let hash = hasher.finalize().to_hex();
    format!("{}.{ext}", &hash.as_str()[..32])
}

/// Where the cached artifact of `kind` for `src` lives (or will live).
///
/// Returns `(final_path, exists)`.
pub(crate) fn cache_entry_for(
    src: &Path,
    kind: &str,
    ext: &str,
) -> Result<(PathBuf, bool), MediaError> {
    let meta = std::fs::metadata(src)?;
    let mtime_nanos = meta
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let file_name = cache_filename(&src.display().to_string(), meta.len(), mtime_nanos, ext);
    let path = cache_dir(kind)?.join(file_name);
    let exists = path.is_file();
    Ok((path, exists))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_is_deterministic_and_keyed_on_identity() {
        let a = cache_filename("/videos/a.mp4", 1000, 42, "jpg");
        let b = cache_filename("/videos/a.mp4", 1000, 42, "jpg");
        assert_eq!(a, b);
        assert!(a.ends_with(".jpg"));
        assert_eq!(a.len(), 32 + 4);

        // Any identity component change yields a different entry.
        assert_ne!(a, cache_filename("/videos/b.mp4", 1000, 42, "jpg"));
        assert_ne!(a, cache_filename("/videos/a.mp4", 1001, 42, "jpg"));
        assert_ne!(a, cache_filename("/videos/a.mp4", 1000, 43, "jpg"));
    }
}
