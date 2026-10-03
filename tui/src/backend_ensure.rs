//! Resolve the local `ra` server backend for a stdio launch so a bare launch
//! "just works" against a sibling or installed server.
//!
//! This client is a *client*: a local launch spawns `ra serve --stdio` as a
//! child (`--stdio-command`). Before the TUI takes over the terminal, this
//! module resolves a usable backend — in order: a `ra`/`ra.exe` beside the
//! running TUI binary (the normal layout in this repo, where `cargo build`
//! drops both into the same target dir), then `ra` on `PATH`, then the install
//! dir (`~/.ra/bin`, or `$RA_PREFIX`), then a legacy upstream `ra` install
//! (`~/.ra/bin`), which speaks a compatible protocol. The first `Ready`
//! candidate wins; a present-but-too-old one surfaces an "update" error.
//!
//! We do NOT auto-install: this fork's server is built from this repo, so a
//! fully-missing backend is an actionable error (`cargo build --bin ra`, or an
//! explicit `--stdio-command`/`--endpoint`) rather than a brew/npm/GitHub fetch.
//!
//! We resolve against `PATH` and the install dirs without mutating our own
//! process PATH (this crate forbids `unsafe`). When the backend is usable only
//! off-PATH: on Unix we rewrite the stdio command to the full path; on Windows
//! we leave the command bare and the stdio transport prepends the resolved dir
//! to the *child's* PATH (a quoted path in the command string is mangled by
//! `cmd /C`).
//!
//! Scope — it acts on a `Mode::Protocol` launch whose `--stdio-command`'s
//! **leading program** is a bare `ra` (PATH-resolved). Trailing args may carry
//! shell syntax (`--data-dir ~/x`, a Windows `C:\...` path, a pipe): we still
//! probe, since only the *rewrite* to an off-PATH path needs round-trippable
//! syntax — and that rewrite bails to a clear error when it can't. An explicit
//! path, a `PATH=` override, a non-`ra` program, or a user-specified legacy
//! `ra` command is the user's own setup and is left untouched. A backend
//! older than [`MIN_OCTOS_VERSION`] surfaces a clear "please update" error.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cli::{Cli, Mode};
use eyre::{Result, eyre};

/// The minimum `ra` server version this build is known to speak with.
/// octoscode pins `ra-core` (the UI-Protocol crate) by git rev; this is the
/// released server version carrying a compatible protocol. Bump it alongside
/// the pinned `ra-core` rev whenever the protocol surface moves.
pub(crate) const MIN_OCTOS_VERSION: &str = "1.1.0";

/// Set to any value to disable auto-install (a missing backend then errors).
const OPT_OUT_ENV: &str = "OCTOSCODE_NO_AUTO_INSTALL";

/// Pre-rename spelling of [`OPT_OUT_ENV`], still honoured.
///
/// This is the ONLY environment variable the binary reads that was part of the
/// documented `octos-tui` contract (the other ~157 `OCTOS_TUI_*` names belong
/// to the soak harness, and the two `_BIN`/`_DIR` ones are read by our own
/// scripts — all renamed in lockstep). Someone with
/// `OCTOS_TUI_NO_AUTO_INSTALL=1` in a CI job or shell profile would otherwise
/// find auto-install silently switching itself back on, which is exactly the
/// kind of quiet breakage a rename must not cause. Honour it, say so once,
/// and drop it a release or two after the rename has settled.
const OPT_OUT_ENV_LEGACY: &str = "OCTOS_TUI_NO_AUTO_INSTALL";

/// Ensure a usable `ra` backend for a stdio launch, rewriting
/// `cli.stdio_command` to an explicit path when the backend is usable only off
/// `PATH`. Call this BEFORE entering raw mode.
pub fn ensure_octos_backend(cli: &mut Cli) -> Result<()> {
    // Only the protocol backend spawns `ra serve`; `--mode mock` uses the
    // in-process mock and never launches a child (codex).
    if cli.mode != Mode::Protocol {
        return Ok(());
    }
    let Some(command) = cli.stdio_command.clone() else {
        return Ok(()); // WebSocket launch — no local backend to provision.
    };
    let Some(program) = bare_octos_program(&command) else {
        // Explicit path / PATH override / non-ra — the user's own setup, and
        // not something we can safely probe or rewrite.
        return Ok(());
    };

    match resolve_backend(&program)? {
        // Already on PATH — the bare `ra serve` command works as-is.
        Resolved::OnPath => Ok(()),
        // Usable only off-PATH — rewrite the command to launch it directly,
        // since its dir isn't on this process's PATH.
        Resolved::AtPath(ra) => {
            // On Windows, DON'T rewrite the command to an explicit path. The
            // stdio transport spawns via `cmd /C <command>`, and a path embedded
            // in that string — quoted or not — gets mangled by Rust's arg quoting
            // plus cmd's own quirky quote parsing (the child then dies with exit
            // 1). Instead the transport prepends this dir to the child's
            // PATH (see `install_bin_dir` / `shell_command`), so the bare `ra`
            // in the command resolves to the resolved exe (a sibling of this
            // binary, or the install dir). Nothing to rewrite here — `ra` is
            // bound only for the non-Windows path below.
            if cfg!(windows) {
                let _ = &ra;
                return Ok(());
            }
            let rewritten = rewrite_program(&command, &ra).ok_or_else(|| {
                eyre!(
                    "ra is installed at {} but isn't on PATH, and the launch command uses \
                     shell syntax we can't safely rewrite to that path. Add {} to PATH and \
                     relaunch the TUI.",
                    ra.display(),
                    ra
                        .parent()
                        .map(|d| d.display().to_string())
                        .unwrap_or_else(|| ra.display().to_string()),
                )
            })?;
            cli.stdio_command = Some(rewritten);
            Ok(())
        }
    }
}

/// A usable backend, either already on `PATH` or at an explicit path we must
/// launch directly.
enum Resolved {
    OnPath,
    AtPath(PathBuf),
}

/// Outcome of probing one candidate ra.
enum Probe {
    /// Runs and is at least [`MIN_OCTOS_VERSION`].
    Ready,
    /// Runs but is older (carries the found version).
    Outdated(String),
    /// Not found.
    Missing,
}

fn opted_out() -> bool {
    match opt_out_from(
        std::env::var_os(OPT_OUT_ENV),
        std::env::var_os(OPT_OUT_ENV_LEGACY),
    ) {
        OptOut::No => false,
        OptOut::Current => true,
        OptOut::Legacy => {
            // Warn once per process, not per probe — `opted_out` is called
            // from several paths and a repeated notice would bury the real
            // output.
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                eprintln!(
                    "octoscode: {OPT_OUT_ENV_LEGACY} is deprecated — \
                     rename it to {OPT_OUT_ENV}. Still honoured for now."
                );
            });
            true
        }
    }
}

/// Which spelling (if either) opted out.
#[derive(Debug, PartialEq, Eq)]
enum OptOut {
    No,
    Current,
    /// Only the pre-rename name was set — honour it, but say so.
    Legacy,
}

/// Pure resolver behind [`opted_out`]: the current name wins, the pre-rename
/// name still counts, and empty values are ignored (matching the original
/// `!v.is_empty()` semantics — `FOO=` is not "set"). Split out so the
/// back-compat is testable without mutating process env (`std::env::set_var`
/// is `unsafe` under edition 2024 + `unsafe_code = deny`).
fn opt_out_from(current: Option<std::ffi::OsString>, legacy: Option<std::ffi::OsString>) -> OptOut {
    if current.is_some_and(|v| !v.is_empty()) {
        return OptOut::Current;
    }
    if legacy.is_some_and(|v| !v.is_empty()) {
        return OptOut::Legacy;
    }
    OptOut::No
}

/// Find a usable backend for the bare `ra` stdio command. Candidates are tried
/// in order — a `ra`/`ra.exe` beside this binary (the repo dev layout), then
/// `ra` on `PATH`, then the install dir (`~/.ra/bin`), then a legacy upstream
/// `ra` install (`~/.ra/bin`) — and the first `Ready` wins. If nothing is
/// `Ready` but a candidate exists and is too old, guide an update; otherwise
/// error with the fix (no upstream install is attempted).
fn resolve_backend(program: &str) -> Result<Resolved> {
    let mut outdated: Option<String> = None;

    // (a) Sibling of the running TUI binary — explicit path.
    if let Some(sibling) = sibling_backend() {
        match probe(&sibling) {
            Probe::Ready => return Ok(Resolved::AtPath(sibling)),
            Probe::Outdated(found) => outdated = Some(found),
            Probe::Missing => {}
        }
    }
    // (b) PATH (bare `ra`).
    match probe(Path::new(program)) {
        Probe::Ready => return Ok(Resolved::OnPath),
        Probe::Outdated(found) => outdated = outdated.or(Some(found)),
        Probe::Missing => {}
    }
    // (c) Install dir.
    if let Some(exe) = install_dir_backend() {
        match probe(&exe) {
            Probe::Ready => return Ok(Resolved::AtPath(exe)),
            Probe::Outdated(found) => outdated = outdated.or(Some(found)),
            Probe::Missing => {}
        }
    }
    // (d) Legacy upstream ra install — protocol-compatible, accepted.
    if let Some(exe) = legacy_install_dir_octos() {
        match probe(&exe) {
            Probe::Ready => return Ok(Resolved::AtPath(exe)),
            Probe::Outdated(found) => outdated = outdated.or(Some(found)),
            Probe::Missing => {}
        }
    }

    if let Some(found) = outdated {
        return Err(outdated_error(&found));
    }

    // Missing everywhere → actionable error; never install upstream.
    if opted_out() {
        return Err(backend_missing_error());
    }
    run_installer()?;
    unreachable!("run_installer only ever returns the actionable 'no backend' error")
}

/// The actionable "no backend" error. No upstream install is attempted: this
/// fork's server is built from this repo.
fn backend_missing_error() -> eyre::Report {
    eyre!(
        "no ra backend found: looked for a ra/ra.exe beside this binary, on PATH, in {}, \
         and for a legacy ra in {}. Build this repo's server (`cargo build --bin ra`) \
         or pass an explicit `--stdio-command` (or `--endpoint` for a running server).",
        install_dir_backend()
            .and_then(|p| p.parent().map(|d| d.display().to_string()))
            .unwrap_or_else(|| "~/.ra/bin".to_owned()),
        legacy_install_dir_octos()
            .and_then(|p| p.parent().map(|d| d.display().to_string()))
            .unwrap_or_else(|| "~/.ra/bin".to_owned()),
    )
}

fn outdated_error(found: &str) -> eyre::Report {
    eyre!(
        "backend {found} is older than the {MIN_OCTOS_VERSION} this client needs. \
         Build the current server (`cargo build --bin ra`) and relaunch, or point \
         --endpoint at a newer server."
    )
}

/// Run `<candidate> --version` and classify it. `ra` (bare) resolves through
/// PATH; a full path probes that file. A present-but-unparseable/erroring
/// binary counts as Ready — don't fight a backend the user clearly has.
///
/// On Windows a bare name may be a `PATHEXT` shim (`.cmd`/`.ps1`, as an npm
/// install ships `ra`) that a direct spawn — which finds only `.exe` —
/// misses, while the stdio transport's `cmd /C` resolves it. We mirror that
/// resolution via `where` so probing classifies the *same* binary the real
/// launch will run, including when an older ra shadows a newer one (codex).
fn probe(candidate: &Path) -> Probe {
    let is_bare = {
        let s = candidate.to_string_lossy();
        !s.contains('/') && !s.contains('\\')
    };
    let output = if cfg!(windows) && is_bare {
        match where_first(candidate) {
            Some(shim) => Command::new("cmd")
                .arg("/C")
                .arg(&shim)
                .arg("--version")
                .output(),
            None => return Probe::Missing,
        }
    } else {
        Command::new(candidate).arg("--version").output()
    };
    match output {
        Ok(output) if output.status.success() => {
            match parse_octos_version(&String::from_utf8_lossy(&output.stdout)) {
                Some(found) if version_lt(&found, MIN_OCTOS_VERSION) => Probe::Outdated(found),
                _ => Probe::Ready,
            }
        }
        Ok(_) => Probe::Ready,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Probe::Missing,
        // Any other spawn error (permissions, etc.): assume present and let the
        // real launch surface a precise error rather than triggering an install.
        Err(_) => Probe::Ready,
    }
}

/// The first path `where <name>` resolves on Windows — the same PATH+PATHEXT
/// order `cmd /C` (the stdio transport) uses — or `None` when it isn't found.
/// Windows-only; on other platforms `probe`/`have` never call it.
fn where_first(name: &Path) -> Option<PathBuf> {
    let out = Command::new("where").arg(name).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(PathBuf::from)
}

/// The ra binary this fork installs: `$RA_PREFIX/ra` or `~/.ra/bin/ra`
/// (`ra.exe` on Windows). `None` if no home dir.
fn install_dir_backend() -> Option<PathBuf> {
    let dir = match std::env::var_os("RA_PREFIX") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => home_dir()?.join(".ra").join("bin"),
    };
    Some(dir.join(backend_binary_name()))
}

/// A legacy upstream `ra` server install: `$OCTOS_PREFIX/ra` or
/// `~/.ra/bin/ra` (`ra.exe` on Windows). It speaks a compatible
/// protocol, so it is accepted as a fallback backend. `None` if no home dir.
fn legacy_install_dir_octos() -> Option<PathBuf> {
    let dir = match std::env::var_os("OCTOS_PREFIX") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => home_dir()?.join(".ra").join("bin"),
    };
    let name = if cfg!(windows) { "ra.exe" } else { "ra" };
    Some(dir.join(name))
}

/// A `ra`/`ra.exe` sitting beside the running TUI binary — the normal dev
/// layout in this repo, where `cargo build` drops both into the same target
/// dir. `None` if the running exe path can't be resolved.
fn sibling_backend() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join(backend_binary_name()))
}

/// The directory the resolved backend actually lives in: the sibling dir of
/// the running TUI binary when a `ra`/`ra.exe` sits there, else the install dir
/// (`$RA_PREFIX` or `~/.ra/bin`). The stdio transport prepends this to the
/// child's PATH so a bare `ra` in the launch command resolves to the right exe
/// — without embedding a path in the command string, which `cmd /C` mangles on
/// Windows. `None` if no sibling and no home dir.
pub(crate) fn install_bin_dir() -> Option<PathBuf> {
    if let Some(sibling) = sibling_backend() {
        if sibling.exists() {
            return sibling.parent().map(Path::to_path_buf);
        }
    }
    install_dir_backend().and_then(|exe| exe.parent().map(Path::to_path_buf))
}

/// Home directory, treating an empty `HOME` as absent so the Windows
/// `USERPROFILE` fallback still applies (codex).
fn home_dir() -> Option<PathBuf> {
    let non_empty = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
    non_empty("HOME")
        .or_else(|| non_empty("USERPROFILE"))
        .map(PathBuf::from)
}

/// The program a `--stdio-command` runs, IFF it is a **bare** `ra` (no path
/// separator) resolved through `PATH`. Handles a leading `env` + `VAR=value`
/// assignments and an optional `stdio:` transport-label prefix. This decides
/// only whether we may *probe* the backend, so it inspects just the
/// leading executable — trailing args carrying shell syntax (a `--data-dir
/// ~/x`, a pipe, or a Windows `C:\...` path) must NOT disqualify provisioning
/// (codex); round-trip safety is enforced separately, at the rewrite step.
/// Returns `None` for an explicit path, a `PATH=` override (the child would
/// resolve `ra` against a different search path than we probe), or a
/// non-`ra` program (a user-specified legacy `ra` command is left to the
/// user).
fn bare_octos_program(command: &str) -> Option<String> {
    let command = command.trim();
    let command = command.strip_prefix("stdio:").unwrap_or(command).trim();
    // Split on whitespace to find the leading executable. We deliberately do
    // NOT shlex-parse here: we need only the program token, and an unquoted
    // Windows path arg (`--data-dir C:\Users\x`) would trip POSIX backslash
    // escaping and drop the token entirely.
    let mut iter = command.split_whitespace();
    let mut program = iter.next()?;
    if program == "env" {
        program = iter.next()?;
    }
    // Skip `KEY=value` assignments before the program. A `PATH=` override means
    // the child resolves `ra` against a different search path than we can
    // probe from this process — treat the whole command as user-managed.
    while is_env_assignment(program) {
        if program.split_once('=').is_some_and(|(k, _)| k == "PATH") {
            return None;
        }
        program = iter.next()?;
    }
    if program.contains('/') || program.contains('\\') {
        return None; // explicit path — user's own setup
    }
    // Only a bare `ra` is our canonical, provisionable form. We deliberately do
    // NOT accept `ra.exe`: it's never the canonical command (bare `ra` is, and
    // Windows `cmd /C` resolves it to whatever `.exe` exists). A user-specified
    // `ra` command is a legacy upstream client the user manages themselves —
    // recognised as-is and never provisioned/rewritten.
    (program == "ra").then(|| program.to_owned())
}

/// Shell metacharacters whose presence means split+rejoin (and `sh -c`
/// re-parsing) would not faithfully preserve the command.
const SHELL_METACHARS: &[char] = &[
    '$', '`', '|', '&', ';', '<', '>', '(', ')', '*', '?', '[', ']', '{', '}', '~', '!', '\\',
    '\n', '\r',
];

/// `KEY=value` with a non-empty, path-free key (so `/opt/x=y` or a bare program
/// isn't mistaken for an assignment).
fn is_env_assignment(token: &str) -> bool {
    token
        .split_once('=')
        .is_some_and(|(k, _)| !k.is_empty() && !k.contains('/') && !k.contains('\\'))
}

/// Rewrite a bare-`ra` stdio command to launch `octos_path` explicitly,
/// preserving a leading `stdio:` prefix, an `env` prefix, `KEY=value`
/// assignments, and all trailing args. Returns `None` — so the caller surfaces
/// an actionable "add ra to PATH" error instead of a mangled command — when
/// the command carries shell syntax the split+rejoin round-trip can't preserve
/// (`$PWD` would become a literal, a `~` would stop expanding, a pipe would be
/// quoted into an argument). Unix-only in practice: the Windows caller errors
/// before reaching here, since `cmd /C` won't honor this POSIX quoting anyway.
fn rewrite_program(command: &str, octos_path: &Path) -> Option<String> {
    let trimmed = command.trim();
    let (prefix, body) = match trimmed.strip_prefix("stdio:") {
        Some(rest) => ("stdio:", rest.trim()),
        None => ("", trimmed),
    };
    if body.contains(SHELL_METACHARS) {
        return None;
    }
    let mut tokens = shlex::split(body)?;
    let has_env_keyword = tokens.first().is_some_and(|t| t == "env");
    let mut idx = usize::from(has_env_keyword);
    let assignments_start = idx;
    while tokens.get(idx).is_some_and(|t| is_env_assignment(t)) {
        idx += 1;
    }
    *tokens.get_mut(idx)? = octos_path.to_string_lossy().into_owned();
    // A DIRECT `VAR=value` prefix (no leading `env`) is fine as typed, but
    // `try_join` re-quotes it (`'VAR=value'`), and `sh -c` then treats the
    // quoted token as the *command name* rather than an assignment — so the
    // backend never launches. Prepend `env` so the (re-quoted) assignments are
    // parsed as `env`'s own args instead (codex). A leading `env` already does
    // this; no assignments → nothing to protect.
    if !has_env_keyword && idx > assignments_start {
        tokens.insert(0, "env".to_owned());
    }
    let joined = shlex::try_join(tokens.iter().map(String::as_str)).ok()?;
    Some(format!("{prefix}{joined}"))
}

/// Pull the first `X.Y.Z` token out of a `--version` line, e.g.
/// `ra 1.1.0 (79c19f6d4 2026-07-11)` → `1.1.0`. `ra --version` prints
/// `ra 2.0.3-rc.13 (…)`, so a leading `v` and any `-pre`/`+build` suffix are
/// stripped before reading the leading `X.Y.Z` core.
fn parse_octos_version(output: &str) -> Option<String> {
    output.split_whitespace().find_map(|tok| {
        let core = tok.trim_start_matches('v');
        let core = core.split(['-', '+']).next().unwrap_or(core);
        let mut parts = core.split('.');
        let ok = [parts.next(), parts.next(), parts.next()]
            .iter()
            .all(|p| p.is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())))
            && parts.next().is_none();
        ok.then(|| core.to_owned())
    })
}

/// `a < b` for dotted numeric versions (`1.2.0 < 1.10.0`). Unparseable segments
/// compare as 0.
fn version_lt(a: &str, b: &str) -> bool {
    let nums = |s: &str| -> Vec<u64> { s.split('.').map(|p| p.parse().unwrap_or(0)).collect() };
    let (a, b) = (nums(a), nums(b));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x < y;
        }
    }
    false
}

/// No upstream install: this fork's server is built from this repo, so a
/// missing backend is an actionable error (`cargo build --bin ra`, or an
/// explicit `--stdio-command`/`--endpoint`) instead of a brew/npm/GitHub fetch.
/// Kept as a named function so the "no backend" path reads the same here and in
/// `resolve_backend`.
fn run_installer() -> Result<()> {
    Err(backend_missing_error())
}

/// The ra/ra **server release this client targets** — the tag whose bundle
/// carries the exact `ra-core` protocol this client pins (see the `ra-core` rev
/// in Cargo.toml). Surfaced by `doctor` as the server version to run against.
///
/// **BUMP THIS whenever you bump the `ra-core` rev in Cargo.toml**, to the
/// release tag that contains that rev. [`REQUIRED_OCTOS_CORE_REV`] and the test
/// beside it make the pair checkable: the rev moved to v2.0.3-rc.1 while this
/// stayed on v2.0.2, so the tag and the pinned rev must move together.
pub(crate) const REQUIRED_OCTOS_RELEASE: &str = "v2.0.3-rc.12";

/// The `ra-core` rev that [`REQUIRED_OCTOS_RELEASE`] resolves to — i.e. the
/// commit the release tag points at, and the rev Cargo.toml must pin.
///
/// These are two halves of one decision (which server protocol this client
/// speaks) held in two files, with only a doc comment joining them. Recording
/// the rev here lets `octos_release_pin_matches_cargo_core_rev` fail when they
/// disagree, so bumping Cargo.toml without revisiting the release tag is caught
/// at test time rather than by a user running a mismatched server.
///
/// Test-only: its whole job is to be compared against Cargo.toml, so it would
/// be dead weight in a real build.
#[cfg(test)]
pub(crate) const REQUIRED_OCTOS_CORE_REV: &str = "f4a31d9a0ef2228e919c3d516b78a9d82ad6f470";
/// The backend binary filename on this platform.
fn backend_binary_name() -> &'static str {
    if cfg!(windows) { "ra.exe" } else { "ra" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_bin_dir_is_the_dir_the_backend_lives_in() {
        // `install_bin_dir` (used by the transport to augment the child PATH)
        // must be the sibling dir when a `ra` sits beside this binary, else the
        // install dir.
        let dir = install_bin_dir();
        let sibling = sibling_backend();
        let install = install_dir_backend();
        match (dir, sibling, install) {
            (Some(dir), Some(sib), _) if sib.exists() => {
                assert_eq!(Some(dir.as_path()), sib.parent())
            }
            (Some(dir), _, Some(exe)) => assert_eq!(Some(dir.as_path()), exe.parent()),
            (None, _, None) => {} // no HOME/USERPROFILE — install dir absent
            other => panic!("unexpected install_bin_dir resolution: {other:?}"),
        }
    }

    #[test]
    fn bare_octos_program_matches_the_standard_shapes() {
        for cmd in [
            "ra serve --stdio --solo",
            "  ra serve --stdio  ",
            "stdio:ra serve --stdio",
            "env RA_FOO=1 DEEPSEEK_API_KEY=sk ra serve --stdio",
            "FOO=1 ra serve",
            // Shell syntax in *arguments* must NOT disqualify provisioning — we
            // only need the leading program to probe (codex).
            "ra serve --stdio --solo --data-dir ~/.ra-data",
            "ra serve --stdio --data-dir C:\\Users\\admin\\data",
            "ra serve --stdio | tee log",
            "ra serve && echo done",
            "RA_HOME=\"$PWD/.ra\" ra serve",
        ] {
            assert_eq!(
                bare_octos_program(cmd).as_deref(),
                Some("ra"),
                "should extract bare ra from: {cmd}"
            );
        }
    }

    #[test]
    fn bare_octos_program_skips_explicit_paths_shell_syntax_and_others() {
        for cmd in [
            "/usr/local/bin/ra serve --stdio", // explicit path — user-managed
            "$HOME/.local/bin/ra serve --stdio", // path (leading program) — user-managed
            "./ra serve",                      // explicit path
            "my-custom-backend --stdio",          // not ra
            "env A=1 my-backend serve",           // not ra
            "ra.exe serve --stdio",               // not canonical; bare `ra` is
            "ra serve --stdio",                // legacy upstream — user-managed
            "env PATH=/custom/bin:$PATH ra serve", // PATH override — can't probe same ra
            "PATH=/opt/ra/bin ra serve",          // leading PATH override
        ] {
            assert_eq!(
                bare_octos_program(cmd),
                None,
                "should NOT auto-manage: {cmd}"
            );
        }
    }

    #[test]
    fn rewrite_program_swaps_the_octos_token_only() {
        let p = Path::new("/home/u/.ra/bin/ra");
        assert_eq!(
            rewrite_program("ra serve --stdio --solo", p).as_deref(),
            Some("/home/u/.ra/bin/ra serve --stdio --solo")
        );
        // env prefix + assignment preserved (shlex may re-quote `A=1`, which is
        // shell-equivalent since the command is re-parsed by `sh -c`).
        let rewritten = rewrite_program("env A=1 ra serve --stdio", p).unwrap();
        assert_eq!(
            shlex::split(&rewritten).unwrap(),
            ["env", "A=1", "/home/u/.ra/bin/ra", "serve", "--stdio"]
        );
        assert_eq!(
            rewrite_program("stdio:ra serve", p).as_deref(),
            Some("stdio:/home/u/.ra/bin/ra serve")
        );
        // A path containing a space is re-quoted so it stays one arg.
        let spaced = Path::new("/home/a b/.ra/bin/ra");
        assert_eq!(
            rewrite_program("ra serve", spaced).as_deref(),
            Some("'/home/a b/.ra/bin/ra' serve")
        );
        // A DIRECT assignment prefix (no `env` keyword) gains one, so `sh -c`
        // keeps it an assignment instead of reading the re-quoted token as a
        // command name (codex).
        let rewritten = rewrite_program("OCTOS_HOME=/data ra serve", p).unwrap();
        assert_eq!(
            shlex::split(&rewritten).unwrap(),
            [
                "env",
                "OCTOS_HOME=/data",
                "/home/u/.ra/bin/ra",
                "serve"
            ]
        );
        // Shell syntax we can't round-trip → None, so the caller errors with an
        // "add ra to PATH" message rather than emitting a mangled command.
        for cmd in [
            "ra serve --data-dir ~/data",          // ~ would stop expanding
            "OCTOS_HOME=\"$PWD/.ra\" ra serve", // $PWD would become literal
            "ra serve | tee log",                  // pipe quoted into an argument
            "ra serve && echo done",               // control operator
        ] {
            assert_eq!(
                rewrite_program(cmd, p),
                None,
                "should refuse to rewrite: {cmd}"
            );
        }
    }

    #[test]
    fn parse_octos_version_extracts_semver() {
        assert_eq!(
            parse_octos_version("ra 1.1.0 (79c19f6d4 2026-07-11)").as_deref(),
            Some("1.1.0")
        );
        assert_eq!(
            parse_octos_version("ra v2.10.3\n").as_deref(),
            Some("2.10.3")
        );
        assert_eq!(parse_octos_version("no version here"), None);
        assert_eq!(parse_octos_version("ra 1.2.3.4"), None); // 4-part isn't X.Y.Z
        // `ra --version` prints a prerelease; the leading X.Y.Z must still read.
        assert_eq!(
            parse_octos_version("ra 2.0.3-rc.13 (dde7655 2026-10-03)").as_deref(),
            Some("2.0.3")
        );
    }

    #[test]
    fn version_lt_is_numeric_not_lexical() {
        assert!(version_lt("1.1.0", "1.2.0"));
        assert!(version_lt("1.2.0", "1.10.0")); // NOT lexical ("2" < "10")
        assert!(version_lt("0.9.9", "1.0.0"));
        assert!(!version_lt("1.1.0", "1.1.0"));
        assert!(!version_lt("2.0.0", "1.9.9"));
        assert!(!version_lt("1.1.0", "1.1")); // 1.1.0 == 1.1(.0)
    }

    /// The rename must not silently re-enable auto-install for anyone who set
    /// the opt-out under the old name. This is the ONE documented env var the
    /// binary reads that predates the rename.
    #[test]
    fn legacy_opt_out_env_is_still_honoured() {
        use std::ffi::OsString;
        let set = |v: &str| Some(OsString::from(v));

        assert_eq!(opt_out_from(None, None), OptOut::No, "neither set");
        assert_eq!(
            opt_out_from(set("1"), None),
            OptOut::Current,
            "current name opts out"
        );
        assert_eq!(
            opt_out_from(None, set("1")),
            OptOut::Legacy,
            "pre-rename name must STILL opt out, or a CI job that set it \
             silently gets auto-install back"
        );
        assert_eq!(
            opt_out_from(set("1"), set("1")),
            OptOut::Current,
            "current name wins so the deprecation notice stays quiet"
        );

        // Empty is not "set" — preserves the original `!v.is_empty()` rule.
        assert_eq!(opt_out_from(set(""), None), OptOut::No, "empty current");
        assert_eq!(opt_out_from(None, set("")), OptOut::No, "empty legacy");
        assert_eq!(
            opt_out_from(set(""), set("1")),
            OptOut::Legacy,
            "empty current falls through to a real legacy value"
        );
    }

    /// The ra-core rev in Cargo.toml and the release tag we auto-provision
    /// are two halves of ONE decision — which server protocol this client
    /// speaks — living in two files, joined only by a doc comment saying "bump
    /// this too". That drifted: the rev reached v2.0.3-rc.1 while
    /// REQUIRED_OCTOS_RELEASE stayed at v2.0.2, so a fresh install provisioned
    /// a server older than the protocol the client had been built against.
    ///
    /// Reading Cargo.toml at test time turns the comment into a check.
    #[test]
    fn octos_release_pin_matches_cargo_core_rev() {
        let manifest = include_str!("../Cargo.toml");
        // The kernel now lives in this repo, so `ra-core` is a path dependency;
        // the protocol rev it corresponds to is recorded in the comment beside
        // it (the upstream pin). Read that rev back so the pair stays checkable.
        let rev = manifest
            .lines()
            .find_map(|l| {
                let idx = l.find("rev = \"")?;
                l[idx + "rev = \"".len()..].split('"').next()
            })
            .expect("Cargo.toml records the ra-core protocol rev");

        assert_eq!(
            rev, REQUIRED_OCTOS_CORE_REV,
            "Cargo.toml records ra-core rev {rev}, but backend_ensure records \
             {REQUIRED_OCTOS_CORE_REV} as the rev behind \
             REQUIRED_OCTOS_RELEASE ({REQUIRED_OCTOS_RELEASE}).\n\
             \n\
             If you bumped the rev, also bump REQUIRED_OCTOS_RELEASE to the \
             release tag containing it, and update \
             REQUIRED_OCTOS_CORE_REV to match. Otherwise the recorded protocol \
             revision disagrees with this client."
        );
    }

    /// A release tag, not a bare version — the auto-installer builds download
    /// URLs from it (`releases/download/<tag>/…`).
    #[test]
    fn octos_release_pin_is_a_tag() {
        assert!(
            REQUIRED_OCTOS_RELEASE.starts_with('v'),
            "REQUIRED_OCTOS_RELEASE must be a tag like `v2.0.3-rc.1`, got {REQUIRED_OCTOS_RELEASE}"
        );
    }
}
