//! Declarative network interception over the CDP `Fetch` domain.
//!
//! Routes are **rules, not a callback**: the caller installs a table before the
//! interaction and the session answers matching requests automatically on the
//! event path. That shape is forced by the protocol — a `Fetch.requestPaused`
//! event must be answered or the page hangs — and it is also the right shape for
//! an agent, which cannot service a request mid-flight but can say "fail every
//! request to `analytics.example`".
//!
//! An unmatched URL is continued, never parked, so installing a route can only
//! change the behaviour of URLs the caller named. A matching route replaces the
//! network response entirely, which is what makes offline and error-path testing
//! possible without a proxy.

use super::cdp::{Cdp, NetworkRoute, RouteAction};
use super::{output, required};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::tools::ToolOutput;
use serde_json::{Value, json};

/// Cap on installed routes and on a synthetic response body.
const MAX_ROUTES: usize = 64;
const MAX_FULFILL_BODY_BYTES: usize = 1024 * 1024;

fn error(message: impl Into<String>) -> Error {
    Error::tool("browser", message)
}

/// Validate the arguments for a route action.
pub(super) fn validate(args: &Value) -> Result<()> {
    required(args, "operation")?;
    let object = args
        .as_object()
        .ok_or_else(|| error("network arguments must be an object"))?;
    for field in object.keys() {
        if !matches!(field.as_str(), "action" | "operation" | "routes" | "tab") {
            return Err(error(format!("unsupported network argument: {field}")));
        }
    }
    match required(args, "operation")? {
        // `set` replaces the table; an empty list therefore also clears it.
        "set" => {
            let routes = args
                .get("routes")
                .and_then(Value::as_array)
                .ok_or_else(|| error("network set requires a routes array"))?;
            if routes.len() > MAX_ROUTES {
                return Err(error(format!("at most {MAX_ROUTES} routes")));
            }
            for route in routes {
                let pattern = route
                    .get("pattern")
                    .and_then(Value::as_str)
                    .filter(|pattern| !pattern.is_empty() && pattern.len() <= 2048)
                    .ok_or_else(|| error("each route needs a 1..=2048 character pattern"))?;
                let _ = pattern;
                let action = route
                    .get("action")
                    .and_then(Value::as_str)
                    .unwrap_or("abort");
                match action {
                    "abort" | "continue" => {}
                    "fulfill" => {
                        if let Some(status) = route.get("status").and_then(Value::as_u64) {
                            if !(100..=599).contains(&status) {
                                return Err(error("fulfill status must be within 100..=599"));
                            }
                        }
                        if let Some(body) = route.get("body").and_then(Value::as_str) {
                            if body.len() > MAX_FULFILL_BODY_BYTES {
                                return Err(error(format!(
                                    "fulfill body exceeds {MAX_FULFILL_BODY_BYTES} bytes"
                                )));
                            }
                        }
                    }
                    other => return Err(error(format!("unsupported route action: {other}"))),
                }
            }
        }
        "clear" => {}
        other => return Err(error(format!("unsupported network operation: {other}"))),
    }
    Ok(())
}

/// Execute a network route operation.
pub(super) async fn execute(
    owner: &AgentCx,
    cdp: &mut Cdp,
    tab: &str,
    args: &Value,
) -> Result<ToolOutput> {
    let routes = match required(args, "operation")? {
        "clear" => Vec::new(),
        "set" => args
            .get("routes")
            .and_then(Value::as_array)
            .map(|routes| {
                routes
                    .iter()
                    .map(|route| NetworkRoute {
                        pattern: route
                            .get("pattern")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        action: match route.get("action").and_then(Value::as_str) {
                            Some("continue") | None => RouteAction::Continue,
                            Some("fulfill") => RouteAction::Fulfill {
                                status: route
                                    .get("status")
                                    .and_then(Value::as_u64)
                                    .and_then(|value| u16::try_from(value).ok())
                                    .unwrap_or(200),
                                body: route
                                    .get("body")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                                content_type: route
                                    .get("content_type")
                                    .and_then(Value::as_str)
                                    .unwrap_or("application/json")
                                    .to_string(),
                            },
                            Some("abort") => RouteAction::Abort,
                            Some(_) => RouteAction::Abort,
                        },
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
        other => return Err(error(format!("unsupported network operation: {other}"))),
    };

    cdp.set_routes(owner, routes).await?;
    let count = cdp.route_count();
    Ok(output(
        if count == 0 {
            format!("Cleared network interception on tab {tab}")
        } else {
            format!("Installed {count} network route(s) on tab {tab}")
        },
        json!({"routes": count, "intercepting": count > 0, "backend": "cdp"}),
    ))
}
