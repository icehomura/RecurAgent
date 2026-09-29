//! Device, geolocation, and network-condition emulation over CDP `Emulation`.
//!
//! Testing a responsive layout, an offline path, or a location-gated feature
//! needs the page to *believe* it is on another device or network. Without this
//! the only lever was a real resize, which does not change the media queries a
//! page reads, and no way at all to simulate offline or a location.
//!
//! Every setting here is **session state that persists until cleared**. That
//! makes `reset` part of the contract, not a convenience: a leftover 375×812
//! viewport would silently change what every later action sees. Callers that do
//! not want `reset` should know they are leaving the session emulated.

use super::cdp::Cdp;
use super::{output, required};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::tools::ToolOutput;
use serde_json::{Value, json};

/// Named devices this tool accepts, mapped to a CDP metrics override.
///
/// A fixed table rather than a free-form `{width, height, dpr}` triple: the
/// point of the action is "look like an iPhone", and letting the model invent
/// dimensions makes the result unreproducible across runs. Arbitrary metrics
/// remain reachable through `evaluate` for the rare case that needs them.
const DEVICES: &[(&str, f64, f64, f64, bool)] = &[
    // (name, width, height, deviceScaleFactor, mobile)
    ("iphone-se", 375.0, 667.0, 2.0, true),
    ("iphone-14", 390.0, 844.0, 3.0, true),
    ("iphone-14-pro-max", 430.0, 932.0, 3.0, true),
    ("pixel-7", 412.0, 915.0, 2.625, true),
    ("ipad-mini", 768.0, 1024.0, 2.0, true),
    ("ipad-pro", 1024.0, 1366.0, 2.0, true),
    ("desktop-1080p", 1920.0, 1080.0, 1.0, false),
    ("desktop-1440p", 2560.0, 1440.0, 1.0, false),
];

/// Longest user-agent string this tool will forward.
const MAX_USER_AGENT_CHARS: usize = 512;

fn error(message: impl Into<String>) -> Error {
    Error::tool("browser", message)
}

/// The origin of the tab's current page, for permission grants.
///
/// `Browser.grantPermissions` takes an origin, not a full URL, and rejects
/// `about:blank` (which has no origin), so an unroutable page yields `None`
/// rather than an error — the caller still gets the geolocation override.
async fn page_origin(owner: &AgentCx, cdp: &mut Cdp) -> Result<Option<String>> {
    let href = cdp
        .evaluate(owner, "location.origin")
        .await
        .ok()
        .and_then(|value| value.as_str().map(str::to_string));
    Ok(href.filter(|origin| origin.starts_with("http") && origin != "null"))
}

/// Validate the arguments for an emulation action.
pub(super) fn validate(args: &Value) -> Result<()> {
    let object = args
        .as_object()
        .ok_or_else(|| error("emulation arguments must be an object"))?;
    for field in object.keys() {
        if !matches!(
            field.as_str(),
            "action"
                | "device"
                | "width"
                | "height"
                | "latitude"
                | "longitude"
                | "offline"
                | "user_agent"
                | "tab"
        ) {
            return Err(error(format!("unsupported emulation argument: {field}")));
        }
    }
    if let Some(device) = args.get("device").and_then(Value::as_str) {
        if !DEVICES.iter().any(|(name, ..)| *name == device) {
            let known: Vec<&str> = DEVICES.iter().map(|(name, ..)| *name).collect();
            return Err(error(format!(
                "unknown device {device}; known devices: {}",
                known.join(", ")
            )));
        }
    }
    if let Some(user_agent) = args.get("user_agent").and_then(Value::as_str) {
        if user_agent.is_empty() || user_agent.chars().count() > MAX_USER_AGENT_CHARS {
            return Err(error(format!(
                "user_agent must be 1..={MAX_USER_AGENT_CHARS} characters"
            )));
        }
    }
    if let Some(latitude) = args.get("latitude").and_then(Value::as_f64) {
        if !(-90.0..=90.0).contains(&latitude) {
            return Err(error("latitude must be within -90..=90"));
        }
    }
    if let Some(longitude) = args.get("longitude").and_then(Value::as_f64) {
        if !(-180.0..=180.0).contains(&longitude) {
            return Err(error("longitude must be within -180..=180"));
        }
    }
    if let Some(offline) = args.get("offline")
        && !offline.is_boolean()
    {
        return Err(error("offline must be a boolean"));
    }
    Ok(())
}

/// Execute an emulation action against the page in `tab`.
pub(super) async fn execute(
    owner: &AgentCx,
    cdp: &mut Cdp,
    tab: &str,
    args: &Value,
) -> Result<ToolOutput> {
    // Reset short-circuits: applying settings and then clearing them would be
    // the same as doing nothing, and reading as if it worked.
    if required(args, "action")? == "reset_emulation" {
        // Clear each override explicitly. `Emulation` has no single tear-down,
        // and leaving one in place is worse than the extra round-trips.
        cdp.call(
            owner,
            "Emulation.clearDeviceMetricsOverride",
            json!({}),
            true,
        )
        .await?;
        cdp.call(
            owner,
            "Emulation.clearGeolocationOverride",
            json!({}),
            true,
        )
        .await?;
        // Drop the geolocation grant too, or the next page keeps a permission
        // the caller asked to have cleared.
        cdp.call(owner, "Browser.resetPermissions", json!({}), false)
            .await?;
        cdp.call(
            owner,
            "Network.emulateNetworkConditions",
            json!({
                "offline": false,
                "latency": 0, "downloadThroughput": -1, "uploadThroughput": -1
            }),
            true,
        )
        .await?;
        // An empty user agent restores the browser's own.
        cdp.call(
            owner,
            "Network.setUserAgentOverride",
            json!({"userAgent": ""}),
            true,
        )
        .await?;
        return Ok(output(
            format!("Cleared emulation overrides on tab {tab}"),
            json!({"reset": true, "backend": "cdp"}),
        ));
    }

    let mut applied: Vec<&str> = Vec::new();

    // A device override and explicit width/height are the same CDP call, so
    // reject the ambiguous combination rather than letting one silently win.
    let device = args.get("device").and_then(Value::as_str);
    let has_explicit_size = args.get("width").is_some() || args.get("height").is_some();
    if device.is_some() && has_explicit_size {
        return Err(error("set either device or width/height, not both"));
    }

    if let Some(name) = device {
        let (_, width, height, scale, mobile) = DEVICES
            .iter()
            .find(|(candidate, ..)| *candidate == name)
            .ok_or_else(|| error(format!("unknown device {name}")))?;
        cdp.call(
            owner,
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": width, "height": height,
                "deviceScaleFactor": scale, "mobile": mobile
            }),
            true,
        )
        .await?;
        applied.push("device");
    }

    if let Some(latitude) = args.get("latitude").and_then(Value::as_f64) {
        let longitude = args
            .get("longitude")
            .and_then(Value::as_f64)
            .ok_or_else(|| error("latitude requires longitude"))?;
        // Granting the permission is part of setting a location, not a separate
        // opt-in: an override the page is not allowed to read is a silent
        // no-op, and a caller who asked for a location plainly wants it visible.
        // `Browser.grantPermissions` is a browser-level command, so it is sent
        // without the page flag.
        let origin = page_origin(owner, cdp).await?;
        if let Some(origin) = origin.as_deref() {
            cdp.call(
                owner,
                "Browser.grantPermissions",
                json!({"origin": origin, "permissions": ["geolocation"]}),
                false,
            )
            .await?;
        }
        cdp.call(
            owner,
            "Emulation.setGeolocationOverride",
            json!({"latitude": latitude, "longitude": longitude, "accuracy": 1.0}),
            true,
        )
        .await?;
        applied.push("geolocation");
    }

    if let Some(offline) = args.get("offline").and_then(Value::as_bool) {
        cdp.call(
            owner,
            "Network.emulateNetworkConditions",
            json!({
                "offline": offline,
                "latency": 0, "downloadThroughput": -1, "uploadThroughput": -1
            }),
            true,
        )
        .await?;
        applied.push("offline");
    }

    if let Some(user_agent) = args.get("user_agent").and_then(Value::as_str) {
        cdp.call(
            owner,
            "Network.setUserAgentOverride",
            json!({"userAgent": user_agent}),
            true,
        )
        .await?;
        applied.push("user_agent");
    }

    if applied.is_empty() {
        return Err(error(
            "emulation needs at least one of device, latitude/longitude, offline, or user_agent",
        ));
    }
    Ok(output(
        format!("Applied {} emulation setting(s) on tab {tab}: {}", applied.len(), applied.join(", ")),
        json!({"applied": applied, "backend": "cdp"}),
    ))
}
