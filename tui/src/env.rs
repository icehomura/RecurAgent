//! ra/legacy environment + user-path compatibility (`ra-fork.md` follow-up #2).
//!
//! Every environment read prefers the new ra spelling and falls back to the
//! pre-rename one:
//!
//! | New | Legacy | Scope |
//! |---|---|---|
//! | `RA_TUI_*` | `OCTOSCODE_*` | behaviour owned by this client |
//! | `RA_*` | `OCTOS_*` | names shared with the kernel (token, home, prefix) |
//!
//! Paths follow the same rule: `~/.ra` is preferred when it exists, a legacy
//! `~/.ra` that already holds state is used when only it exists, and a fresh
//! install targets `~/.ra`. Legacy state is never migrated or deleted.
//!
//! Env reads are routed through [`env_compat`] / [`env_os_compat`]; the pure
//! cores ([`pick_compat`], [`pick_home_entry`]) are split out so the fallback
//! order is unit-testable without mutating process env (`std::env::set_var` is
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

/// Pick the ra-side entry under `home`: `home/<new_rel>` when it exists, else
/// `home/<legacy_rel>` when *only* that exists (keep using the legacy state we
/// found), else `home/<new_rel>` (fresh installs create the ra path).
pub fn pick_home_entry(home: &Path, new_rel: &str, legacy_rel: &str) -> PathBuf {
    let new = home.join(new_rel);
    if new.exists() {
        return new;
    }
    let legacy = home.join(legacy_rel);
    if legacy.exists() {
        return legacy;
    }
    new
}

/// [`pick_home_entry`] rooted at [`home_dir`]; `None` when no home resolves.
pub fn ra_or_legacy_home_entry(new_rel: &str, legacy_rel: &str) -> Option<PathBuf> {
    home_dir().map(|home| pick_home_entry(&home, new_rel, legacy_rel))
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
    fn home_entry_prefers_new_then_legacy_then_defaults_new() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();

        // Neither exists → fresh installs target the ra entry.
        assert_eq!(
            pick_home_entry(home, ".ra", ".octos"),
            home.join(".ra"),
            "neither present must default to the new entry"
        );

        // Only legacy exists → keep using the legacy state.
        std::fs::create_dir(home.join(".octos")).expect("create legacy");
        assert_eq!(
            pick_home_entry(home, ".ra", ".octos"),
            home.join(".octos"),
            "legacy state must still be honoured when it is all there is"
        );

        // Both exist → new wins.
        std::fs::create_dir(home.join(".ra")).expect("create new");
        assert_eq!(
            pick_home_entry(home, ".ra", ".octos"),
            home.join(".ra"),
            "the ra entry must win once it exists"
        );
    }

    #[test]
    fn ra_or_legacy_home_entry_uses_env_home() {
        // The live-process wrapper: no assertion about the host's HOME values,
        // only that a resolved entry is the ra or legacy spelling.
        if let Some(entry) = ra_or_legacy_home_entry(".ra", ".octos") {
            let name = entry.file_name().and_then(|n| n.to_str()).unwrap_or("");
            assert!(
                name == ".ra" || name == ".octos",
                "unexpected entry {entry:?}"
            );
        }
    }
}
