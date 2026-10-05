//! Product identity: name, environment-variable compatibility and state/config locations.
//!
//! The kernel was renamed from `ra` to `ra`. Two rules apply here:
//!
//! * **environment variables are aliased** — `RA_<NAME>` wins and `RA_<NAME>` is still
//!   honoured ([`env_compat`]), so an operator's existing exports keep working;
//! * **state and configuration are not** — [`state_home`] and [`config_home`] resolve to the
//!   `ra` locations only. No legacy directory is read, migrated or cleared, and no legacy
//!   credentials, payloads or databases are interpreted: an install that used the old name
//!   starts fresh with the new one.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Lowercase product slug used for directories, package names and CLI defaults.
pub const APP_SLUG: &str = "ra";
/// Uppercase product name used in user-visible text.
pub const APP_NAME: &str = "ra";
/// Prefix of every environment variable the product introduces.
pub const ENV_PREFIX: &str = "RA_";
/// Prefix of the variables the product used before the rename.
pub const LEGACY_ENV_PREFIX: &str = "RA_";
/// Directory name the product used before the rename.
pub const LEGACY_SLUG: &str = "ra";
/// State-home directory inside the user's home: `~/.ra`.
pub const STATE_DIR: &str = ".ra";
/// Legacy state-home directory: `~/.ra`.
pub const LEGACY_STATE_DIR: &str = ".ra";

/// Read `RA_<name>` and fall back to `RA_<name>`.
///
/// `name` is the suffix without either prefix: `env_compat("HOME")` reads `RA_HOME` then
/// `ra_HOME`. An empty value counts as unset.
pub fn env_compat(name: &str) -> Option<OsString> {
    env_compat_of(
        std::env::var_os(format!("{ENV_PREFIX}{name}")),
        std::env::var_os(format!("{LEGACY_ENV_PREFIX}{name}")),
    )
}

/// [`env_compat`] for callers that want a `String` (lossy for non-UTF-8 values).
pub fn env_compat_str(name: &str) -> Option<String> {
    env_compat(name).map(|value| value.to_string_lossy().into_owned())
}

/// Pure form of [`env_compat`]: new wins, legacy is the fallback, empty means unset.
pub fn env_compat_of(new: Option<OsString>, legacy: Option<OsString>) -> Option<OsString> {
    non_empty(new).or_else(|| non_empty(legacy))
}

fn non_empty(value: Option<OsString>) -> Option<OsString> {
    value.filter(|value| !value.is_empty())
}

fn non_empty_path(path: Option<PathBuf>) -> Option<PathBuf> {
    path.filter(|path| !path.as_os_str().is_empty() && path.is_absolute())
}

/// The runtime state home: `RA_HOME` → `ra_HOME` → `~/.ra`.
///
/// No legacy state directory is consulted: an install that used the old name
/// starts fresh under `~/.ra` (the fork keeps no database compatibility).
pub fn state_home() -> Option<PathBuf> {
    if let Some(path) = non_empty_path(env_compat("HOME").map(PathBuf::from)) {
        return Some(path);
    }
    Some(dirs::home_dir()?.join(STATE_DIR))
}

/// The config home: `RA_CONFIG_DIR` → `ra_CONFIG_DIR` → `<config>/ra`, where
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
    fn env_compat_prefers_the_new_name_and_ignores_empty() {
        assert_eq!(
            env_compat_of(Some("new".into()), Some("legacy".into())),
            Some(OsString::from("new"))
        );
        assert_eq!(env_compat_of(None, Some("legacy".into())), Some(OsString::from("legacy")));
        assert_eq!(
            env_compat_of(Some(OsString::from("")), Some("legacy".into())),
            Some(OsString::from("legacy"))
        );
        assert_eq!(
            env_compat_of(Some(OsString::from("")), Some(OsString::from(""))),
            None
        );
    }

    #[test]
    fn constants_are_the_agreed_names() {
        assert_eq!(APP_SLUG, "ra");
        assert_eq!(APP_NAME, "ra");
        assert_eq!(ENV_PREFIX, "RA_");
        assert_eq!(LEGACY_ENV_PREFIX, "RA_");
        assert_eq!(LEGACY_SLUG, "ra");   // legacy spelling, no longer consulted for paths
        // the state dir is a dot-directory in $HOME, the config dir is plain
        assert_eq!(STATE_DIR, ".ra");
        assert_eq!(LEGACY_STATE_DIR, ".ra");
    }
}
