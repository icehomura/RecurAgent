//! Cookie and web-storage access over the CDP `Network` and `Storage` domains.
//!
//! Extension-hosted browser automation test login flows, seed a signed-in state,
//! and assert what a page persisted. Without these actions the only way to see
//! a cookie was to read it back out of `document.cookie`, which misses
//! `HttpOnly` cookies entirely and cannot set one at all.
//!
//! Every value here is host-scoped: the caller names a tab or a URL, never a
//! browser-level storage partition. Reads return the values; writes go through
//! CDP so the browser applies its own expiry/domain rules rather than us
//! synthesizing a cookie string.

use super::cdp::Cdp;
use super::{output, required};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::tools::ToolOutput;
use serde_json::{Value, json};

/// Cookie fields the model may set. Kept narrower than the CDP surface: a
/// caller cannot name a partition key or an opaque `sameParty` flag.
const SETTABLE_COOKIE_FIELDS: &[&str] = &[
    "name", "value", "url", "domain", "path", "secure", "httpOnly", "sameSite", "expires",
];

fn error(message: impl Into<String>) -> Error {
    Error::tool("browser", message)
}

/// Actions handled by this module.
pub(super) const ACTIONS: &[&str] = &["cookies", "storage"];

/// Validate the arguments for a cookie/storage action.
pub(super) fn validate(args: &Value) -> Result<()> {
    let action = required(args, "action")?;
    let object = args
        .as_object()
        .ok_or_else(|| error("cookie and storage arguments must be an object"))?;
    match action {
        "cookies" => {
            let operation = args
                .get("operation")
                .map_or(Ok("get"), |value| {
                    value
                        .as_str()
                        .ok_or_else(|| error("operation must be a string"))
                })?;
            match operation {
                "get" => {}
                "set" => {
                    let cookie = args
                        .get("cookie")
                        .and_then(Value::as_object)
                        .ok_or_else(|| error("cookies set requires a cookie object"))?;
                    if !cookie.contains_key("name") || !cookie.contains_key("value") {
                        return Err(error("cookie requires name and value"));
                    }
                    if let Some(field) = cookie
                        .keys()
                        .find(|key| !SETTABLE_COOKIE_FIELDS.contains(&key.as_str()))
                    {
                        return Err(error(format!("unsupported cookie field: {field}")));
                    }
                }
                "clear" => {}
                other => return Err(error(format!("unsupported cookie operation: {other}"))),
            }
            for field in object.keys() {
                if !matches!(
                    field.as_str(),
                    "action" | "operation" | "cookie" | "url" | "name" | "tab"
                ) {
                    return Err(error(format!("unsupported cookie argument: {field}")));
                }
            }
        }
        "storage" => {
            let area = args
                .get("area")
                .and_then(Value::as_str)
                .unwrap_or("local");
            if !matches!(area, "local" | "session") {
                return Err(error("storage area must be local or session"));
            }
            for field in object.keys() {
                if !matches!(field.as_str(), "action" | "area" | "tab") {
                    return Err(error(format!("unsupported storage argument: {field}")));
                }
            }
        }
        other => return Err(error(format!("unsupported action: {other}"))),
    }
    Ok(())
}

/// Execute a cookie or storage action against the page in `tab`.
pub(super) async fn execute(
    owner: &AgentCx,
    cdp: &mut Cdp,
    tab: &str,
    args: &Value,
) -> Result<ToolOutput> {
    match required(args, "action")? {
        "cookies" => cookies(owner, cdp, tab, args).await,
        "storage" => storage(owner, cdp, tab, args).await,
        other => Err(error(format!("unsupported action: {other}"))),
    }
}

async fn cookies(
    owner: &AgentCx,
    cdp: &mut Cdp,
    tab: &str,
    args: &Value,
) -> Result<ToolOutput> {
    let operation = args
        .get("operation")
        .and_then(Value::as_str)
        .unwrap_or("get");
    match operation {
        "get" => {
            // Scope the read to the tab's current page unless a URL is named, so
            // "what did this page set" does not become "every cookie in the
            // browser profile".
            let mut params = json!({});
            if let Some(url) = args.get("url").and_then(Value::as_str) {
                params["urls"] = json!([url]);
            } else {
                let current = cdp
                    .evaluate(owner, "location.href")
                    .await
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_string));
                if let Some(url) = current {
                    params["urls"] = json!([url]);
                }
            }
            let result = cdp.call(owner, "Network.getCookies", params, false).await?;
            let cookies = result
                .get("cookies")
                .cloned()
                .unwrap_or_else(|| json!([]));
            let count = cookies.as_array().map_or(0, Vec::len);
            Ok(output(
                format!("{count} cookie(s) for tab {tab}"),
                json!({"cookies": cookies, "count": count, "backend": "cdp"}),
            ))
        }
        "set" => {
            let cookie = args
                .get("cookie")
                .cloned()
                .ok_or_else(|| error("cookies set requires a cookie object"))?;
            // `Network.setCookie` takes the cookie fields flattened, with the
            // same names this tool accepts.
            let result = cdp.call(owner, "Network.setCookie", cookie.clone(), false).await?;
            let stored = result
                .get("success")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let name = cookie.get("name").and_then(Value::as_str).unwrap_or("?");
            if !stored {
                return Err(error(format!("browser rejected cookie {name}")));
            }
            Ok(output(
                format!("Set cookie {name}"),
                json!({"name": name, "stored": true, "backend": "cdp"}),
            ))
        }
        "clear" => {
            let result = cdp.call(owner, "Network.clearBrowserCookies", json!({}), false).await?;
            let _ = result;
            Ok(output(
                format!("Cleared cookies for tab {tab}"),
                json!({"cleared": true, "backend": "cdp"}),
            ))
        }
        other => Err(error(format!("unsupported cookie operation: {other}"))),
    }
}

async fn storage(
    owner: &AgentCx,
    cdp: &mut Cdp,
    tab: &str,
    args: &Value,
) -> Result<ToolOutput> {
    let area = args
        .get("area")
        .and_then(Value::as_str)
        .unwrap_or("local");
    // Storage is per-origin and only reachable from a live document, so read it
    // through the page rather than through `DOMStorage`, which needs an origin
    // the caller would have to name and could name wrongly.
    let expression = format!(
        "(() => {{ const s = window.{area}Storage; const out = {{}}; \
         for (let i = 0; i < s.length; i++) {{ const k = s.key(i); out[k] = s.getItem(k); }} \
         return out; }})()"
    );
    let value = cdp.evaluate(owner, &expression).await?;
    let count = value.as_object().map_or(0, serde_json::Map::len);
    Ok(output(
        format!("{count} {area} storage entr(ies) in tab {tab}"),
        json!({"area": area, "entries": value, "count": count, "backend": "cdp"}),
    ))
}
