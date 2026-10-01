//! Chrome DevTools trace capture over CDP `Tracing`.
//!
//! A trace answers "why was this slow", which a screenshot cannot: it records
//! layout, script, and network timing for a window of activity and can be
//! opened in `chrome://tracing` or Perfetto. The start/stop pair brackets
//! whatever the caller does in between, so the tool is stateful by design and
//! the artifact is written on stop.
//!
//! Trace data arrives as `Tracing.dataCollected` chunks plus a
//! `Tracing.tracingComplete` terminator. The chunks are buffered by the receive
//! path (see [`super::cdp::record_trace_event_into`]) so a long capture does not
//! have to be drained in one command's event window.

use super::cdp::Cdp;
use super::{output, required};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::tools::ToolOutput;
use serde_json::{Value, json};

fn error(message: impl Into<String>) -> Error {
    Error::tool("browser", message)
}

/// Validate the arguments for a trace action.
pub(super) fn validate(args: &Value) -> Result<()> {
    required(args, "operation")?;
    let operation = args
        .get("operation")
        .and_then(Value::as_str)
        .ok_or_else(|| error("operation must be a string"))?;
    if !matches!(operation, "start" | "stop") {
        return Err(error(format!("unsupported trace operation: {operation}")));
    }
    let object = args
        .as_object()
        .ok_or_else(|| error("trace arguments must be an object"))?;
    for field in object.keys() {
        if !matches!(
            field.as_str(),
            "action" | "operation" | "output_path" | "tab"
        ) {
            return Err(error(format!("unsupported trace argument: {field}")));
        }
    }
    if let Some(path) = args.get("output_path") {
        let path = path
            .as_str()
            .filter(|path| !path.is_empty() && path.len() <= 4096 && !path.contains('\0'))
            .ok_or_else(|| {
                error("output_path must be a nonempty NUL-free string of at most 4096 bytes")
            })?;
        if !std::path::Path::new(path)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        {
            return Err(error("trace output_path must use .json"));
        }
    }
    Ok(())
}

/// Execute a trace action.
pub(super) async fn execute(
    owner: &AgentCx,
    cdp: &mut Cdp,
    cwd: &std::path::Path,
    tab: &str,
    args: &Value,
) -> Result<ToolOutput> {
    match required(args, "operation")? {
        "start" => {
            // Clear any stale chunks so a restart cannot append to a previous
            // capture; the caller has no way to see that mixing.
            cdp.clear_trace();
            cdp.call(
                owner,
                "Tracing.start",
                json!({
                    "categories": "-*,devtools.timeline,blink.user_timing,loading,v8.execute",
                    "transferMode": "ReportEvents",
                    // Return immediately; chunks arrive as events and are
                    // buffered on the receive path.
                    "bufferUsageReportingInterval": 1000
                }),
                false,
            )
            .await?;
            Ok(output(
                format!("Started trace on tab {tab}"),
                json!({"tracing": true, "backend": "cdp"}),
            ))
        }
        "stop" => {
            // Ask for the terminator, then drain whatever the buffer holds.
            cdp.call(owner, "Tracing.end", json!({}), false).await?;
            let events = cdp.take_trace();
            if events.is_empty() {
                return Err(error(
                    "trace produced no events; call trace start before the interaction under test",
                ));
            }
            let document = json!({"traceEvents": events});
            let bytes = serde_json::to_vec(&document)
                .map_err(|err| error(format!("could not serialize the trace: {err}")))?;

            let requested = args.get("output_path").and_then(Value::as_str).map_or_else(
                || format!("traces/browser_{}.json", uuid::Uuid::new_v4().simple()),
                ToString::to_string,
            );
            let target = crate::artifact_output::resolve_new(cwd, &requested, "browser")?;
            crate::artifact_output::publish(&target, &bytes, "browser")?;

            let count = events_len(&document);
            Ok(output(
                format!(
                    "Trace of {count} event(s) written to {}",
                    target.path().display()
                ),
                json!({
                    "tracing": false,
                    "saved_path": target.path().display().to_string(),
                    "size_bytes": bytes.len(),
                    "event_count": count,
                    "backend": "cdp"
                }),
            ))
        }
        other => Err(error(format!("unsupported trace operation: {other}"))),
    }
}

fn events_len(document: &Value) -> usize {
    document
        .get("traceEvents")
        .and_then(Value::as_array)
        .map_or(0, Vec::len)
}
