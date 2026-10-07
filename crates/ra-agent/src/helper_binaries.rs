//! Locate the bundled helper binaries that ship alongside `ra`
//! (`weather`, `news_fetch`, `ra-sandbox`, …).
//!
//! Release bundles ship them FLAT, directly beside the `ra` executable, and
//! that remains the first location probed. An install that keeps the CLI at the
//! top of a directory and the helpers in a nested `tools/` folder is equally
//! valid, so `<exe_dir>/tools/` is probed second.
//!
//! ## Confinement note
//!
//! The probe is anchored to the RUNNING EXECUTABLE's directory only — never to
//! the working directory, a request context, or an environment variable.
//! Keeping environment and config inputs out is deliberate: they would let a
//! caller who controls the process environment *choose which helper binary
//! gets executed*, widening the trust boundary that the sandbox helper and the
//! bootstrapped skill binaries sit behind. An actor that can plant
//! `<exe_dir>/tools/weather` could already plant `<exe_dir>/weather`, so adding
//! the subdirectory does not widen who may supply a helper — it only accepts a
//! second, equally-trusted layout of the same install directory.

use std::path::{Path, PathBuf};

/// Subdirectory probed next to the running executable, *after* the
/// executable's own directory, for bundled helper binaries.
///
/// `ra` installed as `<dir>/ra` with its helpers in `<dir>/tools/` is the
/// layout this exists for.
pub const TOOLS_SUBDIR: &str = "tools";

/// Directories probed for a helper binary, in priority order.
///
/// Root first so a standard release bundle (flat layout) always wins over a
/// stale nested copy left behind by an earlier install.
fn search_dirs(exe_dir: &Path) -> [PathBuf; 2] {
    [exe_dir.to_path_buf(), exe_dir.join(TOOLS_SUBDIR)]
}

/// Find the bundled helper binary named `binary_name` relative to `exe_dir`.
///
/// Probes, in order:
///   1. `<exe_dir>/<binary_name>`         — the release-bundle layout
///   2. `<exe_dir>/tools/<binary_name>`   — CLI at the top, helpers nested
///
/// On Windows `binary_name.exe` is accepted at both locations, because release
/// bundles ship `weather.exe`, `news_fetch.exe`, … — a bare-name-only lookup
/// would falsely report every helper missing there (and so never bootstrap the
/// skill, and warn about a bare-binary deploy on a perfectly good install).
///
/// Only regular files are accepted. A *directory* named `weather` next to the
/// executable is not a helper; treating it as one would both silence the
/// missing-binary preflight and make the copy fail at the last step.
pub fn find_helper_binary(exe_dir: &Path, binary_name: &str) -> Option<PathBuf> {
    for dir in search_dirs(exe_dir) {
        let bare = dir.join(binary_name);
        if bare.is_file() {
            return Some(bare);
        }
        #[cfg(windows)]
        {
            let exe = dir.join(format!("{binary_name}.exe"));
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exe_dir_with_nested_tools(tmp: &tempfile::TempDir) -> PathBuf {
        let exe_dir = tmp.path().join("ra");
        std::fs::create_dir_all(exe_dir.join(TOOLS_SUBDIR)).unwrap();
        exe_dir
    }

    #[test]
    fn finds_a_flat_helper_beside_the_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let exe_dir = exe_dir_with_nested_tools(&tmp);
        std::fs::write(exe_dir.join("weather"), b"flat").unwrap();

        let found = find_helper_binary(&exe_dir, "weather").expect("flat helper must be found");
        assert_eq!(std::fs::read(&found).unwrap(), b"flat");
    }

    #[test]
    fn finds_a_helper_in_the_nested_tools_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let exe_dir = exe_dir_with_nested_tools(&tmp);
        std::fs::write(exe_dir.join(TOOLS_SUBDIR).join("weather"), b"nested").unwrap();

        let found = find_helper_binary(&exe_dir, "weather").expect("nested helper must be found");
        assert_eq!(
            std::fs::read(&found).unwrap(),
            b"nested",
            "the nested copy must be the one resolved when it is the only one"
        );
    }

    #[test]
    fn flat_copy_wins_over_the_nested_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let exe_dir = exe_dir_with_nested_tools(&tmp);
        std::fs::write(exe_dir.join("weather"), b"flat").unwrap();
        std::fs::write(exe_dir.join(TOOLS_SUBDIR).join("weather"), b"nested").unwrap();

        let found = find_helper_binary(&exe_dir, "weather").unwrap();
        assert_eq!(
            std::fs::read(&found).unwrap(),
            b"flat",
            "the release-bundle (flat) layout must keep priority"
        );
    }

    #[test]
    fn absent_helper_is_reported_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let exe_dir = exe_dir_with_nested_tools(&tmp);
        assert!(find_helper_binary(&exe_dir, "weather").is_none());
    }

    #[test]
    fn a_directory_of_the_same_name_is_not_a_helper() {
        let tmp = tempfile::tempdir().unwrap();
        let exe_dir = exe_dir_with_nested_tools(&tmp);
        // A directory named `weather` must not shadow the real helper in
        // `tools/`, and must not be returned as a binary itself.
        std::fs::create_dir_all(exe_dir.join("weather")).unwrap();
        std::fs::write(exe_dir.join(TOOLS_SUBDIR).join("weather"), b"nested").unwrap();

        let found = find_helper_binary(&exe_dir, "weather").expect("real helper must be found");
        assert_eq!(std::fs::read(&found).unwrap(), b"nested");
    }

    #[cfg(windows)]
    #[test]
    fn finds_a_dot_exe_helper_in_the_nested_tools_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let exe_dir = exe_dir_with_nested_tools(&tmp);
        std::fs::write(
            exe_dir.join(TOOLS_SUBDIR).join("weather.exe"),
            b"nested-exe",
        )
        .unwrap();

        let found = find_helper_binary(&exe_dir, "weather").expect("nested .exe must be found");
        assert_eq!(std::fs::read(&found).unwrap(), b"nested-exe");
    }
}
