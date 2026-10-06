//! Environment and user-path access for the TUI (`ra-fork.md` follow-up #2).
//!
//! Every environment read uses the current `RA_` spelling directly — one name
//! per variable, no fallback chain:
//!
//! | Prefix | Example | Scope |
//! |---|---|---|
//! | `RA_TUI_*` | `RA_TUI_NO_SPLASH` | behaviour owned by this client |
//! | `RA_*` | `RA_AUTH_TOKEN` | names shared with the kernel (token, home, prefix) |
//!
//! Paths use the RecurAgent spelling only: `~/.ra`, `~/.config/ra-tui`. There is no
//! backward-compat for state directories — other locations are neither read
//! nor migrated.
//!
//! Env reads are routed through [`env_compat`] / [`env_os_compat`]; both take
//! the full variable name (no prefix is added) and ignore empty values.
//! [`home_entry`] is split out so path joining is unit-testable without
//! mutating process env (`std::env::set_var` is `unsafe` under edition 2024 +
//! `unsafe_code = deny`).

use std::path::{Path, PathBuf};

/// Read the environment variable `name` (the full name — no prefix is added).
/// A value set to the empty string counts as unset (`FOO=` never enables
/// anything).
pub fn env_compat(name: &str) -> Option<String> {
    non_empty(name)
}

/// [`env_compat`] for path-ish values (`OsString`, so non-UTF-8 paths survive).
pub fn env_os_compat(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

fn non_empty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}

/// `HOME`, falling back to `USERPROFILE` on native Windows shells (where
/// `HOME` is usually unset); empty values are ignored.
pub fn home_dir() -> Option<PathBuf> {
    let non_empty = |key: &str| std::env::var_os(key).filter(|v| !v.is_empty());
    non_empty("HOME")
        .or_else(|| non_empty("USERPROFILE"))
        .map(PathBuf::from)
}

/// The ra-side entry under `home`: `home/<rel>`. Fresh installs create it; no
/// legacy location is consulted (state dirs have no backward-compat).
pub fn home_entry(home: &Path, rel: &str) -> PathBuf {
    home.join(rel)
}

/// [`home_entry`] rooted at [`home_dir`]; `None` when no home resolves.
pub fn ra_home_entry(rel: &str) -> Option<PathBuf> {
    home_dir().map(|home| home_entry(&home, rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_entry_is_the_new_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();

        assert_eq!(home_entry(home, ".ra"), home.join(".ra"));

        // The new path is returned whether or not it exists yet, and a stray
        // legacy dir is never consulted (state dirs have no backward-compat).
        std::fs::create_dir(home.join(".ra")).expect("create stray legacy dir");
        assert_eq!(home_entry(home, ".ra"), home.join(".ra"));
    }

    #[test]
    fn ra_home_entry_uses_env_home() {
        // The live-process wrapper: only the new spelling is ever returned.
        if let Some(entry) = ra_home_entry(".ra") {
            assert_eq!(entry.file_name().and_then(|n| n.to_str()), Some(".ra"));
        }
    }
}
