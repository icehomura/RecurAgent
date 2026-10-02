//! Shared tool-invocation summary + extension-command catalog helpers.
//!
//! Extracted from the classic `agent` module when the bubbletea stack was
//! removed: the ftui stack calls `tool_invocation_summary` for its tool cards
//! and `extension_commands_for_catalog` for slash-command completion.

use crate::extensions::ExtensionManager;

pub fn extension_commands_for_catalog(
    manager: &ExtensionManager,
) -> Vec<crate::autocomplete::NamedEntry> {
    manager
        .list_commands()
        .into_iter()
        .filter_map(|cmd| {
            let name = cmd.get("name")?.as_str()?.to_string();
            let description = cmd
                .get("description")
                .and_then(|d| d.as_str())
                .map(std::string::ToString::to_string);
            Some(crate::autocomplete::NamedEntry { name, description })
        })
        .collect()
}

/// Strategy used to derive the head of a TUI tool card from its invocation.
///
/// This is the renderer registry promised by bd-cv653.9.2. Keeping the
/// registry separate from the rendering logic makes missing tool coverage a
/// testable condition instead of silently falling through to a generic card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolInvocationRenderer {
    Field(&'static str),
    FieldOrDefault {
        field: &'static str,
        default: &'static str,
    },
    Search {
        pattern: &'static str,
        scope: &'static str,
    },
    Action {
        action: &'static str,
        context: &'static [&'static str],
        default_action: Option<&'static str>,
    },
    Questions,
    Subagent,
    Mcp,
}

/// Resolve every native agent-facing tool to its card renderer. Mounted MCP
/// tools form a dynamic namespace and share one renderer; extension-defined
/// tools retain the extension/custom-renderer fallback.
fn tool_invocation_renderer(tool_name: &str) -> Option<ToolInvocationRenderer> {
    use ToolInvocationRenderer::{Action, Field, FieldOrDefault, Mcp, Questions, Search, Subagent};

    if tool_name
        .strip_prefix("mcp__")
        .is_some_and(|mounted| !mounted.is_empty())
    {
        return Some(Mcp);
    }

    Some(match tool_name {
        "bash" => Field("command"),
        "read" | "write" | "edit" | "hashline_edit" | "inspect_image" | "read_media" => {
            Field("path")
        }
        "ls" => FieldOrDefault {
            field: "path",
            default: ".",
        },
        "grep" | "find" | "ast_grep" => Search {
            pattern: "pattern",
            scope: "path",
        },
        "eval" => Field("code"),
        "web_search" | "recall" => Field("query"),
        "generate_image" => Field("prompt"),
        "tts" => Field("text"),
        "retain" => Field("content"),
        "reflect" => Field("question"),
        "learn" => Field("lesson"),
        "submit_plan" => Field("plan"),
        "jobs" => Action {
            action: "action",
            context: &["jobId"],
            default_action: None,
        },
        "hub" => Action {
            action: "op",
            context: &["name", "application"],
            default_action: None,
        },
        "security_scan" => Action {
            action: "op",
            context: &["sarifOut", "fingerprint", "baseline"],
            default_action: None,
        },
        "github" => Action {
            action: "op",
            context: &["repo", "number", "query", "run_id"],
            default_action: None,
        },
        "ast_edit" => Action {
            action: "action",
            context: &["path", "proposalId"],
            default_action: Some("stage"),
        },
        "lsp" => Action {
            action: "action",
            context: &["file", "symbol", "query", "method"],
            default_action: None,
        },
        "debug" => Action {
            action: "action",
            context: &["program", "file", "expression", "command"],
            default_action: None,
        },
        "computer" => Action {
            action: "action",
            context: &["output_path", "window_id", "display_id", "key"],
            default_action: None,
        },
        "browser" => Action {
            action: "action",
            context: &["url", "selector", "tab", "key", "output_path"],
            default_action: None,
        },
        "memory_edit" => Action {
            action: "op",
            context: &["id"],
            default_action: None,
        },
        "manage_skill" => Action {
            action: "op",
            context: &["name"],
            default_action: None,
        },
        "todo" => Action {
            action: "op",
            context: &["task", "phase"],
            default_action: None,
        },
        "xdev" => Action {
            action: "action",
            context: &["name"],
            default_action: None,
        },
        "ask" => Questions,
        "subagent" => Subagent,
        _ => return None,
    })
}

/// Compact, single-line description of what a tool invocation will do,
/// derived through the per-tool renderer registry (the bash command line,
/// file path, operation and target, first question, ...). A registered
/// renderer may still return `None` for malformed/incomplete arguments.
/// LOAD-BEARING VISIBILITY: `pub(super)` is re-exported as `pub(crate)` by
/// `src/interactive.rs` when `feature = "ftui"` is active (interactive_ftui
/// calls `crate::interactive::tool_invocation_summary`; bd-cv653.9.2).
#[allow(clippy::too_many_lines)]
pub fn tool_invocation_summary(tool_name: &str, args: &serde_json::Value) -> Option<String> {
    fn str_arg<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
        args.get(key).and_then(serde_json::Value::as_str)
    }

    fn nonblank_str_arg<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
        str_arg(args, key).filter(|value| !value.trim().is_empty())
    }
    /// First non-blank line only, collapsed to at most `max` characters.
    /// Control characters (incl. ESC) are dropped so a hostile or binary
    /// command string can never inject escape sequences into the transcript
    /// header, which bypasses the tool-output sanitizer.
    fn clip(text: &str, max: usize) -> String {
        let text = text.trim();
        let first_line = text.lines().next().unwrap_or("").trim_end();
        let mut out: String = first_line
            .chars()
            .filter(|c| !c.is_control() || *c == '\t')
            .take(max)
            .collect();
        if first_line.chars().count() > max || text.lines().count() > 1 {
            out.push('…');
        }
        out
    }

    fn scalar_arg(args: &serde_json::Value, key: &str) -> Option<String> {
        let value = args.get(key)?;
        match value {
            serde_json::Value::String(text) => Some(text.clone()),
            serde_json::Value::Number(number) => Some(number.to_string()),
            serde_json::Value::Bool(value) => Some(value.to_string()),
            _ => None,
        }
    }

    fn action_summary(
        args: &serde_json::Value,
        action: &str,
        context: &[&str],
        default_action: Option<&str>,
        max: usize,
    ) -> Option<String> {
        let action = match str_arg(args, action) {
            Some(value) if !value.trim().is_empty() => value.trim(),
            Some(_) => return None,
            None => default_action?,
        };
        let detail = context
            .iter()
            .find_map(|key| scalar_arg(args, key))
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        Some(clip(
            &detail.map_or_else(
                || action.to_string(),
                |detail| format!("{action} · {detail}"),
            ),
            max,
        ))
    }

    const MAX: usize = 96;
    let summary = match tool_invocation_renderer(tool_name)? {
        ToolInvocationRenderer::Field(field) => clip(nonblank_str_arg(args, field)?, MAX),
        ToolInvocationRenderer::FieldOrDefault { field, default } => match args.get(field) {
            None | Some(serde_json::Value::Null) => default.to_string(),
            Some(serde_json::Value::String(value)) if value.trim().is_empty() => {
                default.to_string()
            }
            Some(serde_json::Value::String(value)) => clip(value, MAX),
            Some(_) => return None,
        },
        ToolInvocationRenderer::Search { pattern, scope } => {
            let pattern = nonblank_str_arg(args, pattern)?.trim();
            nonblank_str_arg(args, scope).map_or_else(
                || clip(pattern, MAX),
                |scope| clip(&format!("{pattern} in {}", scope.trim()), MAX),
            )
        }
        ToolInvocationRenderer::Action {
            action,
            context,
            default_action,
        } => action_summary(args, action, context, default_action, MAX)?,
        ToolInvocationRenderer::Questions => {
            let question = args
                .get("questions")?
                .as_array()?
                .first()?
                .get("question")?
                .as_str()
                .filter(|question| !question.trim().is_empty())?;
            clip(question, MAX)
        }
        ToolInvocationRenderer::Subagent => {
            let has_single = args.get("agent").is_some() || args.get("task").is_some();
            let has_parallel = args.get("parallel").is_some();
            let has_chain = args.get("chain").is_some();
            if usize::from(has_single) + usize::from(has_parallel) + usize::from(has_chain) != 1 {
                return None;
            }

            if has_single {
                let agent = nonblank_str_arg(args, "agent")?.trim();
                let task = nonblank_str_arg(args, "task")?.trim();
                clip(&format!("{agent}: {task}"), MAX)
            } else if has_parallel {
                let parallel = args.get("parallel")?.as_array()?;
                if parallel.is_empty() {
                    return None;
                }
                format!("{} parallel tasks", parallel.len())
            } else if has_chain {
                let chain = args.get("chain")?.as_array()?;
                if chain.is_empty() {
                    return None;
                }
                format!("{} chained tasks", chain.len())
            } else {
                return None;
            }
        }
        ToolInvocationRenderer::Mcp => {
            let mounted = tool_name.strip_prefix("mcp__")?;
            clip(&format!("MCP {}", mounted.replace("__", " · ")), MAX)
        }
    };
    if summary.is_empty() {
        None
    } else {
        Some(summary)
    }
}

/// Build a user-role model message from plain text.
pub(super) fn build_user_message(text: String) -> crate::model::Message {
    crate::model::Message::User(crate::model::UserMessage {
        content: crate::model::UserContent::Text(text),
        timestamp: chrono::Utc::now().timestamp_millis(),
    })
}
