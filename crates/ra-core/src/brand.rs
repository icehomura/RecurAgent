//! Product identity: name, environment variables and state/config locations.
//!
//! The product is RecurAgent; the code-level slug is `ra`. One rule applies here:
//!
//! * **environment variables** are read as `RA_<NAME>` ([`env_compat`]); an empty value
//!   counts as unset;
//! * **state and configuration** resolve to the `ra` locations only — nothing is read from,
//!   migrated out of, or cleared from an older install, so a machine that used a previous
//!   name starts fresh under the current one.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Lowercase product slug used for directories, package names and CLI defaults.
pub const APP_SLUG: &str = "ra";
/// Uppercase product name used in user-visible text.
pub const APP_NAME: &str = "ra";
/// Prefix of every environment variable the product reads.
pub const ENV_PREFIX: &str = "RA_";
/// State-home directory inside the user's home: `~/.ra`.
pub const STATE_DIR: &str = ".ra";

/// Read `RA_<name>`.
///
/// `name` is the suffix without the prefix: `env_compat("HOME")` reads `RA_HOME`.
/// An empty value counts as unset.
pub fn env_compat(name: &str) -> Option<OsString> {
    non_empty(std::env::var_os(format!("{ENV_PREFIX}{name}")))
}

/// [`env_compat`] for callers that want a `String` (lossy for non-UTF-8 values).
pub fn env_compat_str(name: &str) -> Option<String> {
    env_compat(name).map(|value| value.to_string_lossy().into_owned())
}

fn non_empty(value: Option<OsString>) -> Option<OsString> {
    value.filter(|value| !value.is_empty())
}

fn non_empty_path(path: Option<PathBuf>) -> Option<PathBuf> {
    path.filter(|path| !path.as_os_str().is_empty() && path.is_absolute())
}

/// The runtime state home: `RA_HOME` → `RA_HOME` → `~/.ra`.
///
/// No legacy state directory is consulted: an install that used the old name
/// starts fresh under `~/.ra` (the fork keeps no database compatibility).
pub fn state_home() -> Option<PathBuf> {
    if let Some(path) = non_empty_path(env_compat("HOME").map(PathBuf::from)) {
        return Some(path);
    }
    Some(dirs::home_dir()?.join(STATE_DIR))
}

/// The config home: `RA_CONFIG_DIR` → `RA_CONFIG_DIR` → `<config>/ra`, where
/// `<config>` is `%APPDATA%` on Windows and `${XDG_CONFIG_HOME:-~/.config}`
/// elsewhere. The pre-rename config directory is not consulted.
pub fn config_home() -> Option<PathBuf> {
    if let Some(path) = non_empty_path(env_compat("CONFIG_DIR").map(PathBuf::from)) {
        return Some(path);
    }
    Some(platform_config_base()?.join(APP_SLUG))
}

/// Join `name` onto [`state_home`].
pub fn state_path(name: impl AsRef<Path>) -> PathBuf {
    state_home()
        .unwrap_or_else(|| PathBuf::from(STATE_DIR))
        .join(name)
}

fn platform_config_base() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        dirs::config_dir()
    }
    #[cfg(not(windows))]
    {
        // $XDG_CONFIG_HOME wins when set to an ABSOLUTE path (the spec ignores relative values).
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
            let path = PathBuf::from(xdg);
            if path.is_absolute() {
                return Some(path);
            }
        }
        dirs::home_dir().map(|home| home.join(".config"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        // absolute on every platform (a bare "/x" is not absolute on Windows)
        std::env::temp_dir().join("ra-brand-test").join(s)
    }

    #[test]
    fn env_compat_reads_only_the_ra_prefix() {
        // A name that cannot be set in the environment must resolve to `None`
        // (setting one would need `unsafe` under edition 2024's `set_var`).
        assert_eq!(env_compat("BRAND_TEST_MISSING"), None);
        assert_eq!(env_compat_str("BRAND_TEST_MISSING"), None);
    }

    #[test]
    fn constants_are_the_agreed_names() {
        assert_eq!(APP_SLUG, "ra");
        assert_eq!(APP_NAME, "ra");
        assert_eq!(ENV_PREFIX, "RA_");
        // the state dir is a dot-directory in $HOME
        assert_eq!(STATE_DIR, ".ra");
    }
}
