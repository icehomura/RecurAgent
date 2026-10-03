//! `ra update` — Stage 2 ships **`--check` only** (plan, never mutate).
//!
//! `ra update --check` resolves the ra-server [`ProductSpec`], detects the
//! install method, calls the shared `ra_diagnostics::update_check` (a public
//! GitHub Releases fetch + the pure planner), and prints the resulting
//! [`UpdatePlan`]. It NEVER mutates a binary.
//!
//! Bare `ra update` (no `--check`) is intentionally inert in Stage 2: it
//! prints that self-update is not yet wired for this install method (that fold-in
//! over the existing `updater.rs` is Stage 3) plus the per-method upgrade hint,
//! and exits 0. It does NOT call `updater.rs`.
//!
//! Exit-code contract for `--check` (the design's `--check` contract):
//! - `0`  — up to date.
//! - `10` — a newer release is available (any of update-available /
//!   defer-to-package-manager / self-update-allowed).
//! - non-10 nonzero (`2`) — network/API error; a clear message is printed.

use clap::Args;
use colored::Colorize;
use eyre::Result;
use ra_diagnostics::{InstallMethod, ProductSpec, UpdatePlan, detect, update_check};

use super::Executable;

/// Exit code emitted by `--check` when a newer release is available.
const EXIT_UPDATE_AVAILABLE: i32 = 10;
/// Exit code emitted by `--check` when there is no release channel to check
/// (this fork ships from the source tree) — the same contract the terminal
/// client uses for the same situation.
const EXIT_NO_CHANNEL: i32 = 3;
/// Exit code emitted by `--check` on a network/API error (non-10 nonzero).
const EXIT_CHECK_ERROR: i32 = 2;

/// Check for (and, in a future Stage 3, apply) ra updates.
#[derive(Debug, Args)]
pub struct UpdateCommand {
    /// Only check for a newer release and print the plan — never mutate. Exits
    /// 10 when an update is available, 0 when up to date.
    #[arg(long)]
    pub check: bool,
    /// Emit the check result as machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

/// Build the ra server [`ProductSpec`]. `current_version` is the CLI's OWN
/// `CARGO_PKG_VERSION`, passed IN here — never the diagnostics crate's.
///
/// No published release channel exists for this fork (it ships from the source
/// tree), so no GitHub repo / brew formula / npm package is advertised.
fn ra_server_spec() -> ProductSpec {
    ProductSpec::new(
        "ra",                      // binary on PATH
        "ra",                      // package / display name
        env!("CARGO_PKG_VERSION"), // passed IN — ra-cli's own version
        "",                        // no upstream release channel
        "ra-bundle",               // asset prefix (used only if a channel is added)
    )
    .with_cargo_install("ra-cli")
    .with_cargo_dist_app("ra")
}

impl Executable for UpdateCommand {
    fn execute(self) -> Result<()> {
        if !self.check {
            // Stage 2: no self-update mutation wired yet (Stage 3 folds in
            // updater.rs). Print a clear status + the per-method upgrade hint.
            print_not_yet_wired();
            return Ok(());
        }

        let spec = ra_server_spec();
        // No published release channel in this fork: say so instead of asking
        // an upstream project's Releases API about a binary it never shipped.
        if spec.github_repo.trim().is_empty() {
            let message = "ra ships from the source tree; there is no upstream release channel.";
            let rebuild = "cargo build --release --bin ra";
            if self.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "check": true,
                        "current_version": spec.current_version,
                        "source": "ra repository (source build)",
                        "update_available": false,
                        "upgrade_command": null,
                        "rebuild_command": rebuild,
                    })
                );
            } else {
                println!("ra {} {message}", spec.current_version);
                println!("  To update, rebuild from source: {rebuild}");
            }
            std::process::exit(EXIT_NO_CHANNEL);
        }
        let method = detect(&spec);
        match update_check(&spec, &method) {
            Ok(plan) => {
                let (text, json, code) = render_check(&plan, &method, &spec);
                if self.json {
                    println!("{json}");
                } else {
                    print!("{text}");
                }
                std::process::exit(code);
            }
            Err(err) => {
                if self.json {
                    let body = serde_json::json!({
                        "status": "error",
                        "current": spec.current_version,
                        "error": err.to_string(),
                    });
                    println!("{}", serde_json::to_string_pretty(&body)?);
                } else {
                    eprintln!(
                        "{} could not check for updates: {err}",
                        "error:".red().bold()
                    );
                    eprintln!("  (no network? the public GitHub Releases API was unreachable)");
                }
                std::process::exit(EXIT_CHECK_ERROR);
            }
        }
    }
}

/// Bare `ra update` Stage-2 message: not yet wired, here's the hint.
fn print_not_yet_wired() {
    let spec = ra_server_spec();
    let method = detect(&spec);
    println!(
        "self-update for this install method ({}) is not yet wired (Stage 3); \
run with --check to see status",
        method.label()
    );
    if let Some(hint) = method.upgrade_hint(&spec) {
        println!("  upgrade with: {hint}");
    }
}

/// Map an [`UpdatePlan`] to `(human text, json string, exit code)`. Pure — no
/// IO — so the exit-code contract and rendering are unit-testable.
fn render_check(
    plan: &UpdatePlan,
    method: &InstallMethod,
    spec: &ProductSpec,
) -> (String, String, i32) {
    let current = &spec.current_version;
    let (status, mut text, code) = match plan {
        UpdatePlan::UpToDate => (
            "up-to-date",
            format!("{} ra is up to date (v{current})\n", "[✓]".green()),
            0,
        ),
        UpdatePlan::UpdateAvailable { latest } => (
            "update-available",
            format!(
                "{} a newer ra is available: v{current} → v{latest}\n",
                "[!]".yellow()
            ),
            EXIT_UPDATE_AVAILABLE,
        ),
        UpdatePlan::DeferToPackageManager { cmd } => (
            "defer-to-package-manager",
            format!(
                "{} a newer ra is available (current v{current})\n  upgrade with: {cmd}\n",
                "[!]".yellow()
            ),
            EXIT_UPDATE_AVAILABLE,
        ),
        UpdatePlan::SelfUpdateAllowed => (
            "self-update-allowed",
            format!(
                "{} a newer ra is available (current v{current})\n  this install can self-update (run `ra update` once Stage 3 lands)\n",
                "[!]".yellow()
            ),
            EXIT_UPDATE_AVAILABLE,
        ),
    };

    // For UpdateAvailable / SelfUpdateAllowed, still surface the per-method hint
    // so the user has an actionable command even before Stage 3.
    if matches!(
        plan,
        UpdatePlan::UpdateAvailable { .. } | UpdatePlan::SelfUpdateAllowed
    ) {
        if let Some(hint) = method.upgrade_hint(spec) {
            text.push_str(&format!("  upgrade with: {hint}\n"));
        }
    }

    let latest = match plan {
        UpdatePlan::UpdateAvailable { latest } => Some(latest.clone()),
        _ => None,
    };
    let json = serde_json::json!({
        "status": status,
        "current": current,
        "latest": latest,
        "method": method.id(),
        "exit_code": code,
    });
    (
        text,
        serde_json::to_string_pretty(&json).unwrap_or_default(),
        code,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ProductSpec {
        ra_server_spec()
    }

    #[test]
    fn spec_carries_cli_version_and_token_env() {
        let s = spec();
        assert_eq!(s.current_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(s.github_repo, "octos-org/octos");
        assert_eq!(s.github_token_env.as_deref(), Some("OCTOS_GITHUB_TOKEN"));
    }

    #[test]
    fn up_to_date_exits_zero() {
        let (text, json, code) =
            render_check(&UpdatePlan::UpToDate, &InstallMethod::Homebrew, &spec());
        assert_eq!(code, 0);
        assert!(text.contains("up to date"));
        assert!(json.contains("\"status\": \"up-to-date\""));
        assert!(json.contains("\"exit_code\": 0"));
    }

    #[test]
    fn update_available_exits_ten() {
        let plan = UpdatePlan::UpdateAvailable {
            latest: "9.9.9".into(),
        };
        let (text, json, code) = render_check(&plan, &InstallMethod::Unknown, &spec());
        assert_eq!(code, EXIT_UPDATE_AVAILABLE);
        assert!(text.contains("9.9.9"));
        assert!(json.contains("\"latest\": \"9.9.9\""));
        assert!(json.contains("\"exit_code\": 10"));
    }

    #[test]
    fn defer_to_package_manager_exits_ten_and_prints_cmd() {
        let plan = UpdatePlan::DeferToPackageManager {
            cmd: "brew upgrade octos-org/octos/octos".into(),
        };
        let (text, _json, code) = render_check(&plan, &InstallMethod::Homebrew, &spec());
        assert_eq!(code, EXIT_UPDATE_AVAILABLE);
        assert!(text.contains("brew upgrade octos-org/octos/octos"));
    }

    #[test]
    fn self_update_allowed_exits_ten() {
        let (_text, _json, code) = render_check(
            &UpdatePlan::SelfUpdateAllowed,
            &InstallMethod::CargoDistInstaller,
            &spec(),
        );
        assert_eq!(code, EXIT_UPDATE_AVAILABLE);
    }

    #[test]
    fn check_error_code_is_nonzero_and_not_ten() {
        assert_ne!(EXIT_CHECK_ERROR, 0);
        assert_ne!(EXIT_CHECK_ERROR, EXIT_UPDATE_AVAILABLE);
    }
}
