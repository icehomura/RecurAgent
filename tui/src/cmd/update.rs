//! `ra-tui update` — source-build update report.
//!
//! This fork ships from the RecurAgent repository (this source tree). It is not
//! published through a package manager or a GitHub release channel, so there is
//! no upstream to self-update from: `update` reports where the build comes from
//! and how to rebuild it. `--check` prints the same report non-destructively
//! (JSON with `--json`), and nothing here touches the network or the disk.
//!
//! Exit codes: `0` report printed (`--check`) · `3` "rebuild from source now"
//! (the interactive path — the fix is a command the user runs).

use eyre::Result;

/// Parsed `ra-tui update` flags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateArgs {
    /// Only report the update channel; never mutate (the only mode today).
    pub check: bool,
    /// Emit machine-readable JSON.
    pub json: bool,
}

/// Outcome of running `update`, mapped to a process exit code by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// Report printed (`--check`).
    Success,
    /// Can't self-update here; the source-rebuild command was printed.
    DeferredToSourceBuild,
}

impl UpdateOutcome {
    /// The process exit code for this outcome.
    pub fn exit_code(self) -> i32 {
        match self {
            UpdateOutcome::Success => 0,
            UpdateOutcome::DeferredToSourceBuild => 3,
        }
    }
}

/// The one command that updates this build.
pub const REBUILD_COMMAND: &str = "cargo build --release -p ra-cli --bin ra";

/// Entry point for `ra-tui update`.
pub fn run(args: UpdateArgs) -> Result<UpdateOutcome> {
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report_payload(env!("CARGO_PKG_VERSION"), args.check))?
        );
    } else {
        println!("{}", report_text(env!("CARGO_PKG_VERSION")));
    }
    Ok(if args.check {
        UpdateOutcome::Success
    } else {
        UpdateOutcome::DeferredToSourceBuild
    })
}

/// The human-readable report: where the build comes from and how to update it.
fn report_text(current: &str) -> String {
    format!(
        "ra-tui {current} ships from the ra repository (this source tree); there is no \
         upstream release channel.\n  To update, rebuild from source:\n    {REBUILD_COMMAND}"
    )
}

/// The machine-readable report (stable field names for scripts).
fn report_payload(current: &str, check: bool) -> serde_json::Value {
    serde_json::json!({
        "current_version": current,
        "source": "ra repository (source build)",
        "update_available": false,
        "upgrade_command": serde_json::Value::Null,
        "rebuild_command": REBUILD_COMMAND,
        "check": check,
        "message": report_text(current),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_exit_codes_match_design() {
        assert_eq!(UpdateOutcome::Success.exit_code(), 0);
        assert_eq!(UpdateOutcome::DeferredToSourceBuild.exit_code(), 3);
    }

    #[test]
    fn report_names_the_ra_repo_and_the_rebuild_command() {
        let text = report_text("0.3.0-rc.11");
        assert!(text.contains("ra-tui 0.3.0-rc.11"));
        assert!(
            text.contains("ra repository"),
            "must state where the build ships from: {text}"
        );
        assert!(
            text.contains(REBUILD_COMMAND),
            "must name the rebuild command: {text}"
        );
        assert!(
            !text.contains("http"),
            "no upstream install/update URL may be offered: {text}"
        );
    }

    #[test]
    fn json_report_is_non_destructive_and_offers_no_upstream_command() {
        let payload = report_payload("0.3.0-rc.11", true);
        assert_eq!(payload["current_version"], "0.3.0-rc.11");
        assert_eq!(payload["source"], "ra repository (source build)");
        assert_eq!(payload["update_available"], false);
        assert!(payload["upgrade_command"].is_null());
        assert_eq!(payload["rebuild_command"], REBUILD_COMMAND);
        assert_eq!(payload["check"], true);
    }
}
