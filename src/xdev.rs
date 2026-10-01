//! Tool load modes + the `xdev` dispatcher (bd-cv653.1.6).
//!
//! As the built-in tool surface grows, sending every tool's full JSON schema
//! on every provider request costs thousands of tokens per turn and measurably
//! degrades model tool-selection. Load modes split the surface in two:
//!
//! - **Essential** tools are always in the schema (read/write/edit/bash/
//!   grep/find/ls/hashline_edit/ask/todo/xdev).
//! - **Discoverable** tools stay out of the schema; a compact index in the
//!   system prompt (name + one-line purpose) advertises them, and the single
//!   `xdev` dispatcher tool lists, describes, runs, and promotes them.
//!
//! `xdev run` delegates through the agent's normal execution path (effects,
//! approval, logging all preserved — the agent intercepts the dispatch);
//! `xdev promote <name>` moves a discoverable tool into the live schema
//! mid-session without a restart. `--tools` pins the exact set and wins over
//! every tier default.

use crate::config::Config;
use crate::error::Error;
use crate::model::{ContentBlock, TextContent};
use crate::tools::{Tool, ToolOutput, ToolUpdate};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::Path;
use std::path::PathBuf;

/// Load tier for a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadMode {
    /// Always present in the provider schema.
    Essential,
    /// Hidden from the schema; reachable via `xdev` and promotable.
    Discoverable,
    /// Not enabled at all.
    Off,
}

impl LoadMode {
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "essential" => Some(Self::Essential),
            "discoverable" => Some(Self::Discoverable),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Essential => "essential",
            Self::Discoverable => "discoverable",
            Self::Off => "off",
        }
    }
}

/// Tools that are always in the schema by default.
const ESSENTIAL_DEFAULTS: &[&str] = &[
    "read",
    "write",
    "edit",
    "bash",
    "grep",
    "find",
    "ls",
    "hashline_edit",
    "ask",
    "todo",
    "xdev",
    // Daily-driver research tool (bd-cv653.2.1).
    "web_search",
    // Tiny schema, required the moment plan mode activates (bd-cv653.3.5).
    "submit_plan",
    // Zero-parameter clock read (gh #207, #103); cheaper to keep in the schema than
    // to make the model discover it through xdev before every date/time question.
    "current_time",
    // Reading JSON is common enough (session files, tool output, fixtures) that
    // the model should not have to shell out to a `jq` binary that may be absent.
    "json_query",
    // Structural search and staged rewrite: the schema cost is paid back the
    // first time a question is about code shape rather than text, and routing
    // them through `xdev run` made the model reach for regex instead.
    "ast_grep",
    "ast_edit",
    // The DAG scheduler is an orchestrator: a model that has to discover it
    // before every multi-step fan-out will not use it, and its whole value is
    // being the first thing reached for when work parallelizes.
    "dag",
    // Same argument as `dag`, one level down: `run_code` is the preferred way
    // to express any operation that is really tool orchestration, and a
    // guideline telling the model to prefer it over bash is worthless if the
    // tool it names is not in the schema.
    "run_code",
];

/// Tools that are opt-in ONLY (never in the default enabled set, never
/// discoverable-by-default): they spawn additional agent processes.
const OPT_IN_ONLY: &[&str] = &["subagent"];

/// The default tier for a tool name (before config overrides).
#[must_use]
pub fn default_tier(name: &str) -> LoadMode {
    if ESSENTIAL_DEFAULTS.contains(&name) {
        return LoadMode::Essential;
    }
    if OPT_IN_ONLY.contains(&name) {
        return LoadMode::Off;
    }
    LoadMode::Discoverable
}

/// Effective tier for a tool: `tools.loadMode.<name>` override > default
/// table. Unknown override values are ignored (a warning is the caller's job).
#[must_use]
pub fn tier_for(name: &str, config: Option<&Config>) -> LoadMode {
    if let Some(mode) = config
        .and_then(|c| c.tools.as_ref())
        .and_then(|t| t.load_mode.as_ref())
        .and_then(|modes| modes.get(name))
        .and_then(|raw| LoadMode::from_name(raw))
    {
        return mode;
    }
    default_tier(name)
}

/// The tools enabled when the caller passes no `--tools` flag, comma-joined.
///
/// Single source of truth: the clap `default_value` on `--tools` and
/// [`default_enabled_tools`] both read this, so they cannot drift. They did
/// drift once — `json_query` was added to the clap literal and to
/// [`ESSENTIAL_DEFAULTS`] but not to the function, which is what the
/// "must stay in lockstep" comment was warning about.
pub const DEFAULT_ENABLED_TOOLS: &str = "read,bash,edit,write,grep,find,ls,hashline_edit,web_search,ast_grep,ast_edit,lsp,debug,ask,todo,submit_plan,jobs,hub,current_time,run_code,json_query,dag";

/// Names of built-in tools enabled by default when the user passes no
/// `--tools`: the essential set plus the discoverable set. Opt-in-only tools
/// (subagent) stay out. See [`DEFAULT_ENABLED_TOOLS`].
#[must_use]
pub fn default_enabled_tools() -> Vec<&'static str> {
    DEFAULT_ENABLED_TOOLS.split(',').collect()
}

/// Curated one-line purposes for built-in tools, used for the system-prompt
/// discoverable index (bd-cv653.1.6). Kept honest by a unit test that
/// compares each entry against the live tool description.
#[must_use]
pub fn builtin_one_liner(name: &str) -> Option<&'static str> {
    Some(match name {
        "read" => "Read the contents of a file",
        "write" => "Write content to a file",
        "edit" => "Edit a file by replacing text",
        "bash" => "Execute a bash command in the current working directory",
        "grep" => "Search file contents for a pattern",
        "find" => "Search for files by glob pattern",
        "ls" => "List directory contents",
        "hashline_edit" => {
            "Apply precise file edits using LINE#HASH tags from a prior read with hashline=true"
        }
        "web_search" => "Search the web",
        "ast_grep" => "Structural code search using tree-sitter AST patterns (ast-grep syntax)",
        "ast_edit" => {
            "Staged structural code rewrite using tree-sitter AST patterns (ast-grep syntax)"
        }
        "lsp" => {
            "IDE-grade code intelligence via language servers: diagnostics, definition, references, ho…"
        }
        "debug" => {
            "Drive a real native/Python/Go debugger: launch/attach, retained breakpoints, thread-aware…"
        }
        "jobs" => "Manage background bash jobs started with `bash {background: true}`",
        "hub" => "Supervise long-running processes and manage background jobs",
        "subagent" => "Delegate an isolated task to a named Pi child agent",
        "ask" => "Ask the user structured questions mid-turn instead of guessing",
        "todo" => "Maintain the session task list",
        "xdev" => "Discover and run rarely-used tools that are not in the live schema",
        "current_time" => {
            "Return the host's current wall-clock time: UTC and local ISO-8601 timestamps, UTC offset,…"
        }
        "json_query" => "Query a JSON document with a jq filter expression",
        "run_code" => "Execute a JavaScript program against the available tools",
        "dag" => "Execute a dependency DAG of tool calls in parallel within this session",
        _ => return None,
    })
}

/// The system-prompt discoverable index: `(name, one-liner)` for tools that
/// are enabled AND discoverable-tier AND known built-ins. Extension/custom
/// tools still surface via `xdev list` at runtime.
#[must_use]
pub fn prompt_index_for(enabled: &[&str], config: Option<&Config>) -> Vec<(String, String)> {
    enabled
        .iter()
        .filter(|name| tier_for(name, config) == LoadMode::Discoverable)
        .filter_map(|name| builtin_one_liner(name).map(|line| (name.to_string(), line.to_string())))
        .collect()
}

/// One-line summary of a tool for the system-prompt discoverable index.
#[must_use]
pub fn one_liner(description: &str) -> String {
    const MAX: usize = 90;
    let first = description
        .split(['.', '\n'])
        .next()
        .unwrap_or(description)
        .trim();
    if first.chars().count() <= MAX {
        first.to_string()
    } else {
        let mut out: String = first.chars().take(MAX - 1).collect();
        out.push('…');
        out
    }
}

fn text_output(text: &str, is_error: bool) -> ToolOutput {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        details: None,
        is_error,
    }
}

/// Snapshot of a discoverable tool for the xdev dispatcher's list/describe.
#[derive(Debug, Clone)]
pub struct DiscoverableToolInfo {
    pub name: String,
    pub one_liner: String,
    pub description: String,
    pub parameters: Value,
}

/// The `xdev` dispatcher tool.
///
/// `list` and `describe` are handled inline from the snapshot; `run` and
/// `promote` are intercepted by the agent executor so the inner tool executes
/// through the normal path (approval, effects, logs). If an `xdev` run/promote
/// call reaches this `execute` directly (a host that did not install the
/// interception), it fails loudly instead of silently doing nothing.
pub struct XdevTool {
    #[allow(dead_code)]
    cwd: PathBuf,
    discoverable: Vec<DiscoverableToolInfo>,
}

impl XdevTool {
    #[must_use]
    pub fn new(cwd: &Path, discoverable: Vec<DiscoverableToolInfo>) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            discoverable,
        }
    }

    #[must_use]
    pub fn discoverable(&self) -> &[DiscoverableToolInfo] {
        &self.discoverable
    }

    fn list_output(&self) -> ToolOutput {
        let mut lines = String::from(
            "Discoverable tools (not in the live schema; use xdev run to call one, xdev promote to add it to the schema):\n",
        );
        for info in &self.discoverable {
            let _ = std::fmt::Write::write_fmt(
                &mut lines,
                format_args!("- {}: {}\n", info.name, info.one_liner),
            );
        }
        if self.discoverable.is_empty() {
            lines.push_str("(none)\n");
        }
        text_output(&lines, false)
    }

    fn describe_output(&self, name: &str) -> ToolOutput {
        self.discoverable
            .iter()
            .find(|info| info.name == name)
            .map_or_else(
                || {
                    text_output(
                        &format!(
                            "No discoverable tool named {name:?}. Use xdev list to see available tools."
                        ),
                        true,
                    )
                },
                |info| {
                    text_output(
                        &json!({
                            "name": info.name,
                            "description": info.description,
                            "parameters": info.parameters,
                            "tier": LoadMode::Discoverable.as_str(),
                        })
                        .to_string(),
                        false,
                    )
                },
            )
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for XdevTool {
    fn name(&self) -> &str {
        "xdev"
    }

    fn label(&self) -> &str {
        "xdev"
    }

    fn description(&self) -> &str {
        "Discover and run rarely-used tools that are not in the live schema. \
         actions: list (show discoverable tools with one-line purposes), \
         describe <name> (full description + JSON schema), run <name> <args> \
         (execute the tool through the normal path with its own validation), \
         promote <name> (add the tool to the live schema for the rest of the \
         session). Prefer direct tool calls for essential tools; use xdev for \
         anything not in your schema."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "describe", "run", "promote"],
                    "description": "Dispatcher action"
                },
                "name": {
                    "type": "string",
                    "description": "Target tool name (required for describe/run/promote)"
                },
                "args": {
                    "type": "object",
                    "description": "Arguments for the target tool (required for run)"
                }
            },
            "required": ["action"]
        })
    }

    fn effects(&self) -> crate::tools::ToolEffects {
        // Conservative union: a discoverable tool may do anything, so xdev
        // advertises the full effect set (approval gates stay honest).
        crate::tools::ToolEffects::read()
            .union(crate::tools::ToolEffects::write())
            .union(crate::tools::ToolEffects::append())
            .union(crate::tools::ToolEffects::network())
            .union(crate::tools::ToolEffects::process())
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> crate::error::Result<ToolOutput> {
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("list");
        match action {
            "list" => Ok(self.list_output()),
            "describe" => {
                let name = input.get("name").and_then(Value::as_str).unwrap_or("");
                if name.is_empty() {
                    return Ok(text_output("xdev describe requires a `name`", true));
                }
                Ok(self.describe_output(name))
            }
            "run" | "promote" => Err(Error::tool(
                "xdev",
                "xdev run/promote must be dispatched by the agent executor; \
                 this host did not install the interception"
                    .to_string(),
            )),
            other => Ok(text_output(
                &format!("Unknown xdev action {other:?}; expected list|describe|run|promote"),
                true,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn every_tiered_name_is_a_name_the_tools_flag_knows_about() {
        // AGENTS.md already requires the README inventory and these two
        // constants to stay in step. This makes the `--tools` vocabulary the
        // third: a name tiered here but absent from both lists in tools.rs is
        // a tool the flag reports as nonexistent, which is what
        // bd-tools-known-names-missing-seven-mbo3g was about. `xdev` itself
        // was in exactly that state while sitting in ESSENTIAL_DEFAULTS.
        use crate::tools::ToolRegistry;
        let mut missing = Vec::new();
        for name in ESSENTIAL_DEFAULTS.iter().chain(OPT_IN_ONLY) {
            let known = ToolRegistry::KNOWN_TOOL_NAMES.contains(name);
            let unselectable = ToolRegistry::TOOLS_NOT_SELECTED_BY_FLAG
                .iter()
                .any(|(listed, _)| listed == name);
            if !known && !unselectable {
                missing.push(*name);
            }
        }
        assert!(
            missing.is_empty(),
            "tiered here but unknown to --tools: {missing:?}; add each to \
             ToolRegistry::KNOWN_TOOL_NAMES, or to TOOLS_NOT_SELECTED_BY_FLAG \
             with a line saying what does turn it on"
        );
    }

    #[test]
    fn default_tiers_cover_core_and_opt_in() {
        assert_eq!(default_tier("read"), LoadMode::Essential);
        assert_eq!(default_tier("bash"), LoadMode::Essential);
        assert_eq!(default_tier("xdev"), LoadMode::Essential);
        assert_eq!(default_tier("ask"), LoadMode::Essential);
        assert_eq!(default_tier("subagent"), LoadMode::Off);
        assert_eq!(default_tier("web_search"), LoadMode::Essential);
        // Structural search, staged rewrite and the DAG scheduler are
        // first-class: reaching them through `xdev run` was the whole cost.
        assert_eq!(default_tier("ast_grep"), LoadMode::Essential);
        assert_eq!(default_tier("ast_edit"), LoadMode::Essential);
        assert_eq!(default_tier("dag"), LoadMode::Essential);
        assert_eq!(default_tier("run_code"), LoadMode::Essential);
        assert_eq!(default_tier("lsp"), LoadMode::Discoverable);
    }

    #[test]
    fn config_override_beats_default() {
        let mut modes = HashMap::new();
        modes.insert("ast_grep".to_string(), "essential".to_string());
        modes.insert("bash".to_string(), "discoverable".to_string());
        modes.insert("find".to_string(), "off".to_string());
        let config = Config {
            tools: Some(crate::config::ToolSettings {
                load_mode: Some(modes),
            }),
            ..Config::default()
        };
        assert_eq!(tier_for("ast_grep", Some(&config)), LoadMode::Essential);
        assert_eq!(tier_for("bash", Some(&config)), LoadMode::Discoverable);
        assert_eq!(tier_for("find", Some(&config)), LoadMode::Off);
        // Unoverridden names keep the default.
        assert_eq!(tier_for("read", Some(&config)), LoadMode::Essential);
        // Unknown override values are ignored.
        let mut bad = HashMap::new();
        bad.insert("grep".to_string(), "sometimes".to_string());
        let config = Config {
            tools: Some(crate::config::ToolSettings {
                load_mode: Some(bad),
            }),
            ..Config::default()
        };
        assert_eq!(tier_for("grep", Some(&config)), LoadMode::Essential);
    }

    #[test]
    fn one_liner_truncates_at_sentence_and_cap() {
        assert_eq!(one_liner("Short one."), "Short one");
        assert_eq!(one_liner("First sentence. Second."), "First sentence");
        let long = "x".repeat(200);
        let out = one_liner(&long);
        assert!(out.chars().count() <= 90);
    }

    #[test]
    fn default_enabled_excludes_opt_in_only() {
        let enabled = default_enabled_tools();
        assert!(!enabled.contains(&"subagent"));
        assert!(enabled.contains(&"read"));
        assert!(enabled.contains(&"ast_grep"));
        assert!(enabled.contains(&"dag"));
    }

    #[test]
    fn one_liner_table_matches_live_descriptions() {
        // Anti-drift gate: every built-in that exists in a default registry
        // must have its curated one-liner equal the live description's first
        // sentence (so the prompt index can never lie about a tool).
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = crate::tools::ToolRegistry::new(&default_enabled_tools(), temp.path(), None);
        let mut checked = 0;
        for tool in registry.tools() {
            if let Some(line) = builtin_one_liner(tool.name()) {
                assert_eq!(
                    line,
                    one_liner(tool.description()),
                    "one-liner table drifted from live description for {}",
                    tool.name()
                );
                checked += 1;
            }
        }
        assert!(
            checked >= 10,
            "expected to check the built-ins, got {checked}"
        );
    }
}
