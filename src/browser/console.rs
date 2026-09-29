//! Page console and uncaught-exception capture over the CDP `Runtime` domain.
//!
//! Diagnosing a page failure used to mean injecting an `evaluate` call that
//! installed its own `console` hook, which misses anything logged before the
//! hook was installed and cannot see uncaught exceptions at all. These actions
//! read the buffer the CDP session fills on the receive path, so entries are
//! captured from the moment `Runtime.enable` was sent.
//!
//! The buffer is bounded (see `MAX_CONSOLE_ENTRIES`): it is a diagnostic aid,
//! not a log sink, and a page in a render loop must not be able to grow it
//! without limit.

use super::cdp::Cdp;
use super::{output, required};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::tools::ToolOutput;
use serde_json::{Value, json};

fn error(message: impl Into<String>) -> Error {
    Error::tool("browser", message)
}

/// Validate the arguments for a console action.
pub(super) fn validate(args: &Value) -> Result<()> {
    let object = args
        .as_object()
        .ok_or_else(|| error("console arguments must be an object"))?;
    for field in object.keys() {
        if !matches!(field.as_str(), "action" | "clear" | "level" | "tab") {
            return Err(error(format!("unsupported console argument: {field}")));
        }
    }
    if let Some(clear) = args.get("clear")
        && !clear.is_boolean()
    {
        return Err(error("clear must be a boolean"));
    }
    Ok(())
}

/// Execute a console action against the session backing `tab`.
pub(super) async fn execute(
    owner: &AgentCx,
    cdp: &mut Cdp,
    tab: &str,
    args: &Value,
) -> Result<ToolOutput> {
    // Turn the tap on before reading: a session that never enabled the domain
    // has an empty buffer, and reporting "no entries" would be wrong.
    cdp.enable_console(owner).await?;

    let clear = args.get("clear").and_then(Value::as_bool).unwrap_or(false);
    let filter = args
        .get("level")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase);

    let entries = cdp.take_console(clear);
    let total = entries.len();
    let selected: Vec<&super::cdp::ConsoleEntry> = entries
        .iter()
        .filter(|entry| {
            filter
                .as_deref()
                .is_none_or(|wanted| entry.level == wanted)
        })
        .collect();

    let rendered = selected
        .iter()
        .map(|entry| format!("[{}] {}", entry.level, entry.text))
        .collect::<Vec<_>>()
        .join("\n");

    let shown = selected.len();
    let summary = match filter.as_deref() {
        Some(level) => format!("{shown} of {total} console entr(ies) at level {level} on tab {tab}"),
        None => format!("{shown} console entr(ies) on tab {tab}"),
    };
    let text = if rendered.is_empty() {
        format!("{summary}: (none)")
    } else {
        format!("{summary}:\n{rendered}")
    };

    Ok(output(
        text,
        json!({
            "entries": selected
                .iter()
                .map(|entry| json!({"level": entry.level, "text": entry.text}))
                .collect::<Vec<_>>(),
            "count": shown,
            "totalBuffered": total,
            "cleared": clear,
            "backend": "cdp"
        }),
    ))
}

/// The action names this module owns, for the dispatcher's allowlist.
pub(super) const ACTIONS: &[&str] = &["console"];
