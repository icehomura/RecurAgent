//! Canonical config/auth/data path resolver — the single source of truth.
//!
//! Every site that needs to know *where* RecurAgent reads/writes user config, auth
//! credentials, or runtime state MUST resolve through [`resolve_config_context`]
//! and consume the returned [`ConfigContext`]. No call site recomputes these
//! paths from the environment on its own — that split-brain divergence was the
//! root cause of the two prior failed attempts at XDG-primary config.
//!
//! # Resolution rules
//!
//! Inputs: the `--data-dir` CLI flag plus the state-home and config-home
//! environment variables. Every env read is new-name-first with a legacy
//! fallback ([`ra_core::brand`]): `RA_HOME` > `RA_HOME`,
//! `RA_CONFIG_DIR` > `RA_CONFIG_DIR`. An empty-string value (env or flag)
//! counts as unset, and override values are normalized (tilde-expanded, made
//! absolute) before use.
//!
//! The *default* locations come from [`ra_core::brand`] and are the new name
//! only: `~/.ra` (`<config>/ra`). No legacy directory is consulted, migrated
//! or deleted — an install that used the old name starts fresh here.
//!
//! * `data_dir` = `--data-dir` > `RA_HOME` > `RA_HOME` > `~/.ra`
//!   (state/sessions/skills/logs).
//! * `is_explicit` = `--data-dir` set OR a state-home override that does not
//!   name the default location OR `RA_CONFIG_DIR`/`RA_CONFIG_DIR` set.
//!   `is_default = !is_explicit`. An override that names the default
//!   (`RA_HOME=~/.ra`, `RA_HOME=~/.ra`) is deliberately *not* explicit:
//!   it must not split config away from the default config home.
//! * `config_home` = `RA_CONFIG_DIR` > `RA_CONFIG_DIR` if set, else
//!   `data_dir` when the data dir came from an explicit override, else the
//!   brand config home (`<config>/ra`).
//! * `auth_home` = `RA_CONFIG_DIR` > `RA_CONFIG_DIR` if set, else the same
//!   brand config home. **DECOUPLED from `data_dir`** so per-profile gateways
//!   (which run with `--data-dir <profile-data>`) keep the host's shared,
//!   global `ra auth login`.
//!
//! A config-dir env var therefore always wins over an explicit `--data-dir`:
//! `--data-dir` moves *state*, while a config-dir env is the tenant /
//! multi-install isolation knob for where config **and** credentials are read
//! (see `api::admin_setup`, which relies on exactly that).

use std::path::{Path, PathBuf};

use ra_core::brand;

/// Resolved, canonical paths for config / auth / runtime state.
///
/// Constructed exactly once per command entrypoint via
/// [`resolve_config_context`] and threaded to every consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigContext {
    /// Where `config.json` is read from / written to (when not project-local).
    pub config_home: PathBuf,
    /// Where `auth.json` lives. GLOBAL (the brand config home) unless
    /// `RA_CONFIG_DIR` / `RA_CONFIG_DIR` is set.
    pub auth_home: PathBuf,
    /// Runtime state root: episodes, sessions, skills, memory, logs.
    pub data_dir: PathBuf,
    /// `true` when no explicit override was supplied (default install). When
    /// `true`, config load may fall back to the state home's `config.json`.
    pub is_default: bool,
}

/// Best-effort path normalization for comparison: expand a leading `~`, make
/// the path absolute relative to `$HOME`/CWD, then `canonicalize` if the path
/// exists on disk. Falls back to the lexical form when canonicalization fails
/// (e.g. the directory does not exist yet).
fn normalize_for_compare(path: &Path) -> PathBuf {
    let expanded = expand_tilde(path);
    let absolute = if expanded.is_absolute() {
        expanded
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(&expanded)
    } else {
        expanded
    };
    std::fs::canonicalize(&absolute).unwrap_or(absolute)
}

/// Normalize a path for RETURN (config_home / auth_home / data_dir): expand a
/// leading `~` and make it absolute, but do NOT `canonicalize`. Canonicalize is
/// reserved for the *comparison* — applying it to returned paths would resolve
/// symlinks (e.g. macOS `/var`→`/private/var`), require the path to already
/// exist, and surprise callers/tests with a rewritten prefix. We only need to
/// guarantee no literal `~`/relative segment leaks into a file path.
fn normalize_for_return(path: &Path) -> PathBuf {
    let expanded = expand_tilde(path);
    if expanded.is_absolute() {
        expanded
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(&expanded)
    } else {
        expanded
    }
}

/// Expand a leading `~` or `~/` against `$HOME`.
fn expand_tilde(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    } else if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    path.to_path_buf()
}

/// Normalize an override value (CLI flag or env var) before it is used: an
/// empty value is unset, and a `~`/relative path is expanded and made
/// absolute, because only absolute overrides are honoured.
fn normalize_override(path: Option<PathBuf>) -> Option<PathBuf> {
    path.filter(|path| !path.as_os_str().is_empty())
        .map(|path| normalize_for_return(&path))
}

/// `true` when a state-home override does not name `~/.ra` — i.e. it really
/// moves the state home somewhere else. See the module docs.
fn state_override_is_nondefault(override_path: &Path) -> bool {
    let Some(home) = dirs::home_dir() else {
        return true;
    };
    let normalized = normalize_for_compare(override_path);
    normalized != normalize_for_compare(&home.join(brand::STATE_DIR))
}

/// The brand config home with no override in play: `<config>/ra`. MUST only be
/// called when the config-dir env is unset — `brand::config_home` honours that
/// env itself. Falls back to the bare slug when no platform config dir can be
/// determined.
fn default_config_home() -> PathBuf {
    brand::config_home().unwrap_or_else(|| PathBuf::from(brand::APP_SLUG))
}

/// Resolve the canonical [`ConfigContext`] from the `--data-dir` flag plus the
/// state-home / config-home environment variables.
///
/// See the module docs for the full rule set. This is the ONLY function that
/// reads those env vars for path resolution.
pub fn resolve_config_context(cli_data_dir: Option<&Path>) -> ConfigContext {
    // New-name-first reads: RA_HOME > RA_HOME, RA_CONFIG_DIR > RA_CONFIG_DIR.
    let state_override = normalize_override(brand::env_compat("HOME").map(PathBuf::from));
    let config_override = normalize_override(brand::env_compat("CONFIG_DIR").map(PathBuf::from));
    let cli_override = normalize_override(cli_data_dir.map(PathBuf::from));

    // data_dir: --data-dir > RA_HOME > RA_HOME > ~/.ra. `brand::state_home`
    // implements the env tail; the CLI flag is the one input it does not know
    // about.
    let data_dir = cli_override
        .clone()
        .or_else(|| state_override.clone())
        .or_else(brand::state_home)
        .unwrap_or_else(|| PathBuf::from(brand::APP_SLUG));

    let state_override_explicit = state_override
        .as_deref()
        .is_some_and(state_override_is_nondefault);
    let data_dir_is_explicit = cli_override.is_some() || state_override_explicit;

    // config_home: the config-dir env wins (tenant isolation), else an explicit
    // data dir, else the brand config home.
    let config_home = match &config_override {
        Some(path) => path.clone(),
        None if data_dir_is_explicit => data_dir.clone(),
        None => default_config_home(),
    };

    // auth_home is GLOBAL: the config-dir env if set, else the brand config
    // home. NEVER data_dir.
    let auth_home = config_override.clone().unwrap_or_else(default_config_home);

    let is_default = !(data_dir_is_explicit || config_override.is_some());

    // Normalize the RETURNED paths (expand a leading `~`, make absolute, but do
    // NOT canonicalize). Without this, an env value like `RA_HOME=~/x` or a
    // relative `--data-dir foo` would propagate a literal `~`/relative segment
    // into the config/auth/state file paths.
    ConfigContext {
        config_home: normalize_for_return(&config_home),
        auth_home: normalize_for_return(&auth_home),
        data_dir: normalize_for_return(&data_dir),
        is_default,
    }
}

/// Process-wide lock for tests that mutate the global `HOME` / `RA_HOME` /
/// `RA_HOME` / `RA_CONFIG_DIR` / `RA_CONFIG_DIR` environment variables.
/// These vars are process-global, so EVERY env-pivoting test in the crate (here
/// and in `config.rs`) must serialize against this single mutex — separate
/// per-module locks would let tests in different modules race each other and
/// flake.
#[cfg(any(test, feature = "test-util"))]
pub static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    /// Alias for the crate-wide env lock (see [`super::TEST_ENV_LOCK`]).
    use super::TEST_ENV_LOCK as ENV_LOCK;

    /// Every environment variable the resolver reads, in both name generations.
    /// Pivoting saves all of them so no ambient value can leak in or out.
    const ENV_KEYS: [&str; 8] = [
        "HOME",
        "USERPROFILE",
        "RA_HOME",
        "RA_HOME",
        "RA_CONFIG_DIR",
        "RA_CONFIG_DIR",
        "XDG_CONFIG_HOME",
        "APPDATA",
    ];

    struct EnvGuard {
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvGuard {
        /// Pivot `HOME`/`USERPROFILE` to `home` and clear every override.
        #[allow(unsafe_code)]
        fn pivot(home: &Path) -> Self {
            let saved = ENV_KEYS
                .iter()
                .map(|key| (*key, std::env::var_os(key)))
                .collect();
            // SAFETY: callers hold ENV_LOCK; restored in Drop.
            unsafe {
                std::env::set_var("HOME", home);
                std::env::set_var("USERPROFILE", home);
                std::env::remove_var("RA_HOME");
                std::env::remove_var("RA_HOME");
                std::env::remove_var("RA_CONFIG_DIR");
                std::env::remove_var("RA_CONFIG_DIR");
                std::env::remove_var("XDG_CONFIG_HOME");
            }
            EnvGuard { saved }
        }
    }

    impl Drop for EnvGuard {
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            // SAFETY: callers hold ENV_LOCK for the guard's lifetime.
            unsafe {
                for (key, value) in &self.saved {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    /// Lock the env + pivot the home. The guards drop in reverse declaration
    /// order (`EnvGuard` first), so the env is restored before the lock frees.
    fn pivot(home: &Path) -> (std::sync::MutexGuard<'static, ()>, EnvGuard) {
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env = EnvGuard::pivot(home);
        (lock, env)
    }

    #[allow(unsafe_code)]
    fn set_env(key: &str, val: &str) {
        // SAFETY: callers hold ENV_LOCK.
        unsafe { std::env::set_var(key, val) };
    }

    #[allow(unsafe_code)]
    fn remove_env(key: &str) {
        // SAFETY: callers hold ENV_LOCK.
        unsafe { std::env::remove_var(key) };
    }

    // ── precedence ────────────────────────────────────────────────────

    /// The precedence rule, end to end: an explicit env override wins, and
    /// with no override the default is `~/.ra`.
    #[test]
    fn precedence_env_then_fresh() {
        let tmp = tempfile::tempdir().unwrap();
        let (_lock, _env) = pivot(tmp.path());
        let state = tmp.path().join("state");
        let config = tmp.path().join("config");

        // State home: `RA_HOME` wins.
        set_env("RA_HOME", state.to_str().unwrap());
        let ctx = resolve_config_context(None);
        assert_eq!(ctx.data_dir, state, "RA_HOME must win");
        assert!(!ctx.is_default);

        // Config home: `RA_CONFIG_DIR` governs config + auth even while a state
        // override is set.
        set_env("RA_CONFIG_DIR", config.to_str().unwrap());
        let ctx = resolve_config_context(None);
        assert_eq!(ctx.config_home, config);
        assert_eq!(ctx.auth_home, config, "config dir governs auth_home");

        // No override: the default is `~/.ra`, regardless of what exists on
        // disk — the resolver never consults another directory. `dirs`
        // resolves the Windows known folders from the user token rather than
        // `HOME`, so the pivot only moves the default locations on unix.
        #[cfg(not(windows))]
        {
            remove_env("RA_CONFIG_DIR");
            remove_env("RA_HOME");

            let ctx = resolve_config_context(None);
            assert_eq!(ctx.data_dir, tmp.path().join(".ra"));
            assert!(ctx.is_default);
        }
    }

    /// Empty-string values are treated as unset.
    #[test]
    fn empty_overrides_are_unset() {
        let tmp = tempfile::tempdir().unwrap();
        let (_lock, _env) = pivot(tmp.path());

        set_env("RA_HOME", "");
        set_env("RA_CONFIG_DIR", "");
        let ctx = resolve_config_context(None);
        assert!(ctx.is_default, "empty overrides must not count as explicit");
    }

    /// An override that names the default state home is NOT explicit: it must
    /// not split config away from the default config home.
    #[test]
    #[cfg(not(windows))]
    fn override_naming_the_default_state_home_is_not_explicit() {
        let tmp = tempfile::tempdir().unwrap();
        let (_lock, _env) = pivot(tmp.path());
        let default_config = tmp.path().join(".config").join("ra");

        set_env("RA_HOME", tmp.path().join(".ra").to_str().unwrap());
        let ctx = resolve_config_context(None);
        assert!(ctx.is_default, "RA_HOME==~/.ra must be is_default");
        assert_eq!(ctx.data_dir, tmp.path().join(".ra"));
        assert_eq!(
            ctx.config_home, default_config,
            "RA_HOME==~/.ra must resolve config_home to the brand default (no split-brain)"
        );
    }

    // ── data_dir / config_home / auth_home ────────────────────────────

    /// Default install on unix: data `~/.ra`, config + auth `${XDG_CONFIG_HOME:
    /// -~/.config}/ra` — true XDG `~/.config`, NOT Apple's
    /// `~/Library/Application Support`. An absolute $XDG_CONFIG_HOME wins.
    #[test]
    #[cfg(not(windows))]
    fn pure_default_resolves_to_brand_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let (_lock, _env) = pivot(tmp.path());

        let ctx = resolve_config_context(None);
        assert!(ctx.is_default);
        assert_eq!(ctx.data_dir, tmp.path().join(".ra"));
        assert_eq!(ctx.config_home, tmp.path().join(".config").join("ra"));
        assert_eq!(ctx.auth_home, tmp.path().join(".config").join("ra"));
        assert!(
            !ctx.config_home
                .to_string_lossy()
                .contains("Application Support"),
            "config must not live in ~/Library/Application Support, got {}",
            ctx.config_home.display()
        );

        // Absolute XDG_CONFIG_HOME wins.
        let xdg = tmp.path().join("xdgcfg");
        set_env("XDG_CONFIG_HOME", xdg.to_str().unwrap());
        let ctx2 = resolve_config_context(None);
        assert_eq!(ctx2.config_home, xdg.join("ra"));
    }

    /// Pre-existing legacy directories are ignored: with no override the
    /// defaults are the new-name locations (`~/.ra`, `<config>/ra`) even when
    /// the old layout is present on disk.
    #[test]
    #[cfg(not(windows))]
    fn default_dirs_ignore_pre_existing_legacy_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let (_lock, _env) = pivot(tmp.path());
        std::fs::create_dir_all(tmp.path().join(".ra")).unwrap();
        std::fs::create_dir_all(tmp.path().join(".config").join("ra")).unwrap();

        let ctx = resolve_config_context(None);
        assert!(ctx.is_default);
        assert_eq!(ctx.data_dir, tmp.path().join(".ra"));
        assert_eq!(ctx.config_home, tmp.path().join(".config").join("ra"));
        assert_eq!(ctx.auth_home, tmp.path().join(".config").join("ra"));
    }

    /// A non-default state home pushes config beside the state dir, while auth
    /// stays global.
    #[test]
    fn nondefault_state_home_sets_config_home_to_state_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let (_lock, _env) = pivot(tmp.path());
        let custom = tmp.path().join("projects").join("foo");
        set_env("RA_HOME", custom.to_str().unwrap());
        let ctx = resolve_config_context(None);
        assert!(!ctx.is_default);
        assert_eq!(ctx.data_dir, custom);
        assert_eq!(ctx.config_home, custom);
        // auth stays GLOBAL.
        assert_ne!(ctx.auth_home, ctx.config_home);
        #[cfg(not(windows))]
        assert_eq!(ctx.auth_home, tmp.path().join(".config").join("ra"));
    }

    /// `--data-dir T` → config_home == T, but auth_home stays the GLOBAL
    /// brand config home (shared login across per-profile gateways).
    #[test]
    fn cli_data_dir_isolates_config_but_auth_stays_global() {
        let tmp = tempfile::tempdir().unwrap();
        let (_lock, _env) = pivot(tmp.path());
        let t = tmp.path().join("profile-data");
        let ctx = resolve_config_context(Some(&t));
        assert!(!ctx.is_default);
        assert_eq!(ctx.data_dir, t);
        assert_eq!(ctx.config_home, t, "explicit --data-dir → config in T");
        assert_ne!(
            ctx.auth_home, t,
            "auth MUST stay global (brand config home) across --data-dir profiles"
        );
        #[cfg(not(windows))]
        assert_eq!(ctx.auth_home, tmp.path().join(".config").join("ra"));
    }

    /// The config-dir env governs BOTH config and auth, even with a
    /// `--data-dir`: it is the tenant-isolation knob (`api::admin_setup`).
    #[test]
    fn config_dir_env_governs_config_and_auth_even_with_data_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let (_lock, _env) = pivot(tmp.path());
        let c = tmp.path().join("tenant-config");
        set_env("RA_CONFIG_DIR", c.to_str().unwrap());
        let t = tmp.path().join("tenant-data");
        let ctx = resolve_config_context(Some(&t));
        assert!(!ctx.is_default);
        assert_eq!(ctx.data_dir, t);
        assert_eq!(ctx.config_home, c, "config-dir env governs config_home");
        assert_eq!(ctx.auth_home, c, "config-dir env governs auth_home");
    }
}
