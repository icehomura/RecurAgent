//! Product identity: name, environment-variable compatibility and state/config locations.
//!
//! The kernel was renamed from `octos` to `ra`. Everything a *user* can see or set must move
//! without breaking an existing install, so every read here is new-name-first with a legacy
//! fallback:
//!
//! * env vars — `RA_<NAME>` wins, `OCTOS_<NAME>` is still honoured ([`env_compat`]);
//! * state dir — `~/.ra`, but an existing `~/.octos` keeps being used ([`state_home`]);
//! * config dir — `<config>/ra`, with the same "prefer the new one only if it exists" rule
//!   ([`config_home`]).
//!
//! Nothing here migrates or deletes legacy state: *prefer the new location when it exists,
//! otherwise keep using the legacy one the user already has, otherwise start fresh at the new one.*

use std::ffi::{OsString};
use std::path::{Path, PathBuf};

/// Lowercase product slug used for directories, package names and CLI defaults.
pub const APP_SLUG: &str = "ra";
/// Uppercase product name used in user-visible text.
pub const APP_NAME: &str = "ra";
/// Prefix of every environment variable the product introduces.
pub const ENV_PREFIX: &str = "RA_";
/// Prefix of the variables the product used before the rename.
pub const LEGACY_ENV_PREFIX: &str = "OCTOS_";
/// Directory name the product used before the rename.
pub const LEGACY_SLUG: &str = "octos";
/// State-home directory inside the user's home: `~/.ra`.
pub const STATE_DIR: &str = ".ra";
/// Legacy state-home directory: `~/.octos`.
pub const LEGACY_STATE_DIR: &str = ".octos";

/// Read `RA_<name>` and fall back to `OCTOS_<name>`.
///
/// `name` is the suffix without either prefix: `env_compat("HOME")` reads `RA_HOME` then
/// `OCTOS_HOME`. An empty value counts as unset.
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

/// Pick a directory: explicit new override, explicit legacy override, the existing new dir, the
/// existing legacy dir, else the new default.
pub fn choose(
    new_override: Option<PathBuf>,
    legacy_override: Option<PathBuf>,
    new_default: PathBuf,
    legacy_default: PathBuf,
    new_exists: bool,
    legacy_exists: bool,
) -> PathBuf {
    if let Some(path) = non_empty_path(new_override) {
        return path;
    }
    if let Some(path) = non_empty_path(legacy_override) {
        return path;
    }
    if new_exists {
        return new_default;
    }
    if legacy_exists {
        return legacy_default;
    }
    new_default
}

/// The runtime state home: `RA_HOME` → `OCTOS_HOME` → existing `~/.ra` → existing `~/.octos`.
pub fn state_home() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let new_dir = home.join(STATE_DIR);
    let legacy_dir = home.join(LEGACY_STATE_DIR);
    Some(choose(
        env_compat("HOME").map(PathBuf::from),
        None,
        new_dir.clone(),
        legacy_dir.clone(),
        new_dir.is_dir(),
        legacy_dir.is_dir(),
    ))
}

/// The config home: `RA_CONFIG_DIR` → `OCTOS_CONFIG_DIR` → existing `<config>/ra` → existing
/// `<config>/octos`, where `<config>` is `%APPDATA%` on Windows and
/// `${XDG_CONFIG_HOME:-~/.config}` elsewhere.
pub fn config_home() -> Option<PathBuf> {
    let base = platform_config_base()?;
    let new_dir = base.join(APP_SLUG);
    let legacy_dir = base.join(LEGACY_SLUG);
    Some(choose(
        env_compat("CONFIG_DIR").map(PathBuf::from),
        None,
        new_dir.clone(),
        legacy_dir.clone(),
        new_dir.is_dir(),
        legacy_dir.is_dir(),
    ))
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
    fn choose_prefers_overrides_then_existing_state_then_the_new_default() {
        let new_default = p(".ra");
        let legacy_default = p(".octos");
        // explicit overrides win over everything on disk
        assert_eq!(
            choose(Some(p("custom")), None, new_default.clone(), legacy_default.clone(), true, true),
            p("custom")
        );
        // legacy override still honoured when the new one is absent
        assert_eq!(
            choose(None, Some(p("legacy-custom")), new_default.clone(), legacy_default.clone(), true, false),
            p("legacy-custom")
        );
        // relative overrides are ignored (they are not paths the user meant)
        assert_eq!(
            choose(Some(PathBuf::from("relative")), None, new_default.clone(), legacy_default.clone(), false, true),
            legacy_default
        );
        // both dirs present -> the new one
        assert_eq!(
            choose(None, None, new_default.clone(), legacy_default.clone(), true, true),
            new_default
        );
        // only the legacy dir present -> keep the user's state
        assert_eq!(
            choose(None, None, new_default.clone(), legacy_default.clone(), false, true),
            legacy_default
        );
        // fresh install -> the new default
        assert_eq!(
            choose(None, None, new_default.clone(), legacy_default.clone(), false, false),
            new_default
        );
    }

    #[test]
    fn constants_are_the_agreed_names() {
        assert_eq!(APP_SLUG, "ra");
        assert_eq!(APP_NAME, "ra");
        assert_eq!(ENV_PREFIX, "RA_");
        assert_eq!(LEGACY_ENV_PREFIX, "OCTOS_");
        assert_eq!(LEGACY_SLUG, "octos");
        // the state dir is a dot-directory in $HOME, the config dir is plain
        assert_eq!(STATE_DIR, ".ra");
        assert_eq!(LEGACY_STATE_DIR, ".octos");
    }
}
