//! Resolve the local `ra` server backend for a stdio launch so a bare launch
//! "just works" against a sibling or installed server.
//!
//! This client is a *client*: a local launch spawns `ra serve --stdio` as a
//! child (`--stdio-command`). Before the TUI takes over the terminal, this
//! module resolves a usable backend — in order: a `ra`/`ra.exe` beside the
//! running TUI binary (the normal layout in this repo, where `cargo build`
//! drops both into the same target dir), then `ra` on `PATH`, then the install
//! dir (`~/.ra/bin`, or `$RA_PREFIX`). The first `Ready` candidate wins; a
//! present-but-too-old one surfaces an "update" error.
//!
//! We do NOT auto-install: this fork's server is built from this repo, so a
//! fully-missing backend is an actionable error (`cargo build --bin ra`, or an
//! explicit `--stdio-command`/`--endpoint`) rather than a brew/npm/GitHub fetch.
//!
//! We resolve against `PATH` and the install dirs without mutating our own
//! process PATH (this crate forbids `unsafe`). When the backend is usable only
//! off-PATH: on Unix we rewrite the stdio command to the full path; on Windows
//! we never embed a path (a quoted path in the command string is mangled by
//! `cmd /C`) — if the resolved binary's name differs from the command's program
//! token we rewrite just that token, and the transport prepends the resolved
//! dir to the *child's* PATH. The prepend is a function of the chosen
//! resolution, so a rejected (e.g. outdated) candidate can never be prepended
//! back into the launch.
//!
//! Scope — it acts on a `Mode::Protocol` launch whose `--stdio-command`'s
//! **leading program** is a bare `ra` (PATH-resolved). Trailing args may carry
//! shell syntax (`--data-dir ~/x`, a Windows `C:\...` path, a pipe): we still
//! probe, since only the *rewrite* to an off-PATH path needs round-trippable
//! syntax — and that rewrite bails to a clear error when it can't. An explicit
//! path, a `PATH=` override, or a non-`ra` program is the user's own setup and
//! is left untouched. A backend older than [`MIN_BACKEND_VERSION`] surfaces a
//! clear "please update" error.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cli::{Cli, Mode};
use eyre::{Result, eyre};

/// The minimum `ra` server version this build is known to speak with.
/// ra-tui pins `ra-core` (the UI-Protocol crate) by git rev; this is the
/// released server version carrying a compatible protocol. Bump it alongside
/// the pinned `ra-core` rev whenever the protocol surface moves.
pub(crate) const MIN_BACKEND_VERSION: &str = "0.1.0";

/// Set to any value to disable auto-install (a missing backend then errors).
const OPT_OUT_ENV: &str = "RA_TUI_NO_AUTO_INSTALL";

/// Pre-rename spelling of [`OPT_OUT_ENV`], still honoured.
///
/// This is the ONLY environment variable the binary reads that was part of the
/// documented `ra-tui` contract (the other ~157 `ra_TUI_*` names belong
/// to the soak harness, and the two `_BIN`/`_DIR` ones are read by our own
/// scripts — all renamed in lockstep). Someone with
/// `RA_TUI_NO_AUTO_INSTALL=1` in a CI job or shell profile would otherwise
/// find auto-install silently switching itself back on, which is exactly the
/// kind of quiet breakage a rename must not cause. Honour it, say so once,
/// and drop it a release or two after the rename has settled.
const OPT_OUT_ENV_LEGACY: &str = "RA_TUI_NO_AUTO_INSTALL";

/// Ensure a usable `ra` backend for a stdio launch, rewriting
/// `cli.stdio_command` to an explicit path when the backend is usable only off
/// `PATH`. Call this BEFORE entering raw mode.
pub fn ensure_ra_backend(cli: &mut Cli) -> Result<()> {
    // Only the protocol backend spawns `ra serve`; `--mode mock` uses the
    // in-process mock and never launches a child (codex).
    if cli.mode != Mode::Protocol {
        return Ok(());
    }
    let Some(command) = cli.stdio_command.clone() else {
        return Ok(()); // WebSocket launch — no local backend to provision.
    };
    let Some(program) = bare_ra_program(&command) else {
        // Explicit path / PATH override / non-ra — the user's own setup, and
        // not something we can safely probe or rewrite.
        return Ok(());
    };

    let resolved = resolve_backend(&program)?;
    // The child PATH prepend is a function of the chosen resolution: nothing for
    // a PATH hit, the binary's dir for an explicit one — so an outdated sibling
    // that lost to a PATH `ra` can never be prepended back into the launch.
    cli.backend_prepend = child_path_prepend(&resolved);

    match resolved {
        // Already on PATH — the bare `ra serve` command works as-is.
        Resolved::OnPath => Ok(()),
        // Usable only off-PATH — make the launch find it without embedding a
        // path where that is unsafe.
        Resolved::AtPath(bin) => {
            if cfg!(windows) {
                // `cmd /C` mangles a path embedded in the command string, so we
                // never do that. If the resolved binary's name differs from the
                // command's program token, rewrite just that token; the
                // transport prepends the binary's dir to the child's PATH (see
                // `child_path_prepend` / `shell_command`) so the bare token
                // resolves to it.
                if let Some(rewritten) = windows_command_for(&command, &bin) {
                    cli.stdio_command = Some(rewritten);
                }
                Ok(())
            } else {
                let rewritten = rewrite_program(&command, &bin).ok_or_else(|| {
                    eyre!(
                        "ra is installed at {} but isn't on PATH, and the launch command uses \
                         shell syntax we can't safely rewrite to that path. Add {} to PATH and \
                         relaunch the TUI.",
                        bin.display(),
                        bin.parent()
                            .map(|d| d.display().to_string())
                            .unwrap_or_else(|| bin.display().to_string()),
                    )
                })?;
                cli.stdio_command = Some(rewritten);
                Ok(())
            }
        }
    }
}

/// A usable backend, either already on `PATH` or at an explicit path we must
/// launch directly.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Resolved {
    OnPath,
    AtPath(PathBuf),
}

/// Outcome of probing one candidate ra.
enum Probe {
    /// Runs and is at least [`MIN_BACKEND_VERSION`].
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
                    "ra-tui: {OPT_OUT_ENV_LEGACY} is deprecated — \
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

/// Which candidate slot a backend path came from, in resolution precedence.
/// Shared with `doctor` so the reported set can't drift from what launches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CandidateKind {
    /// A `ra`/`ra.exe` beside the running ra-tui binary.
    Sibling,
    /// Bare `ra` resolved through `PATH`.
    Path,
    /// The install dir (`$RA_PREFIX`/`$RA_PREFIX` or `~/.ra/bin`).
    InstallDir,
}

impl CandidateKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Sibling => "sibling of ra-tui",
            Self::Path => "PATH",
            Self::InstallDir => "ra install dir",
        }
    }
}

/// The backend candidates in resolution precedence. Shared by
/// [`resolve_backend`] and `doctor` so the two can't disagree about the set.
pub(crate) fn backend_candidates(program: &str) -> Vec<(PathBuf, CandidateKind)> {
    let mut candidates = Vec::new();
    if let Some(sibling) = sibling_backend() {
        candidates.push((sibling, CandidateKind::Sibling));
    }
    candidates.push((PathBuf::from(program), CandidateKind::Path));
    if let Some(exe) = install_dir_backend() {
        candidates.push((exe, CandidateKind::InstallDir));
    }
    candidates
}

/// Outcome of scanning the candidate list — pure, with the probe injected so
/// the precedence rules are testable without spawning real binaries.
enum Choice {
    Resolved(Resolved),
    Outdated(String),
    Missing,
}

fn choose_backend(
    candidates: &[(PathBuf, CandidateKind)],
    probe: impl Fn(&Path) -> Probe,
) -> Choice {
    let mut outdated: Option<String> = None;
    for (path, kind) in candidates {
        match probe(path) {
            Probe::Ready => {
                return Choice::Resolved(match kind {
                    CandidateKind::Path => Resolved::OnPath,
                    _ => Resolved::AtPath(path.clone()),
                });
            }
            Probe::Outdated(found) => {
                if outdated.is_none() {
                    outdated = Some(found.clone());
                }
            }
            Probe::Missing => {}
        }
    }
    match outdated {
        Some(found) => Choice::Outdated(found),
        None => Choice::Missing,
    }
}

/// Find a usable backend for the bare `ra` stdio command. Candidates are tried
/// in [`backend_candidates`] order and the first `Ready` wins. If nothing is
/// `Ready` but a candidate exists and is too old, guide an update; otherwise
/// error with the fix (no upstream install is attempted).
fn resolve_backend(program: &str) -> Result<Resolved> {
    match choose_backend(&backend_candidates(program), probe) {
        Choice::Resolved(resolved) => Ok(resolved),
        Choice::Outdated(found) => Err(outdated_error(&found)),
        Choice::Missing => {
            // Missing everywhere → actionable error; never install upstream.
            if opted_out() {
                return Err(backend_missing_error());
            }
            run_installer()?;
            unreachable!("run_installer only ever returns the actionable 'no backend' error")
        }
    }
}

/// The backend `doctor` should report: the first `Ready` candidate and which
/// slot it came from, or `None` when nothing is `Ready`. Same order as
/// [`resolve_backend`], so the report matches what a launch would pick.
pub(crate) fn resolved_backend_report() -> Option<(PathBuf, &'static str)> {
    backend_candidates("ra")
        .into_iter()
        .find(|(path, _)| matches!(probe(path), Probe::Ready))
        .map(|(path, kind)| (path, kind.label()))
}

/// The actionable "no backend" error. No upstream install is attempted: this
/// fork's server is built from this repo.
fn backend_missing_error() -> eyre::Report {
    eyre!(
        "no ra backend found: looked for a ra/ra.exe beside this binary, on PATH, and in {}. \
         Build this repo's server (`cargo build --bin ra`) or pass an explicit \
         `--stdio-command` (or `--endpoint` for a running server).",
        install_dir_backend()
            .and_then(|p| p.parent().map(|d| d.display().to_string()))
            .unwrap_or_else(|| "~/.ra/bin".to_owned()),
    )
}

fn outdated_error(found: &str) -> eyre::Report {
    eyre!(
        "backend {found} is older than the {MIN_BACKEND_VERSION} this client needs. \
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
/// launch will run, including when an older RecurAgent shadows a newer one (codex).
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
            match parse_ra_version(&String::from_utf8_lossy(&output.stdout)) {
                Some(found) if version_lt(&found, MIN_BACKEND_VERSION) => Probe::Outdated(found),
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

/// The RecurAgent binary this fork installs: `$RA_PREFIX/ra` (or the legacy
/// `$RA_PREFIX` env fallback) or `~/.ra/bin/ra` (`ra.exe` on Windows).
/// `None` if no home dir.
fn install_dir_backend() -> Option<PathBuf> {
    let dir = match crate::env::env_os_compat("RA_PREFIX", "RA_PREFIX") {
        Some(p) => PathBuf::from(p),
        _ => home_dir()?.join(".ra").join("bin"),
    };
    Some(dir.join(backend_binary_name()))
}

/// A `ra`/`ra.exe` sitting beside the running TUI binary — the normal dev
/// layout in this repo, where `cargo build` drops both into the same target
/// dir. `None` if the running exe path can't be resolved.
fn sibling_backend() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join(backend_binary_name()))
}

/// The directory to prepend to the *child's* PATH so the launch command's bare
/// program token resolves to the *resolved* backend: nothing for a `PATH`
/// resolution (the child's own PATH already finds it), the binary's directory
/// for an explicit one. A pure function of the chosen [`Resolved`], so the
/// prepended dir can never disagree with the binary the resolver picked.
fn child_path_prepend(resolved: &Resolved) -> Option<PathBuf> {
    match resolved {
        Resolved::OnPath => None,
        Resolved::AtPath(path) => path.parent().map(Path::to_path_buf),
    }
}

/// Home directory, treating an empty `HOME` as absent so the Windows
/// `USERPROFILE` fallback still applies (codex).
fn home_dir() -> Option<PathBuf> {
    let non_empty = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
    non_empty("HOME")
        .or_else(|| non_empty("USERPROFILE"))
        .map(PathBuf::from)
}

/// Byte range of the leading program token in `command`, after an optional
/// `stdio:` prefix, a leading `env`, and `KEY=value` assignments. We walk
/// whitespace rather than shlex-parsing: we need only the program token, and an
/// unquoted Windows path arg (`--data-dir C:\Users\x`) would trip POSIX
/// backslash escaping and drop the token entirely. `None` when there is no
/// program, or for a `PATH=` override (the child would resolve `ra` against a
/// different search path than we can probe).
fn program_token_span(command: &str) -> Option<std::ops::Range<usize>> {
    let mut idx = command.len() - command.trim_start().len();
    if command[idx..].starts_with("stdio:") {
        idx += "stdio:".len();
        let body = &command[idx..];
        idx += body.len() - body.trim_start().len();
    }
    let mut saw_env = false;
    while idx < command.len() {
        let body = &command[idx..];
        idx += body.len() - body.trim_start().len();
        if idx >= command.len() {
            break;
        }
        let end = idx
            + command[idx..]
                .find(char::is_whitespace)
                .unwrap_or(command.len() - idx);
        let token = &command[idx..end];
        if !saw_env && token == "env" {
            saw_env = true;
            idx = end;
            continue;
        }
        if is_env_assignment(token) {
            if token.split_once('=').is_some_and(|(k, _)| k == "PATH") {
                return None;
            }
            idx = end;
            continue;
        }
        return Some(idx..end);
    }
    None
}

/// The program a `--stdio-command` runs, IFF it is a **bare** `ra` (no path
/// separator) resolved through `PATH`. This decides only whether we may
/// *probe* the backend, so it inspects just the leading executable — trailing
/// args carrying shell syntax (a `--data-dir ~/x`, a pipe, or a Windows
/// `C:\...` path) must NOT disqualify provisioning (codex); round-trip safety
/// is enforced separately, at the rewrite step. Returns `None` for an explicit
/// path, a `PATH=` override, or a non-`ra` program (a user-specified non-`ra`
/// command is left to the user).
fn bare_ra_program(command: &str) -> Option<String> {
    let span = program_token_span(command)?;
    let token = &command[span];
    if token.contains('/') || token.contains('\\') {
        return None; // explicit path — user's own setup
    }
    // Only a bare `ra` is our canonical, provisionable form. We deliberately do
    // NOT accept `ra.exe`: it's never the canonical command (bare `ra` is, and
    // Windows `cmd /C` resolves it to whatever `.exe` exists). Any other
    // program is the user's own setup — left as-is, never provisioned.
    (token == "ra").then(|| token.to_owned())
}

/// Replace the leading program token of `command` with `new_program`,
/// preserving a `stdio:` prefix, `env`, `KEY=value` assignments, and all
/// trailing args verbatim (no shell re-parsing). Used for the Windows
/// bare-token rewrite, where embedding a path in a `cmd /C` command is unsafe.
/// `None` for an explicit path or a `PATH=` override.
fn rewrite_program_token(command: &str, new_program: &str) -> Option<String> {
    let span = program_token_span(command)?;
    let token = &command[span.clone()];
    if token.contains('/') || token.contains('\\') {
        return None;
    }
    let mut out = String::with_capacity(command.len() + new_program.len());
    out.push_str(&command[..span.start]);
    out.push_str(new_program);
    out.push_str(&command[span.end..]);
    Some(out)
}

/// Windows never embeds a path in the stdio command (`cmd /C` mangles it).
/// When the resolved binary's bare stem differs from the command's program
/// token, rewrite just that token so the child's prepended PATH resolves it;
/// otherwise leave the command untouched. `None` = no change needed.
fn windows_command_for(command: &str, bin: &Path) -> Option<String> {
    let token = bare_ra_program(command)?;
    let stem = bin.file_stem()?.to_str()?;
    (token != stem)
        .then(|| rewrite_program_token(command, stem))
        .flatten()
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

/// Rewrite a bare-`ra` stdio command to launch `ra_path` explicitly,
/// preserving a leading `stdio:` prefix, an `env` prefix, `KEY=value`
/// assignments, and all trailing args. Returns `None` — so the caller surfaces
/// an actionable "add RecurAgent to PATH" error instead of a mangled command — when
/// the command carries shell syntax the split+rejoin round-trip can't preserve
/// (`$PWD` would become a literal, a `~` would stop expanding, a pipe would be
/// quoted into an argument). Unix-only in practice: the Windows caller errors
/// before reaching here, since `cmd /C` won't honor this POSIX quoting anyway.
fn rewrite_program(command: &str, ra_path: &Path) -> Option<String> {
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
    *tokens.get_mut(idx)? = ra_path.to_string_lossy().into_owned();
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
/// `ra 0.1.0 (…)`, so a leading `v` and any `-pre`/`+build` suffix are
/// stripped before reading the leading `X.Y.Z` core.
fn parse_ra_version(output: &str) -> Option<String> {
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

/// The RecurAgent server **release this client targets** — the tag whose server carries
/// the exact `ra-core` protocol this client pins (see the `ra-core` rev in
/// Cargo.toml). Surfaced by `doctor` as the server version to run against.
///
/// **BUMP THIS whenever you bump the `ra-core` rev in Cargo.toml**, to the
/// release tag that contains that rev. [`REQUIRED_ra_CORE_REV`] and the test
/// beside it make the pair checkable: the rev moved to v2.0.3-rc.1 while this
/// stayed on v2.0.2, so the tag and the pinned rev must move together.
pub(crate) const REQUIRED_BACKEND_RELEASE: &str = "v0.1.0";

/// The `ra-core` rev that [`REQUIRED_BACKEND_RELEASE`] resolves to — i.e. the
/// commit the release tag points at, and the rev Cargo.toml must pin.
///
/// These are two halves of one decision (which server protocol this client
/// speaks) held in two files, with only a doc comment joining them. Recording
/// the rev here lets `ra_release_pin_matches_cargo_core_rev` fail when they
/// disagree, so bumping Cargo.toml without revisiting the release tag is caught
/// at test time rather than by a user running a mismatched server.
///
/// Test-only: its whole job is to be compared against Cargo.toml, so it would
/// be dead weight in a real build.
#[cfg(test)]
pub(crate) const REQUIRED_ra_CORE_REV: &str = "f4a31d9a0ef2228e919c3d516b78a9d82ad6f470";
/// The backend binary filename on this platform.
fn backend_binary_name() -> &'static str {
    if cfg!(windows) { "ra.exe" } else { "ra" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolver_prefers_a_ready_candidate_in_order_and_never_lets_an_outdated_sibling_shadow_it() {
        use CandidateKind::*;
        let sibling = PathBuf::from("/tui/dir/ra.exe");
        let path = PathBuf::from("ra");
        let install = PathBuf::from("/home/u/.ra/bin/ra.exe");
        let candidates = vec![
            (sibling.clone(), Sibling),
            (path.clone(), Path),
            (install.clone(), InstallDir),
        ];

        // MUST-FIX 1: an OUTDATED sibling must not win over a Ready `ra` on
        // PATH — the chosen Resolved is OnPath, so nothing is prepended and the
        // stale sibling can't be launched.
        let choice = choose_backend(&candidates, |p| {
            if p == sibling.as_path() {
                Probe::Outdated("1.0.0".into())
            } else if p == path.as_path() {
                Probe::Ready
            } else {
                Probe::Missing
            }
        });
        assert!(
            matches!(choice, Choice::Resolved(Resolved::OnPath)),
            "an outdated sibling must yield OnPath, not AtPath(sibling)"
        );

        // A Ready sibling wins outright (the repo dev layout).
        let choice = choose_backend(&candidates, |p| {
            if p == sibling.as_path() {
                Probe::Ready
            } else {
                Probe::Missing
            }
        });
        assert!(matches!(choice, Choice::Resolved(Resolved::AtPath(p)) if p == sibling));

        // A Ready install dir wins when nothing earlier is Ready.
        let choice = choose_backend(&candidates, |p| {
            if p == install.as_path() {
                Probe::Ready
            } else {
                Probe::Missing
            }
        });
        assert!(matches!(choice, Choice::Resolved(Resolved::AtPath(p)) if p == install));

        // Nothing Ready, one Outdated → the update guidance fires (first one
        // encountered in precedence order).
        let choice = choose_backend(&candidates, |p| {
            if p == install.as_path() {
                Probe::Outdated("1.0.0".into())
            } else {
                Probe::Missing
            }
        });
        assert!(matches!(choice, Choice::Outdated(v) if v == "1.0.0"));

        // All Missing → Missing (caller then errors without installing).
        assert!(matches!(
            choose_backend(&candidates, |_| Probe::Missing),
            Choice::Missing
        ));
    }

    #[test]
    fn prepend_is_derived_from_the_resolved_value() {
        assert_eq!(child_path_prepend(&Resolved::OnPath), None);
        let exe = PathBuf::from("/home/u/.ra/bin/ra");
        assert_eq!(
            child_path_prepend(&Resolved::AtPath(exe)),
            Some(PathBuf::from("/home/u/.ra/bin"))
        );
    }

    #[test]
    fn windows_command_rewrites_only_a_different_program_stem() {
        // Same stem (`ra` vs `ra.exe`) → the bare command is left untouched;
        // the child's prepended PATH resolves it via PATHEXT.
        assert_eq!(
            windows_command_for(
                "ra serve --stdio --solo",
                Path::new("C:/Users/u/.ra/bin/ra.exe")
            ),
            None
        );
        // A resolved binary whose stem differs from the command token → rewrite
        // only the token (never embed a path in a `cmd /C` command).
        assert_eq!(
            windows_command_for(
                "ra serve --stdio --solo",
                Path::new("C:/Users/u/.ra/bin/ra-legacy.exe")
            )
            .as_deref(),
            Some("ra-legacy serve --stdio --solo")
        );
        // A user-managed (non-canonical) command is never rewritten.
        assert_eq!(
            windows_command_for("ra serve --stdio", Path::new("C:/Users/u/.ra/bin/ra.exe")),
            None
        );
    }

    #[test]
    fn sibling_ready_backend_keeps_the_bare_command_and_prepends_its_dir() {
        let sibling = PathBuf::from("/tui/ra.exe");
        assert_eq!(windows_command_for("ra serve --stdio", &sibling), None);
        assert_eq!(
            child_path_prepend(&Resolved::AtPath(sibling)),
            Some(PathBuf::from("/tui"))
        );
    }

    #[test]
    fn resolved_backend_is_launchable_on_both_platform_paths() {
        let backend = PathBuf::from("/home/u/.ra/bin/ra");
        let command = "ra serve --stdio --solo";
        let resolved = Resolved::AtPath(backend.clone());

        // Windows: never embed a path. The resolved stem matches the command
        // token, so the command stays bare and its dir is prepended to the
        // child PATH (which resolves `ra` → `ra.exe`).
        assert_eq!(windows_command_for(command, &backend), None);
        assert_eq!(
            child_path_prepend(&resolved),
            Some(PathBuf::from("/home/u/.ra/bin"))
        );

        // Unix: the command is rewritten to the explicit path.
        assert_eq!(
            rewrite_program(command, &backend).as_deref(),
            Some("/home/u/.ra/bin/ra serve --stdio --solo")
        );
    }

    #[test]
    fn rewrite_program_token_preserves_env_prefix_and_trailing_args() {
        assert_eq!(
            rewrite_program_token("env A=1 ra serve --stdio", "ra").as_deref(),
            Some("env A=1 ra serve --stdio")
        );
        assert_eq!(
            rewrite_program_token("stdio:ra serve", "ra").as_deref(),
            Some("stdio:ra serve")
        );
        // A PATH override / explicit path is never rewritten.
        assert_eq!(rewrite_program_token("PATH=/x ra serve", "ra"), None);
        assert_eq!(rewrite_program_token("/opt/ra serve", "ra"), None);
    }

    #[test]
    fn bare_ra_program_matches_the_standard_shapes() {
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
                bare_ra_program(cmd).as_deref(),
                Some("ra"),
                "should extract bare ra from: {cmd}"
            );
        }
    }

    #[test]
    fn bare_ra_program_skips_explicit_paths_shell_syntax_and_others() {
        for cmd in [
            "/usr/local/bin/ra serve --stdio",   // explicit path — user-managed
            "$HOME/.local/bin/ra serve --stdio", // path (leading program) — user-managed
            "./ra serve",                        // explicit path
            "my-custom-backend --stdio",         // not ra
            "env A=1 my-backend serve",          // not ra
            "ra.exe serve --stdio",              // not canonical; bare `ra` is
            "ra serve --stdio",                  // legacy name — user-managed
            "env PATH=/custom/bin:$PATH ra serve", // PATH override — can't probe same ra
            "PATH=/opt/ra/bin ra serve",         // leading PATH override
        ] {
            assert_eq!(bare_ra_program(cmd), None, "should NOT auto-manage: {cmd}");
        }
    }

    #[test]
    fn rewrite_program_swaps_the_ra_token_only() {
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
        let rewritten = rewrite_program("RA_HOME=/data ra serve", p).unwrap();
        assert_eq!(
            shlex::split(&rewritten).unwrap(),
            ["env", "RA_HOME=/data", "/home/u/.ra/bin/ra", "serve"]
        );
        // Shell syntax we can't round-trip → None, so the caller errors with an
        // "add RecurAgent to PATH" message rather than emitting a mangled command.
        for cmd in [
            "ra serve --data-dir ~/data",    // ~ would stop expanding
            "RA_HOME=\"$PWD/.ra\" ra serve", // $PWD would become literal
            "ra serve | tee log",            // pipe quoted into an argument
            "ra serve && echo done",         // control operator
        ] {
            assert_eq!(
                rewrite_program(cmd, p),
                None,
                "should refuse to rewrite: {cmd}"
            );
        }
    }

    #[test]
    fn parse_backend_version_extracts_semver() {
        assert_eq!(
            parse_ra_version("ra 1.1.0 (79c19f6d4 2026-07-11)").as_deref(),
            Some("1.1.0")
        );
        assert_eq!(parse_ra_version("ra v2.10.3\n").as_deref(), Some("2.10.3"));
        assert_eq!(parse_ra_version("no version here"), None);
        assert_eq!(parse_ra_version("ra 1.2.3.4"), None); // 4-part isn't X.Y.Z
        // `ra --version` prints a prerelease; the leading X.Y.Z must still read.
        assert_eq!(
            parse_ra_version("ra 0.1.0-rc.13 (dde7655 2026-10-03)").as_deref(),
            Some("0.1.0")
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

    /// The ra-core rev in Cargo.toml and the server release tag this client
    /// targets are two halves of ONE decision — which server protocol this
    /// client speaks — living in two files, joined only by a doc comment saying
    /// "bump this too". That drifted: the rev reached v2.0.3-rc.1 while
    /// REQUIRED_BACKEND_RELEASE stayed at v2.0.2, so `doctor` pointed at a server
    /// older than the protocol the client had been built against.
    ///
    /// Reading Cargo.toml at test time turns the comment into a check.
    #[test]
    fn ra_release_pin_matches_cargo_core_rev() {
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
            rev, REQUIRED_ra_CORE_REV,
            "Cargo.toml records ra-core rev {rev}, but backend_ensure records \
             {REQUIRED_ra_CORE_REV} as the rev behind \
             REQUIRED_BACKEND_RELEASE ({REQUIRED_BACKEND_RELEASE}).\n\
             \n\
             If you bumped the rev, also bump REQUIRED_BACKEND_RELEASE to the \
             release tag containing it, and update \
             REQUIRED_ra_CORE_REV to match. Otherwise the recorded protocol \
             revision disagrees with this client."
        );
    }

    /// A release tag, not a bare version. Only `doctor` and this test read it
    /// now — nothing downloads from it.
    #[test]
    fn ra_release_pin_is_a_tag() {
        assert!(
            REQUIRED_BACKEND_RELEASE.starts_with('v'),
            "REQUIRED_BACKEND_RELEASE must be a tag like `v2.0.3-rc.1`, got {REQUIRED_BACKEND_RELEASE}"
        );
    }
}
