//! Source-file presence checks for missing-media detection.

use std::path::Path;

/// For each path, whether it currently exists as a regular file.
///
/// Order matches the input. Non-file paths (directories, dangling
/// symlinks, permission errors) count as missing — a clip can't decode
/// from any of those.
pub fn paths_exist<S: AsRef<str>>(paths: &[S]) -> Vec<bool> {
    paths
        .iter()
        .map(|p| Path::new(p.as_ref()).is_file())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_files_dirs_and_missing() {
        let dir = std::env::temp_dir().join("cutty-media-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("exists.txt");
        std::fs::write(&file, "x").unwrap();

        let paths = [
            file.display().to_string(),
            dir.display().to_string(),
            dir.join("nope.mp4").display().to_string(),
        ];
        assert_eq!(paths_exist(&paths), vec![true, false, false]);
        assert_eq!(paths_exist::<String>(&[]), Vec::<bool>::new());
    }
}
