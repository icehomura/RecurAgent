//! ra/legacy environment + user-path compatibility (`ra-fork.md` follow-up #2).
//!
//! Every environment read prefers the new RecurAgent spelling and falls back to the
//! pre-rename one:
//!
//! | New | Legacy | Scope |
//! |---|---|---|
//! | `RA_TUI_*` | `RA_TUI_*` | behaviour owned by this client |
//! | `RA_*` | `RA_*` | names shared with the kernel (token, home, prefix) |
//!
//! Paths use the new RecurAgent spelling only: `~/.ra`, `~/.config/ra-tui`. There is no
//! backward-compat for state directories — legacy locations are neither read
//! nor migrated. The env-var fallback above is the only compatibility kept.
//!
//! Env reads are routed through [`env_compat`] / [`env_os_compat`]; the pure
//! cores ([`pick_compat`], [`home_entry`]) are split out so the fallback order
//! is unit-testable without mutating process env (`std::env::set_var` is
//! `unsafe` under edition 2024 + `unsafe_code = deny`).

use std::path::{Path, PathBuf};

/// Read `new`, falling back to `legacy`. The first variable set to a
/// **non-empty** value wins; `FOO=` counts as unset (matching the pre-rename
/// opt-out semantics — an empty opt-out never disabled anything).
pub fn env_compat(new: &str, legacy: &str) -> Option<String> {
    pick_compat(non_empty(new), non_empty(legacy))
}

/// [`env_compat`] for path-ish values (`OsString`, so non-UTF-8 paths survive).
pub fn env_os_compat(new: &str, legacy: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(new)
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var_os(legacy).filter(|v| !v.is_empty()))
}

fn non_empty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Pure fallback core of [`env_compat`]: new wins, legacy still works, neither
/// set is `None`.
fn pick_compat(new: Option<String>, legacy: Option<String>) -> Option<String> {
    new.or(legacy)
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
    fn env_pick_new_wins_legacy_still_works_neither_set() {
        assert_eq!(
            pick_compat(Some("new".into()), Some("legacy".into())).as_deref(),
            Some("new")
        );
        assert_eq!(
            pick_compat(None, Some("legacy".into())).as_deref(),
            Some("legacy")
        );
        assert_eq!(pick_compat(None, None), None);
    }

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
