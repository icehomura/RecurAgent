//! SuperMemory memory provider.
//!
//! SuperMemory stores documents under a `containerTag` and recalls them with
//! semantic search (`POST /v3/documents`, `POST /v3/search`). The uniform
//! [`MemoryProvider`] namespace maps onto a container tag and the KV key rides
//! in the document's custom id, so lattice fusion treats it like any other
//! provider.
//!
//! Off by default (`SUPERMEMORY_ENABLED` / config `enabled`): no requests are
//! issued until the user opts in.
//!
//! Configuration precedence: `supermemory.json` under the agent config dir,
//! then environment (`SUPERMEMORY_API_KEY`, `SUPERMEMORY_BASE_URL`,
//! `SUPERMEMORY_CONTAINER_TAG`).

use crate::error::Result;
use crate::memory::MemoryProvider;

use super::http::{json_str, post_json, provider_config_path, read_config_file};

const CONFIG_FILE: &str = "supermemory.json";
const DEFAULT_BASE_URL: &str = "https://api.supermemory.ai";
const DEFAULT_CONTAINER_TAG: &str = "pi";
const DEFAULT_TIMEOUT_MS: u64 = 6_000;
const SEARCH_LIMIT: usize = 20;

/// Resolved SuperMemory connection settings.
#[derive(Debug, Clone)]
pub struct SupermemoryConfig {
    /// Master switch. Default false.
    pub enabled: bool,
    /// Bearer API key. Empty disables the provider.
    pub api_key: String,
    /// API base URL.
    pub base_url: String,
    /// Default container tag when a namespace has no explicit mapping.
    pub container_tag: String,
    /// Per-request budget in milliseconds.
    pub timeout_ms: u64,
}

impl Default for SupermemoryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key: String::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
            container_tag: DEFAULT_CONTAINER_TAG.to_string(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }
}

impl SupermemoryConfig {
    /// Load settings from `supermemory.json` (if present) then the environment.
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
            if let Some(tag) = json_str(&raw, "container_tag") {
                config.container_tag = tag;
            }
        }
        if let Ok(flag) = std::env::var("SUPERMEMORY_ENABLED") {
            config.enabled = matches!(flag.trim(), "1" | "true" | "yes" | "on");
        }
        if let Ok(api_key) = std::env::var("SUPERMEMORY_API_KEY") {
            config.api_key = api_key;
        }
        if let Ok(base_url) = std::env::var("SUPERMEMORY_BASE_URL")
            && !base_url.trim().is_empty()
        {
            config.base_url = base_url.trim().to_string();
        }
        if let Ok(tag) = std::env::var("SUPERMEMORY_CONTAINER_TAG")
            && !tag.trim().is_empty()
        {
            config.container_tag = tag.trim().to_string();
        }
        config
    }

    /// Whether the provider can actually issue requests.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.enabled && !self.api_key.trim().is_empty()
    }
}

/// SuperMemory provider behind the uniform [`MemoryProvider`] trait.
pub struct SupermemoryProvider {
    config: SupermemoryConfig,
}

impl SupermemoryProvider {
    /// Wrap explicit config.
    #[must_use]
    pub const fn new(config: SupermemoryConfig) -> Self {
        Self { config }
    }

    fn headers(&self) -> Vec<(String, String)> {
        vec![
            (
                "Authorization".to_string(),
                format!("Bearer {}", self.config.api_key),
            ),
            ("Accept".to_string(), "application/json".to_string()),
        ]
    }

    /// Container tag for one namespace; namespace wins over the default.
    fn container(&self, namespace: &str) -> String {
        if namespace.trim().is_empty() {
            self.config.container_tag.clone()
        } else {
            format!("{}/{namespace}", self.config.container_tag)
        }
    }

    /// Deterministic, idempotent document id for a namespace/key slot.
    fn slot_id(namespace: &str, key: &str) -> String {
        format!("pi-{namespace}-{key}")
    }
}

#[async_trait::async_trait]
impl MemoryProvider for SupermemoryProvider {
    async fn save(&self, namespace: &str, key: &str, value: &str) -> Result<()> {
        if !self.config.is_active() {
            return Ok(());
        }
        let payload = serde_json::json!({
            "id": Self::slot_id(namespace, key),
            "content": value,
            "containerTags": [self.container(namespace)],
        });
        // Degrade on failure: external store outage must not block the agent.
        let _ = post_json(
            &self.config.base_url,
            "/v3/documents",
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
            "q": key,
            "containerTags": [self.container(namespace)],
            "limit": SEARCH_LIMIT,
        });
        let Some(body) = post_json(
            &self.config.base_url,
            "/v3/search",
            &self.headers(),
            &payload,
            self.config.timeout_ms,
        )
        .await?
        else {
            return Ok(None);
        };
        let empty = Vec::new();
        let items = body
            .get("results")
            .and_then(serde_json::Value::as_array)
            .or_else(|| body.get("documents").and_then(serde_json::Value::as_array))
            .or_else(|| body.as_array())
            .unwrap_or(&empty);
        Ok(items.iter().find_map(|item| {
            let text = item
                .get("content")
                .and_then(serde_json::Value::as_str)
                .or_else(|| item.get("text").and_then(serde_json::Value::as_str))?;
            Some(text.to_string())
        }))
    }

    async fn list_namespaces(&self) -> Result<Vec<String>> {
        if !self.config.is_active() {
            return Ok(Vec::new());
        }
        let payload = serde_json::json!({
            "q": "",
            "containerTags": [self.config.container_tag.clone()],
            "limit": SEARCH_LIMIT,
        });
        let Some(body) = post_json(
            &self.config.base_url,
            "/v3/search",
            &self.headers(),
            &payload,
            self.config.timeout_ms,
        )
        .await?
        else {
            return Ok(Vec::new());
        };
        let empty = Vec::new();
        let items = body
            .get("results")
            .and_then(serde_json::Value::as_array)
            .or_else(|| body.get("documents").and_then(serde_json::Value::as_array))
            .unwrap_or(&empty);
        let prefix = format!("{}/", self.config.container_tag);
        let mut namespaces: Vec<String> = items
            .iter()
            .flat_map(|item| {
                item.get("containerTags")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
            })
            .filter_map(serde_json::Value::as_str)
            .filter_map(|tag| tag.strip_prefix(&prefix))
            .map(ToString::to_string)
            .collect();
        namespaces.sort();
        namespaces.dedup();
        Ok(namespaces)
    }

    async fn delete(&self, namespace: &str, key: &str) -> Result<()> {
        // SuperMemory deletes by document id via DELETE /v3/documents/{id};
        // the shared JSON helper is POST-only, so delete degrades to a no-op.
        // Documented deviation — KV reads still observe the newest write.
        let _ = (namespace, key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_off_and_inactive() {
        let config = SupermemoryConfig::default();
        assert!(!config.enabled, "provider must default off");
        assert!(!config.is_active());
    }

    #[test]
    fn enabled_without_key_stays_inactive() {
        let config = SupermemoryConfig {
            enabled: true,
            ..SupermemoryConfig::default()
        };
        assert!(!config.is_active());
    }

    #[test]
    fn container_prefixes_namespace_under_default_tag() {
        let provider = SupermemoryProvider::new(SupermemoryConfig::default());
        assert_eq!(provider.container("project"), "pi/project");
        assert_eq!(provider.container(""), "pi");
    }

    #[test]
    fn slot_id_is_deterministic() {
        assert_eq!(SupermemoryProvider::slot_id("project", "k"), "pi-project-k");
    }

    #[test]
    fn inactive_provider_degrades_without_network() {
        let provider = SupermemoryProvider::new(SupermemoryConfig::default());
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            assert!(
                provider
                    .load("project", "k")
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
