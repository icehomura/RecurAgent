//! Mem0 memory provider.
//!
//! Mem0 exposes a REST KV-ish memory store: `POST /v1/memories/` to add,
//! `POST /v1/memories/search/` to recall, `GET /v1/memories/` to list. This
//! adapter maps the uniform [`MemoryProvider`] namespace/key pair onto Mem0's
//! `user_id` + `metadata` fields so the lattice can fuse Mem0 results without
//! knowing the wire shape.
//!
//! Off by default (`MEM0_ENABLED` / config `enabled`) so the port is a no-op
//! until the user opts in.
//!
//! Configuration precedence: `$HERMES_HOME`-style JSON file under the agent
//! config dir (`mem0.json`), then environment (`MEM0_API_KEY`, `MEM0_BASE_URL`,
//! `MEM0_USER_ID`).

use crate::error::Result;
use crate::memory::MemoryProvider;

use super::http::{json_str, post_json, provider_config_path, read_config_file};

const CONFIG_FILE: &str = "mem0.json";
const DEFAULT_BASE_URL: &str = "https://api.mem0.ai/v1";
const DEFAULT_USER_ID: &str = "pi-user";
const DEFAULT_TIMEOUT_MS: u64 = 6_000;
const SEARCH_LIMIT: usize = 20;

/// Resolved Mem0 connection settings.
#[derive(Debug, Clone)]
pub struct Mem0Config {
    /// Master switch. Default false: no requests are made until opted in.
    pub enabled: bool,
    /// Bearer API key. Empty disables the provider.
    pub api_key: String,
    /// API base URL (self-hosted or cloud).
    pub base_url: String,
    /// Mem0 `user_id` scope; the provider's namespace maps onto this.
    pub user_id: String,
    /// Per-request budget in milliseconds.
    pub timeout_ms: u64,
}

impl Default for Mem0Config {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key: String::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
            user_id: DEFAULT_USER_ID.to_string(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }
}

impl Mem0Config {
    /// Load settings from `mem0.json` (if present) then the environment.
    #[must_use]
    pub fn load() -> Self {
        let mut config = Self::default();
        if let Some(raw) = read_config_file(&provider_config_path(CONFIG_FILE)) {
            if let Some(enabled) = raw.get("enabled").and_then(serde_json::Value::as_bool) {
                config.enabled = enabled;
            }
            if let Some(api_key) = json_str(&raw, "api_key") {
                config.api_key = api_key;
            }
            if let Some(base_url) = json_str(&raw, "base_url") {
                config.base_url = base_url;
            }
            if let Some(user_id) = json_str(&raw, "user_id") {
                config.user_id = user_id;
            }
        }
        if let Ok(flag) = std::env::var("MEM0_ENABLED") {
            config.enabled = matches!(flag.trim(), "1" | "true" | "yes" | "on");
        }
        if let Ok(api_key) = std::env::var("MEM0_API_KEY") {
            config.api_key = api_key;
        }
        if let Ok(base_url) = std::env::var("MEM0_BASE_URL") {
            if !base_url.trim().is_empty() {
                config.base_url = base_url.trim().to_string();
            }
        } else if let Ok(host) = std::env::var("MEM0_HOST")
            && !host.trim().is_empty()
        {
            config.base_url = host.trim().to_string();
        }
        if let Ok(user_id) = std::env::var("MEM0_USER_ID")
            && !user_id.trim().is_empty()
        {
            config.user_id = user_id.trim().to_string();
        }
        config
    }

    /// Whether the provider can actually issue requests.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.enabled && !self.api_key.trim().is_empty()
    }
}

/// Mem0 provider behind the uniform [`MemoryProvider`] trait.
pub struct Mem0Provider {
    config: Mem0Config,
}

impl Mem0Provider {
    /// Wrap explicit config.
    #[must_use]
    pub const fn new(config: Mem0Config) -> Self {
        Self { config }
    }

    fn headers(&self) -> Vec<(String, String)> {
        vec![
            (
                "Authorization".to_string(),
                format!("Token {}", self.config.api_key),
            ),
            ("Accept".to_string(), "application/json".to_string()),
        ]
    }

    /// Metadata tag that pins a value to one namespace/key slot.
    fn slot_tag(namespace: &str, key: &str) -> String {
        format!("pi:{namespace}/{key}")
    }
}

#[async_trait::async_trait]
impl MemoryProvider for Mem0Provider {
    async fn save(&self, namespace: &str, key: &str, value: &str) -> Result<()> {
        if !self.config.is_active() {
            return Ok(());
        }
        let payload = serde_json::json!({
            "messages": [{ "role": "user", "content": value }],
            "user_id": self.config.user_id,
            "metadata": { "pi_slot": Self::slot_tag(namespace, key) },
        });
        // Mem0 has no upsert; a failed write degrades to a no-op so the agent
        // main flow never blocks on an external store.
        let _ = post_json(
            &self.config.base_url,
            "/memories/",
            &self.headers(),
            &payload,
            self.config.timeout_ms,
        )
        .await;
        Ok(())
    }

    async fn load(&self, namespace: &str, key: &str) -> Result<Option<String>> {
        if !self.config.is_active() {
            return Ok(None);
        }
        let payload = serde_json::json!({
            "query": Self::slot_tag(namespace, key),
            "user_id": self.config.user_id,
            "limit": SEARCH_LIMIT,
        });
        let Some(body) = post_json(
            &self.config.base_url,
            "/memories/search/",
            &self.headers(),
            &payload,
            self.config.timeout_ms,
        )
        .await?
        else {
            return Ok(None);
        };
        let Some(items) = body.get("results").and_then(serde_json::Value::as_array) else {
            return Ok(None);
        };
        Ok(items.iter().find_map(|item| {
            let memory = item.get("memory").and_then(serde_json::Value::as_str)?;
            Some(memory.to_string())
        }))
    }

    async fn list_namespaces(&self) -> Result<Vec<String>> {
        if !self.config.is_active() {
            return Ok(Vec::new());
        }
        let payload = serde_json::json!({ "user_id": self.config.user_id, "page_size": 200 });
        let Some(body) = post_json(
            &self.config.base_url,
            "/memories/",
            &self.headers(),
            &payload,
            self.config.timeout_ms,
        )
        .await?
        else {
            return Ok(Vec::new());
        };
        let empty = Vec::new();
        let items = body.as_array().unwrap_or(&empty);
        let mut namespaces: Vec<String> = items
            .iter()
            .filter_map(|item| {
                item.get("metadata")
                    .and_then(|meta| meta.get("pi_slot"))
                    .and_then(serde_json::Value::as_str)
            })
            .filter_map(|slot| slot.strip_prefix("pi:"))
            .filter_map(|rest| rest.split_once('/'))
            .map(|(namespace, _)| namespace.to_string())
            .collect();
        namespaces.sort();
        namespaces.dedup();
        Ok(namespaces)
    }

    async fn delete(&self, namespace: &str, key: &str) -> Result<()> {
        // Mem0 deletes by server-side id; the KV view has no id handle, so the
        // delete degrades to a no-op rather than guessing. Documented deviation.
        let _ = (namespace, key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_off_and_inactive() {
        let config = Mem0Config::default();
        assert!(!config.enabled, "provider must default off");
        assert!(!config.is_active(), "no api key means inactive");
    }

    #[test]
    fn enabled_without_key_stays_inactive() {
        let config = Mem0Config {
            enabled: true,
            ..Mem0Config::default()
        };
        assert!(!config.is_active(), "enabled but keyless must not call out");
    }

    #[test]
    fn enabled_with_key_is_active() {
        let config = Mem0Config {
            enabled: true,
            api_key: "test-key".to_string(),
            ..Mem0Config::default()
        };
        assert!(config.is_active());
    }

    #[test]
    fn slot_tag_round_trips_namespace() {
        assert_eq!(Mem0Provider::slot_tag("project", "k"), "pi:project/k");
        assert_eq!(
            Mem0Provider::slot_tag("project", "k")
                .strip_prefix("pi:")
                .and_then(|rest| rest.split_once('/')),
            Some(("project", "k"))
        );
    }

    #[test]
    fn inactive_provider_degrades_without_network() {
        let provider = Mem0Provider::new(Mem0Config::default());
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            assert!(
                provider
                    .load("project", "missing")
                    .await
                    .expect("degrades")
                    .is_none()
            );
            assert!(
                provider
                    .list_namespaces()
                    .await
                    .expect("degrades")
                    .is_empty()
            );
            provider.save("project", "k", "v").await.expect("no-op");
            provider.delete("project", "k").await.expect("no-op");
        });
    }
}
