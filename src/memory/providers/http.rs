//! Shared HTTP plumbing for external memory providers.
//!
//! All external providers speak the same four-method KV contract, so the
//! transport lives here once instead of in each provider file. Every request
//! carries a per-call timeout and degrades on transport/parse failure rather
//! than surfacing a hard error — M7's "degraded, not fatal" acceptance rule.

use crate::error::Result;

/// One JSON HTTP call, already authenticated by the caller's headers.
///
/// `Ok(None)` means "transport or non-2xx failure, degrade silently"; `Ok(Some)`
/// carries the decoded body. Genuine configuration mistakes (bad timeout) are
/// the only `Err` — they are programmer errors and must not be swallowed.
pub(crate) async fn post_json(
    base_url: &str,
    path: &str,
    headers: &[(String, String)],
    payload: &serde_json::Value,
    timeout_ms: u64,
) -> Result<Option<serde_json::Value>> {
    let url = format!("{}{}", base_url.trim_end_matches('/'), path);
    let client = crate::http::client::Client::new();
    let mut builder = match client.post(&url).json(payload) {
        Ok(builder) => builder,
        Err(err) => {
            tracing::debug!("memory provider request build failed: {err}");
            return Ok(None);
        }
    };
    builder = builder.timeout(std::time::Duration::from_millis(timeout_ms));
    for (key, value) in headers {
        builder = builder.header(key.clone(), value.clone());
    }
    let response = match builder.send().await {
        Ok(response) => response,
        Err(err) => {
            tracing::debug!("memory provider request failed: {err}");
            return Ok(None);
        }
    };
    if !(200..300).contains(&response.status()) {
        tracing::debug!(
            "memory provider returned status {} for {url}",
            response.status()
        );
        return Ok(None);
    }
    let body = match response.text_limited(4 * 1024 * 1024).await {
        Ok(body) => body,
        Err(err) => {
            tracing::debug!("memory provider body read failed: {err}");
            return Ok(None);
        }
    };
    match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(value) => Ok(Some(value)),
        Err(err) => {
            tracing::debug!("memory provider returned invalid JSON: {err}");
            Ok(None)
        }
    }
}

/// Resolve a config file path under the agent's global config dir.
pub(crate) fn provider_config_path(file_name: &str) -> std::path::PathBuf {
    crate::config::Config::global_dir().join(file_name)
}

/// Read a JSON config file, returning `None` when absent or malformed.
///
/// Provider config files are optional overrides; a broken file must never
/// crash startup.
pub(crate) fn read_config_file(path: &std::path::Path) -> Option<serde_json::Value> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Coerce a string (or redundant `{"value": "..."}` wrapper) field to text.
pub(crate) fn json_str(value: &serde_json::Value, key: &str) -> Option<String> {
    value.get(key).and_then(|field| match field {
        serde_json::Value::String(text) if !text.trim().is_empty() => Some(text.trim().to_string()),
        serde_json::Value::Object(map) => map
            .get("value")
            .and_then(|inner| inner.as_str())
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(ToString::to_string),
        _ => None,
    })
}
