//! Canonical config/auth/data path resolver — the single source of truth.
//!
//! Every site that needs to know *where* ra reads/writes user config, auth
//! credentials, or runtime state MUST resolve through [`resolve_config_context`]
//! and consume the returned [`ConfigContext`]. No call site recomputes these
//! paths from the environment on its own — that split-brain divergence was the
//! root cause of the two prior failed attempts at XDG-primary config.
//!
//! # Resolution rules
//!
//! Inputs: the `--data-dir` CLI flag plus the state-home and config-home
//! environment variables. Every env read is new-name-first with a legacy
//! fallback ([`ra_core::brand`]): `RA_HOME` > `OCTOS_HOME`,
//! `RA_CONFIG_DIR` > `OCTOS_CONFIG_DIR`. An empty-string value (env or flag)
//! counts as unset, and override values are normalized (tilde-expanded, made
//! absolute) before use.
//!
//! The *default* locations come from [`ra_core::brand`]: an existing `~/.ra`
//! (`<config>/ra`) is preferred, else an existing `~/.ra` (`<config>/ra`)
//! keeps being used, else the new name is picked. Nothing here migrates or
//! deletes that state on its own.
//!
//! * `data_dir` = `--data-dir` > `RA_HOME` > `OCTOS_HOME` > existing `~/.ra` >
//!   existing `~/.ra` > `~/.ra` (state/sessions/skills/logs).
//! * `is_explicit` = `--data-dir` set OR a state-home override that does not
//!   name one of the default locations OR `RA_CONFIG_DIR`/`OCTOS_CONFIG_DIR`
//!   set. `is_default = !is_explicit`. An override that names the default
//!   (`RA_HOME=~/.ra`, `OCTOS_HOME=~/.ra`) is deliberately *not* explicit:
//!   it must not split config away from the default config home, and it keeps
//!   the legacy migration working for an install that exported that path.
//! * `config_home` = `RA_CONFIG_DIR` > `OCTOS_CONFIG_DIR` if set, else
//!   `data_dir` when the data dir came from an explicit override, else the
//!   brand config home (existing `<config>/ra` > existing `<config>/ra` >
//!   `<config>/ra`).
//! * `auth_home` = `RA_CONFIG_DIR` > `OCTOS_CONFIG_DIR` if set, else the same
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
    /// `RA_CONFIG_DIR` / `OCTOS_CONFIG_DIR` is set.
    pub auth_home: PathBuf,
    /// Runtime state root: episodes, sessions, skills, memory, logs.
    pub data_dir: PathBuf,
    /// `true` when no explicit override was supplied (default install). When
    /// `true`, config load may fall back to the state home's `config.json`, and
    /// config migration into the brand config home is permitted.
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

/// Normalize an override value (CLI flag or env var) before it reaches the
/// [`ra_core::brand::choose`] helpers: an empty value is unset, and a
/// `~`/relative path is expanded and made absolute, because `choose` only
/// honours absolute overrides.
fn normalize_override(path: Option<PathBuf>) -> Option<PathBuf> {
    path.filter(|path| !path.as_os_str().is_empty())
        .map(|path| normalize_for_return(&path))
}

/// `true` when a state-home override does not name `~/.ra` or `~/.ra` — i.e.
/// it really moves the state home somewhere else. See the module docs.
fn state_override_is_nondefault(override_path: &Path) -> bool {
    let Some(home) = dirs::home_dir() else {
        return true;
    };
    let normalized = normalize_for_compare(override_path);
    normalized != normalize_for_compare(&home.join(brand::STATE_DIR))
        && normalized != normalize_for_compare(&home.join(brand::LEGACY_STATE_DIR))
}

/// The brand config home with no override in play: existing `<config>/ra` >
/// existing `<config>/ra` > `<config>/ra`. MUST only be called when the
/// config-dir env is unset — `brand::config_home` honours that env itself.
/// Falls back to the bare slug when no platform config dir can be determined.
fn default_config_home() -> PathBuf {
    brand::config_home().unwrap_or_else(|| PathBuf::from(brand::APP_SLUG))
}

/// Resolve the canonical [`ConfigContext`] from the `--data-dir` flag plus the
/// state-home / config-home environment variables.
///
/// See the module docs for the full rule set. This is the ONLY function that
/// reads those env vars for path resolution (plus [`run_migrations`]'s single
/// config-dir presence check, which decides whether the resolved `auth_home` is
/// the global default).
pub fn resolve_config_context(cli_data_dir: Option<&Path>) -> ConfigContext {
    // New-name-first reads: RA_HOME > OCTOS_HOME, RA_CONFIG_DIR > OCTOS_CONFIG_DIR.
    let state_override = normalize_override(brand::env_compat("HOME").map(PathBuf::from));
    let config_override = normalize_override(brand::env_compat("CONFIG_DIR").map(PathBuf::from));
    let cli_override = normalize_override(cli_data_dir.map(PathBuf::from));

    // data_dir: --data-dir > RA_HOME > OCTOS_HOME > existing ~/.ra > existing
    // ~/.ra > ~/.ra. `brand::state_home` implements the env + existence tail;
    // the CLI flag is the one input it does not know about.
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
    let auth_home = config_override
        .clone()
        .unwrap_or_else(default_config_home);

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

/// Atomically copy `src` into `dest`, non-destructively and idempotently.
///
/// Semantics (shared by config + auth migration):
/// * No-op (returns `false`) if `dest` already exists or `src` is missing —
///   we never overwrite a destination or invent a source.
/// * `mkdir -p` the destination's parent directory.
/// * Write into a temp file *in the destination directory*, `fsync` it, set
///   `mode` (Unix only) on the temp file before the rename so the final file
///   never exists with looser permissions, then `fs::rename` (atomic on the
///   same filesystem).
/// * Best-effort: any I/O error is swallowed and reported as `false` (the
///   legacy file is always left intact, so a failed migration degrades to
///   "config/auth still read from legacy" rather than data loss).
///
/// Returns `true` when a copy was performed.
pub fn atomic_copy_into(src: &Path, dest: &Path, mode: Option<u32>) -> bool {
    if dest.exists() || !src.exists() {
        return false;
    }
    let Some(parent) = dest.parent() else {
        return false;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return false;
    }
    let Ok(bytes) = std::fs::read(src) else {
        return false;
    };

    // Unpredictable temp name in the destination directory (same filesystem →
    // atomic rename). Combining pid, a monotonic counter, and the nanosecond
    // clock keeps the name from being guessable so an attacker cannot
    // pre-plant a file/symlink that we would open-and-truncate.
    let tmp = parent.join(format!(
        ".{}.{}.{}.{}.tmp",
        dest.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "ra-migrate".to_string()),
        std::process::id(),
        next_temp_counter(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));

    let write_ok = write_temp(&tmp, &bytes, mode);
    if !write_ok {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }

    // `fs::rename` overwrites an existing dest on Unix. We already established
    // `!dest.exists()` above and migration runs once at startup, so the
    // remaining window is negligible; keep the rename (atomic same-fs swap).
    if std::fs::rename(&tmp, dest).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    true
}

/// Monotonic per-process counter for temp-file name uniqueness.
fn next_temp_counter() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Write `bytes` to a FRESH temp file (`create_new` — never opens an existing
/// path, so a pre-planted file/symlink at `tmp` causes a clean failure rather
/// than a truncate-and-write), applying `mode` on Unix, and fsync before close.
fn write_temp(tmp: &Path, bytes: &[u8], mode: Option<u32>) -> bool {
    use std::io::Write as _;

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut opts = std::fs::OpenOptions::new();
        // create_new(true) ⇒ O_CREAT|O_EXCL: fails if `tmp` already exists,
        // and applies `mode` atomically at creation (no looser-perm window).
        opts.write(true).create_new(true);
        if let Some(m) = mode {
            opts.mode(m);
        }
        let Ok(mut f) = opts.open(tmp) else {
            return false;
        };
        if f.write_all(bytes).is_err() {
            return false;
        }
        // Defense in depth: re-assert the mode after creation in case a umask
        // or platform quirk loosened it. Failure here aborts the copy.
        if let Some(m) = mode {
            use std::os::unix::fs::PermissionsExt as _;
            if std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(m)).is_err() {
                return false;
            }
        }
        f.sync_all().is_ok()
    }

    #[cfg(not(unix))]
    {
        let _ = mode;
        let Ok(mut f) = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(tmp)
        else {
            return false;
        };
        if f.write_all(bytes).is_err() {
            return false;
        }
        f.sync_all().is_ok()
    }
}

/// State-dir locations an earlier install may have written `config.json` /
/// `auth.json` into: the new `~/.ra` first, then the legacy `~/.ra`. Built
/// from the brand constants and NEVER from an explicit override, so a migration
/// can only ever read the user's real state dirs — migrating a per-profile or
/// per-tenant state dir into the global config home would leak credentials.
fn migration_source_roots() -> Vec<PathBuf> {
    match dirs::home_dir() {
        Some(home) => vec![
            home.join(brand::STATE_DIR),
            home.join(brand::LEGACY_STATE_DIR),
        ],
        None => vec![PathBuf::from(brand::STATE_DIR)],
    }
}

/// Run the config + auth migrations for the given context. Idempotent and
/// best-effort. Call once at the command entrypoint.
///
/// Sources are the state dirs from [`migration_source_roots`] (new name first),
/// and every source file is left intact.
///
/// * Config migration: only when `ctx.is_default`. Copies
///   `<state home>/config.json` → `ctx.config_home/config.json` and logs a
///   one-line notice when the copy happens.
/// * Auth migration: only when `ctx.auth_home` is the brand config home (i.e.
///   `RA_CONFIG_DIR` / `OCTOS_CONFIG_DIR` is unset). Copies
///   `<state home>/auth.json` → `ctx.auth_home/auth.json` with mode `0600`.
///   Never migrates host auth into a tenant-scoped config dir.
pub fn run_migrations(ctx: &ConfigContext) {
    // `auth_home` is the global default exactly when no config-dir override is
    // set, so gate on that same compat read (new name first). `ctx.auth_home`
    // alone cannot tell "default" apart from "explicitly set to the default".
    let auth_home_is_default = brand::env_compat("CONFIG_DIR").is_none();
    let state_roots = migration_source_roots();

    // ── Config migration (default installs only) ──────────────────────
    if ctx.is_default {
        let dest = ctx.config_home.join("config.json");
        for state_root in &state_roots {
            let legacy_config = state_root.join("config.json");
            if legacy_config != dest && atomic_copy_into(&legacy_config, &dest, None) {
                tracing::info!(
                    from = %legacy_config.display(),
                    to = %dest.display(),
                    "migrated config.json to the ra config directory (legacy copy left intact)"
                );
                break;
            }
        }
    } else if auth_home_is_default {
        // Non-default data dir (an explicit `--data-dir`, `RA_HOME` /
        // `OCTOS_HOME`): config now lives in the state dir. Surface a one-line
        // notice so the user is not surprised. No copy — explicit dirs are
        // isolated; this is informational.
        tracing::info!(
            config_home = %ctx.config_home.display(),
            "state-home override active: config.json is read/written under the state dir"
        );
    }

    // ── Auth migration (global-default auth_home only) ────────────────
    if auth_home_is_default {
        let dest = ctx.auth_home.join("auth.json");
        for state_root in &state_roots {
            let legacy_auth = state_root.join("auth.json");
            if legacy_auth != dest && atomic_copy_into(&legacy_auth, &dest, Some(0o600)) {
                tracing::info!(
                    from = %legacy_auth.display(),
                    to = %dest.display(),
                    "migrated auth.json to the ra config directory (mode 0600, legacy copy left intact)"
                );
                break;
            }
        }
    }
}

/// Process-wide lock for tests that mutate the global `HOME` / `RA_HOME` /
/// `OCTOS_HOME` / `RA_CONFIG_DIR` / `OCTOS_CONFIG_DIR` environment variables.
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
        "OCTOS_HOME",
        "RA_CONFIG_DIR",
        "OCTOS_CONFIG_DIR",
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
                std::env::remove_var("OCTOS_HOME");
                std::env::remove_var("RA_CONFIG_DIR");
                std::env::remove_var("OCTOS_CONFIG_DIR");
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

    /// The precedence rule, end to end: new-name env wins, the legacy env still
    /// works, an existing `~/.ra` beats an existing `~/.ra`, and a fresh
    /// install starts at `~/.ra`.
    #[test]
    fn precedence_new_env_then_legacy_env_then_existing_then_fresh() {
        let tmp = tempfile::tempdir().unwrap();
        let (_lock, _env) = pivot(tmp.path());
        let new_state = tmp.path().join("new-state");
        let legacy_state = tmp.path().join("legacy-state");
        let new_config = tmp.path().join("new-config");
        let legacy_config = tmp.path().join("legacy-config");

        // State home: RA_HOME wins over OCTOS_HOME…
        set_env("RA_HOME", new_state.to_str().unwrap());
        set_env("OCTOS_HOME", legacy_state.to_str().unwrap());
        let ctx = resolve_config_context(None);
        assert_eq!(ctx.data_dir, new_state, "RA_HOME must win over OCTOS_HOME");
        assert!(!ctx.is_default);
        // …and the legacy name alone still works.
        remove_env("RA_HOME");
        let ctx = resolve_config_context(None);
        assert_eq!(ctx.data_dir, legacy_state, "OCTOS_HOME must still work");

        // Config home: RA_CONFIG_DIR wins over OCTOS_CONFIG_DIR, and the
        // config dir governs config + auth even while a state override is set.
        set_env("RA_CONFIG_DIR", new_config.to_str().unwrap());
        set_env("OCTOS_CONFIG_DIR", legacy_config.to_str().unwrap());
        let ctx = resolve_config_context(None);
        assert_eq!(ctx.config_home, new_config);
        assert_eq!(ctx.auth_home, new_config);
        remove_env("RA_CONFIG_DIR");
        let ctx = resolve_config_context(None);
        assert_eq!(ctx.config_home, legacy_config);
        assert_eq!(ctx.auth_home, legacy_config, "config dir governs auth_home");

        // Existence rules. `dirs` resolves the Windows known folders from the
        // user token rather than `HOME`, so the pivot only moves the default
        // locations on unix.
        #[cfg(not(windows))]
        {
            remove_env("RA_CONFIG_DIR");
            remove_env("OCTOS_CONFIG_DIR");
            remove_env("OCTOS_HOME");

            // Existing ~/.ra beats existing ~/.ra.
            std::fs::create_dir_all(tmp.path().join(".ra")).unwrap();
            std::fs::create_dir_all(tmp.path().join(".ra")).unwrap();
            let ctx = resolve_config_context(None);
            assert_eq!(ctx.data_dir, tmp.path().join(".ra"));
            assert!(ctx.is_default);

            // Existing ~/.ra alone keeps being used.
            std::fs::remove_dir(tmp.path().join(".ra")).unwrap();
            let ctx = resolve_config_context(None);
            assert_eq!(ctx.data_dir, tmp.path().join(".ra"));

            // Fresh install → ~/.ra.
            std::fs::remove_dir(tmp.path().join(".ra")).unwrap();
            let ctx = resolve_config_context(None);
            assert_eq!(ctx.data_dir, tmp.path().join(".ra"));
        }
    }

    /// Empty-string values (both name generations) are treated as unset.
    #[test]
    fn empty_overrides_are_unset() {
        let tmp = tempfile::tempdir().unwrap();
        let (_lock, _env) = pivot(tmp.path());

        set_env("RA_HOME", "");
        set_env("OCTOS_HOME", "");
        set_env("RA_CONFIG_DIR", "");
        set_env("OCTOS_CONFIG_DIR", "");
        let ctx = resolve_config_context(None);
        assert!(ctx.is_default, "empty overrides must not count as explicit");
    }

    /// An override that names the default state home is NOT explicit: it must
    /// not split config away from the default config home, and it keeps the
    /// legacy migration working for an install that exported that path.
    #[test]
    #[cfg(not(windows))]
    fn override_naming_the_default_state_home_is_not_explicit() {
        let tmp = tempfile::tempdir().unwrap();
        let (_lock, _env) = pivot(tmp.path());
        let default_config = tmp.path().join(".config").join("ra");

        // OCTOS_HOME explicitly set to the legacy default location.
        set_env("OCTOS_HOME", tmp.path().join(".ra").to_str().unwrap());
        let ctx = resolve_config_context(None);
        assert!(ctx.is_default, "OCTOS_HOME==~/.ra must be is_default");
        assert_eq!(ctx.data_dir, tmp.path().join(".ra"));
        assert_eq!(
            ctx.config_home, default_config,
            "OCTOS_HOME==~/.ra must resolve config_home to the brand default (no split-brain)"
        );

        // RA_HOME explicitly set to the new default location.
        remove_env("OCTOS_HOME");
        set_env("RA_HOME", tmp.path().join(".ra").to_str().unwrap());
        let ctx = resolve_config_context(None);
        assert!(ctx.is_default, "RA_HOME==~/.ra must be is_default");
        assert_eq!(ctx.data_dir, tmp.path().join(".ra"));
        assert_eq!(ctx.config_home, default_config);
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

    /// A legacy install keeps both legacy locations: existing `~/.ra` state
    /// and an existing `<config>/ra` config home.
    #[test]
    #[cfg(not(windows))]
    fn existing_legacy_dirs_are_kept() {
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
        set_env("OCTOS_HOME", custom.to_str().unwrap());
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
        set_env("OCTOS_CONFIG_DIR", c.to_str().unwrap());
        let t = tmp.path().join("tenant-data");
        let ctx = resolve_config_context(Some(&t));
        assert!(!ctx.is_default);
        assert_eq!(ctx.data_dir, t);
        assert_eq!(ctx.config_home, c, "config-dir env governs config_home");
        assert_eq!(ctx.auth_home, c, "config-dir env governs auth_home");
    }

    // ── atomic_copy_into ──────────────────────────────────────────────

    #[test]
    fn atomic_copy_into_copies_when_dest_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.json");
        let dest = tmp.path().join("nested").join("dest.json");
        std::fs::write(&src, b"{\"k\":1}").unwrap();

        assert!(atomic_copy_into(&src, &dest, None));
        assert_eq!(std::fs::read(&dest).unwrap(), b"{\"k\":1}");
        // src left intact.
        assert!(src.exists());
    }

    #[test]
    fn atomic_copy_into_is_idempotent_and_nondestructive() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.json");
        let dest = tmp.path().join("dest.json");
        std::fs::write(&src, b"new").unwrap();
        std::fs::write(&dest, b"existing").unwrap();

        // Dest exists → no copy, dest preserved.
        assert!(!atomic_copy_into(&src, &dest, None));
        assert_eq!(std::fs::read(&dest).unwrap(), b"existing");
    }

    #[test]
    fn atomic_copy_into_noop_when_src_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("absent.json");
        let dest = tmp.path().join("dest.json");
        assert!(!atomic_copy_into(&src, &dest, None));
        assert!(!dest.exists());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_copy_into_applies_0600_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.json");
        let dest = tmp.path().join("dest.json");
        // Simulate a world-readable legacy file (0644).
        std::fs::write(&src, b"secret").unwrap();
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert!(atomic_copy_into(&src, &dest, Some(0o600)));
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "dest must be 0600 regardless of src perms");
    }

    // ── run_migrations ────────────────────────────────────────────────

    /// A legacy install's `~/.ra/config.json` is copied into the brand
    /// config home; the legacy file is left intact.
    #[test]
    #[cfg(not(windows))]
    fn config_migration_copies_legacy_state_config_and_leaves_legacy_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let (_lock, _env) = pivot(tmp.path());

        let legacy = tmp.path().join(".ra").join("config.json");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, b"{\"provider\":\"anthropic\"}").unwrap();

        let ctx = resolve_config_context(None);
        assert_eq!(ctx.config_home, tmp.path().join(".config").join("ra"));
        run_migrations(&ctx);

        let dest = ctx.config_home.join("config.json");
        assert!(dest.exists(), "brand config.json must be created");
        assert_eq!(std::fs::read(&dest).unwrap(), b"{\"provider\":\"anthropic\"}");
        assert!(legacy.exists(), "legacy config.json must be left intact");
    }

    /// Migration reads the new state home first when both generations have a
    /// `config.json`.
    #[test]
    #[cfg(not(windows))]
    fn migration_prefers_the_new_state_home() {
        let tmp = tempfile::tempdir().unwrap();
        let (_lock, _env) = pivot(tmp.path());

        for (slug, body) in [(".ra", b"new".as_slice()), (".ra", b"legacy".as_slice())] {
            let path = tmp.path().join(slug).join("config.json");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, body).unwrap();
        }

        let ctx = resolve_config_context(None);
        run_migrations(&ctx);

        let dest = ctx.config_home.join("config.json");
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        assert!(tmp.path().join(".ra").join("config.json").exists());
        assert!(tmp.path().join(".ra").join("config.json").exists());
    }

    /// Legacy `~/.ra/auth.json` (0644) → brand config home auth.json at
    /// 0600; legacy left intact.
    #[cfg(unix)]
    #[test]
    fn auth_migration_produces_0600_and_leaves_legacy_intact() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let (_lock, _env) = pivot(tmp.path());

        let legacy = tmp.path().join(".ra").join("auth.json");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, b"{\"credentials\":{}}").unwrap();
        std::fs::set_permissions(&legacy, std::fs::Permissions::from_mode(0o644)).unwrap();

        let ctx = resolve_config_context(None);
        run_migrations(&ctx);

        let dest = ctx.auth_home.join("auth.json");
        assert!(dest.exists(), "brand auth.json must be created");
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "migrated auth.json must be 0600");
        assert!(legacy.exists(), "legacy auth.json must be left intact");
    }

    /// A tenant-scoped config dir is never a migration destination: host auth
    /// must not be copied into it.
    #[test]
    fn auth_migration_does_not_touch_tenant_config_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let (_lock, _env) = pivot(tmp.path());

        let legacy = tmp.path().join(".ra").join("auth.json");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, b"{\"credentials\":{}}").unwrap();

        let tenant = tmp.path().join("tenant");
        set_env("RA_CONFIG_DIR", tenant.to_str().unwrap());

        let ctx = resolve_config_context(None);
        run_migrations(&ctx);

        assert!(
            !tenant.join("auth.json").exists(),
            "host auth must NOT be migrated into an RA_CONFIG_DIR tenant dir"
        );
        assert!(legacy.exists(), "legacy auth.json must be left intact");
    }

    /// An explicit state home is never a migration source: migrating a
    /// per-profile/-tenant state dir into the global config home would leak
    /// credentials.
    #[test]
    #[cfg(not(windows))]
    fn explicit_state_home_is_not_a_migration_source() {
        let tmp = tempfile::tempdir().unwrap();
        let (_lock, _env) = pivot(tmp.path());

        let custom = tmp.path().join("profile-state");
        std::fs::create_dir_all(&custom).unwrap();
        std::fs::write(custom.join("config.json"), b"{\"k\":1}").unwrap();
        std::fs::write(custom.join("auth.json"), b"{\"credentials\":{}}").unwrap();
        set_env("RA_HOME", custom.to_str().unwrap());

        let ctx = resolve_config_context(None);
        assert!(!ctx.is_default);
        run_migrations(&ctx);

        let global_auth = default_config_home().join("auth.json");
        assert!(
            !global_auth.exists(),
            "an explicit RA_HOME must not be migrated into the global config home"
        );
        assert!(custom.join("auth.json").exists());
        assert!(custom.join("config.json").exists());
    }
}
