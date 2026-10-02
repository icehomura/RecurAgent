//! Workspace slash commands shared by the interactive stacks: `/rules`,
//! `/omfg`, `/commit`, `/review`, `/handoff`, `/approval`, `/advisor`, and
//! (default stack) `/memory`, `/hub`, `/security`, `/plugins`. Each takes
//! what it reads (working directory, session, approval state, package
//! manager) and returns what to show; the stacks differ only in where they
//! put it.

use std::fmt::Write as _;
use std::path::Path;

use rust_i18n::t;

/// A command's result: an optional transcript card plus a one-line status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub card: Option<String>,
    pub status: String,
}

impl Report {
    fn status(status: impl Into<String>) -> Self {
        Self {
            card: None,
            status: status.into(),
        }
    }

    fn card(card: String, status: impl Into<String>) -> Self {
        Self {
            card: Some(card),
            status: status.into(),
        }
    }
}

/// `/rules [list|remove <id>|toggle <id>]`: the project's TTSR stream rules.
pub fn rules(cwd: &Path, args: &str) -> Report {
    let args = args.trim();
    let mut store = crate::stream_rules::StreamRuleStore::load_for_project(cwd);
    if args.is_empty() || args == "list" {
        let rules = store.list_all_rules();
        let mut text = t!("workspace_rules_card_title", count = rules.len()).to_string();
        if rules.is_empty() {
            text.push_str(&t!("workspace_rules_none"));
        } else {
            for r in &rules {
                let status = if r.enabled {
                    format!("✅ {}", t!("workspace_rules_enabled"))
                } else {
                    format!("⏸️ {}", t!("workspace_rules_disabled"))
                };
                let _ = write!(
                    text,
                    "{}",
                    t!(
                        "workspace_rules_row",
                        name = r.name,
                        status = status,
                        pattern = r.pattern,
                        body = r.body
                    )
                );
            }
        }
        let count = rules.len();
        return Report::card(text, t!("workspace_rules_count", count = count).to_string());
    }
    if let Some(rest) = args.strip_prefix("remove ") {
        let id = rest.trim();
        return Report::status(match store.remove_rule(id) {
            Ok(true) => t!("workspace_rules_removed", id = id).to_string(),
            Ok(false) => t!("workspace_rules_not_found", id = id).to_string(),
            Err(e) => t!("workspace_rules_err_remove", error = e).to_string(),
        });
    }
    if let Some(rest) = args.strip_prefix("toggle ") {
        let id = rest.trim();
        let current = store
            .list_all_rules()
            .into_iter()
            .find(|r| r.id == id)
            .is_none_or(|r| r.enabled);
        return Report::status(match store.toggle_rule(id, !current) {
            Ok(true) => {
                let st = if current {
                    t!("workspace_rules_disabled")
                } else {
                    t!("workspace_rules_enabled")
                };
                t!("workspace_rules_toggled", id = id, state = st).to_string()
            }
            _ => t!("workspace_rules_not_found", id = id).to_string(),
        });
    }
    Report::status(t!("workspace_rules_usage"))
}

/// `/omfg <complaint>`: log the grievance and forge an active stream rule.
pub fn omfg(cwd: &Path, args: &str) -> Report {
    let args = args.trim();
    if args.is_empty() {
        return Report::status(t!("workspace_omfg_usage"));
    }
    match crate::stream_rules::GrievancesLedger::record_complaint(cwd, args, None) {
        Ok(g) => {
            let candidate = crate::stream_rules::GrievancesLedger::forge_candidate_rule(&g);
            let mut store = crate::stream_rules::StreamRuleStore::load_for_project(cwd);
            let _ = store.add_rule(candidate.clone(), false);
            let card = t!(
                "workspace_omfg_card",
                gid = g.id,
                complaint = g.complaint,
                rid = candidate.id,
                name = candidate.name,
                pattern = candidate.pattern,
                body = candidate.body
            )
            .to_string();
            Report::card(
                card,
                t!("workspace_omfg_forged", id = candidate.id).to_string(),
            )
        }
        Err(e) => Report::status(t!("workspace_omfg_err", error = e).to_string()),
    }
}

/// Paths from `git status --porcelain` output. Each line is `XY <path>` (or
/// `XY <old> -> <new>` for a rename); X is a space for an unstaged change,
/// so the line must not be trimmed before the path is sliced off.
fn porcelain_paths(status: &str) -> Vec<String> {
    status
        .lines()
        .filter_map(|line| line.get(3..))
        .map(|path| {
            path.split_once(" -> ")
                .map_or(path, |(_, new)| new)
                .trim()
                .to_string()
        })
        .filter(|path| !path.is_empty())
        .collect()
}

/// `/commit [--dry-run|-n|plan] [--include-lockfiles]`: split the working
/// tree's changes into atomic commits, or only show the plan.
pub fn commit(cwd: &Path, args: &str) -> Report {
    let args = args.trim();
    let dry_run = args == "dry-run"
        || args == "plan"
        || args
            .split_whitespace()
            .any(|arg| arg == "--dry-run" || arg == "-n");
    let include_lockfiles = args
        .split_whitespace()
        .any(|arg| arg == "--include-lockfiles");

    let status_out = match std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(cwd)
        .output()
    {
        Ok(o) => o,
        Err(e) => return Report::status(t!("workspace_commit_err_status", error = e).to_string()),
    };
    let changed_files = porcelain_paths(&String::from_utf8_lossy(&status_out.stdout));
    if changed_files.is_empty() {
        return Report::status(t!("workspace_commit_clean"));
    }

    let hunks = std::process::Command::new("git")
        .args(["diff", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()
        .and_then(|out| {
            crate::commit_split::DiffParser::parse_unified_diff(&String::from_utf8_lossy(
                &out.stdout,
            ))
            .ok()
        })
        .unwrap_or_default();

    let options = crate::commit_split::CommitOptions {
        dry_run,
        include_lockfiles,
        all_untracked: false,
        bead_reference: None,
        custom_prefix: None,
    };
    let plan = match crate::commit_split::CommitPlanner::plan(&hunks, &changed_files, &options) {
        Ok(plan) => plan,
        Err(e) => return Report::status(t!("workspace_commit_err_plan", error = e).to_string()),
    };
    if plan.units.is_empty() {
        return Report::status(t!("workspace_commit_none"));
    }

    let mut card = t!("workspace_commit_card_title", count = plan.units.len()).to_string();
    for (idx, unit) in plan.units.iter().enumerate() {
        let msg = unit.formatted_message(None);
        let _ = write!(
            card,
            "{}",
            t!(
                "workspace_commit_row",
                index = idx + 1,
                message = msg,
                scope = unit.scope
            )
        );
        for f in &unit.files {
            let _ = writeln!(card, "   - `{f}`");
        }
    }
    if dry_run {
        card.push_str(&t!("workspace_commit_dry_run"));
    } else {
        match crate::commit_split::CommitExecutor::execute(cwd, &plan, &options) {
            Ok(results) => {
                let successful = results.iter().filter(|r| r.success).count();
                let _ = write!(
                    card,
                    "{}",
                    t!(
                        "workspace_commit_done",
                        successful = successful,
                        total = plan.units.len()
                    )
                );
                for res in results {
                    if let Some(ref sha) = res.commit_sha {
                        let _ = writeln!(card, "- `[{sha}]` {}", res.message);
                    }
                }
            }
            Err(e) => {
                let _ = write!(card, "{}", t!("workspace_commit_err_exec", error = e));
            }
        }
    }
    let units = plan.units.len();
    Report::card(
        card,
        t!("workspace_commit_plan_status", units = units).to_string(),
    )
}

/// `/review [target]`: heuristic code review of the working tree (or target).
pub fn review(cwd: &Path, args: &str) -> Report {
    let args = args.trim();
    let options = crate::review::ReviewOptions {
        target: (!args.is_empty()).then(|| args.to_string()),
        fail_on: None,
        confidence_threshold: 0.70,
        format: "markdown".to_string(),
        max_findings: 15,
        out_file: None,
    };
    match crate::review::CodeReviewer::review(cwd, &options) {
        Ok(report) => Report::card(
            report.format_markdown(),
            format!("{}: {}", report.verdict.badge(), report.summary),
        ),
        Err(e) => Report::status(t!("workspace_review_err", error = e).to_string()),
    }
}

/// `/handoff [human|agent|<target>] [path]`: a handoff brief of the session.
pub fn handoff(session: &crate::session::Session, args: &str) -> Report {
    let args = args.trim();
    let (to_target, out_path) = if args.is_empty() {
        (crate::handoff::HandoffTarget::Human, None)
    } else {
        let mut parts = args.split_whitespace();
        let target_str = parts.next().unwrap_or("human");
        let path_str = parts.next().map(std::path::PathBuf::from);
        (crate::handoff::HandoffTarget::parse(target_str), path_str)
    };
    let doc = crate::handoff::HandoffGenerator::generate_from_session(session);

    // The card below always presents the brief; persistence is opt-in. A
    // human-targeted `/handoff` with no path leaves no file behind.
    let is_human = matches!(to_target, crate::handoff::HandoffTarget::Human);
    let output = if let Some(path) = out_path {
        crate::handoff::HandoffOutput::Path(path)
    } else if is_human {
        crate::handoff::HandoffOutput::Stdout
    } else {
        crate::handoff::HandoffOutput::Dir(crate::config::Config::handoffs_dir())
    };

    match crate::handoff::HandoffGenerator::deliver(&doc, &to_target, &output) {
        Ok(report) => Report::card(
            t!(
                "workspace_handoff_card",
                body = doc.to_markdown(),
                status = report.status
            )
            .to_string(),
            t!("workspace_handoff_ok"),
        ),
        Err(e) => Report::status(t!("workspace_handoff_err", error = e).to_string()),
    }
}

/// The `/memory` usage line.
///
/// A function rather than a `const` because the text now comes from the
/// catalogue and `t!` is not const-evaluable.
fn memory_usage() -> String {
    t!("workspace_memory_usage").to_string()
}

/// `/memory [view|list|search <query>|forget <id>]`: this project's memory
/// bank (bd-cv653.4.1). `view` (the default) is the mental model the agent
/// is given at session start.
pub fn memory(cwd: &Path, args: &str) -> Report {
    match crate::memory::MemoryStore::open(cwd) {
        Ok(store) => memory_in(&store, args),
        Err(e) => Report::status(format!("Memory bank unavailable: {e}")),
    }
}

fn memory_in(store: &crate::memory::MemoryStore, args: &str) -> Report {
    let args = args.trim();
    let (verb, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    let rest = rest.trim();
    let listing = |title: &str, memories: Vec<crate::memory::Memory>| {
        if memories.is_empty() {
            return Report::status(t!("workspace_memory_none", title = title).to_string());
        }
        let mut card = t!(
            "workspace_memory_card_title",
            title = title,
            count = memories.len()
        )
        .to_string();
        for m in &memories {
            let _ = write!(
                card,
                "{}",
                t!(
                    "workspace_memory_row",
                    id = m.id,
                    kind = m.kind,
                    content = m.content
                )
            );
            if !m.tags.is_empty() {
                let _ = write!(card, " _({})_", m.tags.join(", "));
            }
            card.push('\n');
        }
        Report::card(
            card,
            t!(
                "workspace_memory_count_status",
                title = title,
                count = memories.len()
            )
            .to_string(),
        )
    };
    match verb.to_ascii_lowercase().as_str() {
        "" | "view" => match store.mental_model() {
            Ok(model) if model.is_empty() => Report::status(t!("workspace_memory_empty")),
            Ok(model) => Report::card(
                t!("workspace_memory_view_card", model = model).to_string(),
                t!("workspace_memory_view_status"),
            ),
            Err(e) => Report::status(t!("workspace_memory_err_view", error = e).to_string()),
        },
        "list" => match store.list(20) {
            Ok(memories) => listing(&t!("workspace_memory_recent_title"), memories),
            Err(e) => Report::status(t!("workspace_memory_err_list", error = e).to_string()),
        },
        "search" if !rest.is_empty() => match store.recall(rest, Some(10)) {
            Ok(memories) => listing(
                &t!("workspace_memory_search_title", query = rest).to_string(),
                memories,
            ),
            Err(e) => Report::status(t!("workspace_memory_err_search", error = e).to_string()),
        },
        "forget" => rest.trim_start_matches('#').parse::<i64>().map_or_else(
            |_| Report::status(memory_usage()),
            |id| match store.edit(id, crate::memory::MemoryEditOp::Forget, None) {
                Ok(()) => Report::status(t!("workspace_memory_forgot", id = id).to_string()),
                Err(e) => Report::status(
                    t!("workspace_memory_err_forget", id = id, error = e).to_string(),
                ),
            },
        ),
        _ => Report::status(memory_usage()),
    }
}

/// `/security [paths...]`: the native source scan (bd-cv653.2.6) over the
/// workspace or the given paths, with recorded dispositions applied, as the
/// `security_scan` tool reports it.
pub fn security(cwd: &Path, args: &str) -> Report {
    const SHOWN: usize = 30;
    let paths: Vec<String> = args.split_whitespace().map(str::to_string).collect();
    let findings = match crate::security_scan::run_scan(cwd, &paths) {
        Ok(findings) => findings,
        Err(e) => return Report::status(t!("workspace_security_err_scan", error = e).to_string()),
    };
    let dispositions = match crate::security_scan::load_dispositions(cwd) {
        Ok(dispositions) => dispositions,
        Err(e) => {
            return Report::status(
                t!("workspace_security_err_dispositions", error = e).to_string(),
            );
        }
    };
    let (active, suppressed) =
        crate::security_scan::partition_by_disposition(findings, &dispositions);
    if active.is_empty() {
        return Report::status(
            t!("workspace_security_none", suppressed = suppressed.len()).to_string(),
        );
    }
    let mut card = t!("workspace_security_card_title", count = active.len()).to_string();
    if !suppressed.is_empty() {
        let _ = write!(
            card,
            "{}",
            t!("workspace_security_suppressed", count = suppressed.len())
        );
    }
    card.push_str("\n\n");
    for finding in active.iter().take(SHOWN) {
        let _ = writeln!(
            card,
            "{}",
            t!(
                "workspace_security_row",
                severity = finding.severity,
                path = finding.path,
                line = finding.line,
                message = finding.message,
                rule = finding.rule_id
            )
        );
    }
    if active.len() > SHOWN {
        let _ = writeln!(
            card,
            "{}",
            t!("workspace_security_more", count = active.len() - SHOWN)
        );
    }
    Report::card(
        card,
        t!("workspace_security_count", count = active.len()).to_string(),
    )
}

/// `/plugins`: the packages (extensions, skills, prompts, themes) installed
/// at user and project scope.
pub fn plugins(manager: &crate::package_manager::PackageManager) -> Report {
    use crate::package_manager::PackageScope;
    let packages = match manager.list_packages_blocking() {
        Ok(packages) => packages,
        Err(e) => return Report::status(t!("workspace_plugins_err", error = e).to_string()),
    };
    if packages.is_empty() {
        return Report::status(t!("workspace_plugins_none"));
    }
    let mut card = t!("workspace_plugins_card_title", count = packages.len()).to_string();
    for package in &packages {
        let scope = match package.scope {
            PackageScope::User => "user",
            PackageScope::Project => "project",
            PackageScope::Temporary => "temporary",
        };
        let _ = writeln!(
            card,
            "{}",
            t!(
                "workspace_plugins_row",
                source = package.source,
                scope = scope
            )
        );
    }
    card.push_str(&t!("workspace_plugins_hint"));
    Report::card(
        card,
        t!("workspace_plugins_count", count = packages.len()).to_string(),
    )
}

/// `/hub [id]`: this session's subagent children (bd-cv653.5.3), or one
/// child's transcript tail.
pub fn hub(args: &str) -> Report {
    let registry = crate::agent_hub::registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let id = args.trim();
    if id.is_empty() {
        return format_roster(&registry.roster());
    }
    match registry.transcript_page(id) {
        Ok(page) if page.trim().is_empty() => {
            Report::status(t!("workspace_hub_no_transcript", id = id).to_string())
        }
        Ok(page) => Report::card(
            t!("workspace_hub_transcript_card", id = id, page = page).to_string(),
            id,
        ),
        Err(e) => Report::status(e.to_string()),
    }
}

fn format_roster(roster: &[crate::agent_hub::ChildEntry]) -> Report {
    if roster.is_empty() {
        return Report::status(t!("workspace_hub_none"));
    }
    let mut card = t!("workspace_hub_card_title", count = roster.len()).to_string();
    for child in roster {
        let _ = writeln!(
            card,
            "{}",
            t!(
                "workspace_hub_row",
                id = child.id,
                kind = child.kind.as_str(),
                status = child.status.as_str(),
                task = child.task
            )
        );
    }
    card.push_str(&t!("workspace_hub_hint"));
    let running = roster.iter().filter(|c| !c.status.settled()).count();
    Report::card(
        card,
        t!(
            "workspace_hub_count",
            count = roster.len(),
            running = running
        )
        .to_string(),
    )
}

/// OMP `/advisor [toggle|on|off|status]`: bare toggles. `configured` is the
/// advisor role's model spec, when one is assigned. `pause`/`resume` stay
/// accepted as aliases of `off`/`on`.
pub fn advisor(configured: Option<&str>, args: &str) -> Report {
    advisor_with(&crate::advisor::ADVISOR_PAUSED, configured, args)
}

fn advisor_with(
    paused: &std::sync::atomic::AtomicBool,
    configured: Option<&str>,
    args: &str,
) -> Report {
    use std::sync::atomic::Ordering;
    let enable = match args.trim().to_ascii_lowercase().as_str() {
        "" | "toggle" => paused.load(Ordering::SeqCst),
        "on" | "resume" => true,
        "off" | "pause" => false,
        "status" => {
            return Report::status(match (configured, paused.load(Ordering::SeqCst)) {
                (Some(spec), false) => t!("workspace_advisor_on", spec = spec).to_string(),
                (Some(spec), true) => t!("workspace_advisor_off_assigned", spec = spec).to_string(),
                (None, _) => t!("workspace_advisor_none").to_string(),
            });
        }
        other => {
            return Report::status(t!("workspace_advisor_unknown", other = other).to_string());
        }
    };
    paused.store(!enable, Ordering::SeqCst);
    Report::status(match (enable, configured) {
        (false, _) => t!("workspace_advisor_disabled").to_string(),
        (true, Some(_)) => t!("workspace_advisor_enabled").to_string(),
        (true, None) => t!("workspace_advisor_enabled_unassigned").to_string(),
    })
}

/// `/approval [status|always-ask|write|yolo]`. Returns the new mode when it
/// changed, so the caller can record the transition in the session.
pub fn approval(
    state: &crate::approval::ApprovalState,
    args: &str,
) -> (Report, Option<crate::approval::ApprovalMode>) {
    use crate::approval::ApprovalMode;
    let (mode, message) = match args.trim().to_ascii_lowercase().as_str() {
        "" | "status" => {
            let classes = state.dual_confirm_classes();
            let dual = if classes.is_empty() {
                t!("workspace_approval_none").to_string()
            } else {
                classes
                    .iter()
                    .map(|c| c.label())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            return (
                Report::status(
                    t!(
                        "workspace_approval_status",
                        mode = state.mode().as_str(),
                        dual = dual
                    )
                    .to_string(),
                ),
                None,
            );
        }
        "always-ask" | "always_ask" | "always" | "ask" => (
            ApprovalMode::AlwaysAsk,
            t!("workspace_approval_set_always_ask"),
        ),
        "write" | "files" => (ApprovalMode::Write, t!("workspace_approval_set_write")),
        "yolo" | "auto-approve" | "auto" | "all" => {
            (ApprovalMode::Yolo, t!("workspace_approval_set_yolo"))
        }
        other => {
            return (
                Report::status(t!("workspace_approval_unknown", other = other).to_string()),
                None,
            );
        }
    };
    state.set_mode(mode);
    (Report::status(message), Some(mode))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/memory` over a real (per-test) project bank: retained facts are
    /// listed and searchable, and `forget` removes one.
    #[test]
    fn memory_lists_searches_and_forgets() {
        let root = std::env::temp_dir().join(format!(
            "pi-memory-report-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).expect("root");
        let store = crate::memory::MemoryStore::open(&root).expect("open");
        assert_eq!(
            memory_in(&store, "").status,
            "Memory bank is empty for this project."
        );
        let kept = store
            .retain(
                crate::memory::MemoryKind::Fact,
                "the parser lives in src/parser.rs",
                &["layout".to_string()],
                None,
            )
            .expect("retain");
        let listed = memory_in(&store, "list");
        assert!(
            listed
                .card
                .as_deref()
                .is_some_and(|card| card.contains("src/parser.rs")),
            "{listed:?}"
        );
        assert!(memory_in(&store, "search parser").card.is_some());
        assert_eq!(memory_in(&store, "search").status, memory_usage());
        assert_eq!(
            memory_in(&store, &format!("forget #{}", kept.id)).status,
            format!("Forgot memory #{}", kept.id)
        );
        assert_eq!(memory_in(&store, "list").status, "Recent memories: none");
    }

    /// A planted secret-shaped line is reported with its location; a clean
    /// tree reports no findings.
    #[test]
    fn security_reports_findings_with_their_location() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("clean.rs"), "fn main() {}\n").expect("write");
        let clean = security(dir.path(), "");
        assert!(
            clean.status.starts_with("Security scan: no findings"),
            "{clean:?}"
        );

        std::fs::write(
            dir.path().join("leak.py"),
            // ubs:ignore planted fake key for the scanner under test
            "import os\napi_key = \"abcdefghijklmnopqrstuvwxyz0123\"\n",
        )
        .expect("write");
        let found = security(dir.path(), "");
        assert!(
            found.status.ends_with("security finding(s)"),
            "{}",
            found.status
        );
        let card = found.card.expect("a findings card");
        assert!(card.contains("`leak.py:2`"), "{card}");
        assert!(card.contains("secret.generic-api-key"), "{card}");
    }

    #[test]
    fn plugins_lists_nothing_for_an_empty_project() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = crate::package_manager::PackageManager::new(dir.path().to_path_buf());
        let report = plugins(&manager);
        // The user's own global packages may be listed; the call must not
        // fail, and an empty result says how to add one.
        assert!(
            report.card.is_some() || report.status.starts_with("No packages installed"),
            "{report:?}"
        );
    }

    #[test]
    fn hub_reports_an_empty_roster_and_unknown_children() {
        assert!(format_roster(&[]).status.starts_with("No subagents"));
        assert!(hub("no-such-child-9").status.contains("unknown child"));
    }

    /// OMP semantics: bare `/advisor` toggles; on/off are idempotent; the
    /// reply says when no advisor model is assigned.
    #[test]
    fn advisor_toggles_and_reports_a_missing_model() {
        let paused = std::sync::atomic::AtomicBool::new(false);
        let spec = Some("anthropic/claude-haiku-4-5");
        let paused_now = || paused.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(advisor_with(&paused, spec, "").status, "Advisor disabled.");
        assert!(paused_now());
        assert_eq!(advisor_with(&paused, spec, "").status, "Advisor enabled.");
        assert!(!paused_now());
        assert_eq!(advisor_with(&paused, spec, "on").status, "Advisor enabled.");
        assert!(!paused_now(), "on is idempotent");
        assert_eq!(
            advisor_with(&paused, spec, "off").status,
            "Advisor disabled."
        );
        assert_eq!(
            advisor_with(&paused, spec, "status").status,
            "Advisor: off (anthropic/claude-haiku-4-5 assigned)"
        );
        assert_eq!(
            advisor_with(&paused, None, "on").status,
            "Advisor enabled, but no model is assigned to the 'advisor' role."
        );
        assert!(
            advisor_with(&paused, None, "sideways")
                .status
                .starts_with("Unknown /advisor subcommand")
        );
    }

    #[test]
    fn approval_sets_modes_and_reports_status() {
        use crate::approval::{ApprovalMode, ApprovalState};
        let state = ApprovalState::new(ApprovalMode::AlwaysAsk, false, Vec::new());
        let (report, changed) = approval(&state, "");
        assert_eq!(
            report.status,
            "Approval mode: always-ask | Dual-confirm classes: none"
        );
        assert_eq!(changed, None);
        let (_, changed) = approval(&state, "yolo");
        assert_eq!(changed, Some(ApprovalMode::Yolo));
        assert_eq!(state.mode(), ApprovalMode::Yolo);
        let (report, changed) = approval(&state, "maybe");
        assert_eq!(changed, None);
        assert!(report.status.starts_with("Unknown /approval mode"));
        assert_eq!(
            state.mode(),
            ApprovalMode::Yolo,
            "an unknown mode changes nothing"
        );
    }

    #[test]
    fn porcelain_paths_keep_the_first_character_of_unstaged_paths() {
        let status = " M src/a.rs\nM  src/b.rs\n?? new.txt\nR  old.rs -> src/renamed.rs\n";
        assert_eq!(
            porcelain_paths(status),
            vec!["src/a.rs", "src/b.rs", "new.txt", "src/renamed.rs"]
        );
    }

    /// `/omfg` forges a rule that `/rules` can then toggle and remove, in the
    /// same project. (Counts are not asserted: the store also merges the
    /// user's global rules file.)
    #[test]
    fn omfg_forges_a_rule_that_rules_manages() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            omfg(dir.path(), "  ").status,
            "Usage: /omfg <complaint about model behavior>"
        );
        assert_eq!(
            rules(dir.path(), "bogus").status,
            "Usage: /rules [list|remove <id>|toggle <id>]"
        );
        assert_eq!(
            rules(dir.path(), "toggle nope").status,
            "Stream rule 'nope' not found"
        );

        let forged = omfg(dir.path(), "stop apologizing before every answer");
        assert!(forged.card.is_some(), "{forged:?}");
        let id = forged
            .status
            .strip_prefix("Forged and activated stream rule '")
            .and_then(|rest| rest.strip_suffix('\''))
            .expect("status names the rule")
            .to_string();
        assert!(rules(dir.path(), "list").card.is_some());
        assert_eq!(
            rules(dir.path(), &format!("toggle {id}")).status,
            format!("Stream rule '{id}' is now disabled")
        );
        assert_eq!(
            rules(dir.path(), &format!("remove {id}")).status,
            format!("Removed stream rule '{id}'")
        );
    }

    #[test]
    fn commit_in_a_clean_repo_has_nothing_to_do() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git")
        };
        git(&["init", "-q"]);
        assert_eq!(
            commit(dir.path(), "").status,
            "Working tree clean; nothing to commit."
        );
        std::fs::write(dir.path().join("notes.md"), "hello\n").expect("write");
        git(&["add", "notes.md"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "init",
        ]);
        // An UNSTAGED edit: porcelain prints ` M notes.md`, the line shape
        // whose path the old parser cut to `otes.md`.
        std::fs::write(dir.path().join("notes.md"), "hello again\n").expect("write");
        let planned = commit(dir.path(), "--dry-run");
        let card = planned.card.expect("a plan card");
        assert!(card.contains("`notes.md`"), "{card}");
        assert!(card.contains("Dry run"), "{card}");
    }
}
