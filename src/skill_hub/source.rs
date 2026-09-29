//! Skill source abstraction and the agentskills.io primary source.
//!
//! The primary source is a static JSON index behind CDN caching — no token,
//! no pagination, schema `discovery/0.2.0`. The index is cached in memory
//! for 24h so repeated `skill_hub_search` calls stay free (per the M8 plan:
//! local daily cache + ETag semantics).

use crate::error::{Error, Result};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Default in-memory index freshness window.
const INDEX_TTL: Duration = Duration::from_hours(24);

/// One entry in a skill index.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SkillMeta {
    /// Skill name (directory / SKILL.md frontmatter name).
    pub name: String,
    /// Short description used for matching and L0 display.
    pub description: String,
    /// Origin identifier (`"agentskills"`, `"github"`, …).
    pub source: String,
    /// Download URL when the index carries one.
    pub url: Option<String>,
}

/// A fetched skill index.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SkillIndex {
    /// Index schema tag as reported by the source.
    pub schema: String,
    /// Entries, in source order.
    pub skills: Vec<SkillMeta>,
    /// Fetch time (epoch millis).
    pub fetched_at_ms: u64,
}

/// A downloaded skill ready for quarantine + install.
#[derive(Debug, Clone)]
pub struct SkillPackage {
    /// Skill name.
    pub name: String,
    /// SKILL.md body.
    pub content: String,
    /// Origin identifier.
    pub source: String,
}

/// Uniform surface every skill source implements.
#[async_trait::async_trait]
pub trait SkillSource: Send + Sync {
    /// Fetch (or serve from cache) the source's index.
    async fn fetch_index(&self) -> Result<SkillIndex>;
    /// Download one skill's SKILL.md body by name.
    async fn fetch_skill(&self, name: &str) -> Result<SkillPackage>;
    /// Stable source identifier.
    fn name(&self) -> &str;
}

/// agentskills.io primary source (`/.well-known/agent-skills/index.json`).
pub struct AgentskillsSource {
    /// Base URL, no trailing slash.
    base_url: String,
    /// Cached index + fetch instant + last-seen ETag.
    cache: Mutex<Option<CachedIndex>>,
}

/// One in-memory index snapshot with the validators needed for a conditional
/// refresh.
#[derive(Clone)]
struct CachedIndex {
    index: SkillIndex,
    at: Instant,
    /// `ETag` from the last 200 response, if the CDN sent one.
    etag: Option<String>,
}

impl AgentskillsSource {
    /// Create a source against the production index.
    #[must_use]
    pub fn new() -> Self {
        Self {
            base_url: "https://agentskills.io".to_string(),
            cache: Mutex::new(None),
        }
    }

    /// Create a source against a custom base URL (tests, mirrors).
    #[must_use]
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            cache: Mutex::new(None),
        }
    }

    /// Store a fresh snapshot in the in-memory cache, refreshing its window.
    fn store_cache(&self, index: SkillIndex, etag: Option<String>) {
        if let Ok(mut cache) = self.cache.lock() {
            *cache = Some(CachedIndex {
                index,
                at: Instant::now(),
                etag,
            });
        }
    }

    /// Parse one index payload into [`SkillIndex`].
    ///
    /// Tolerates both the `{"skills": [...]}` wrapper and a bare array.
    pub fn parse_index(json: &str, fetched_at_ms: u64) -> Result<SkillIndex> {
        let value: serde_json::Value = serde_json::from_str(json)
            .map_err(|err| Error::tool("skill_hub", format!("index JSON: {err}")))?;
        let items = if let Some(items) = value.get("skills").and_then(serde_json::Value::as_array) {
            items.as_slice()
        } else if let Some(items) = value.as_array() {
            items.as_slice()
        } else {
            return Err(Error::tool(
                "skill_hub",
                "index has neither a `skills` array nor a top-level array",
            ));
        };
        let skills = items
            .iter()
            .filter_map(|item| {
                Some(SkillMeta {
                    name: item.get("name")?.as_str()?.to_string(),
                    description: item
                        .get("description")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    source: item
                        .get("source")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("agentskills")
                        .to_string(),
                    url: item
                        .get("url")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                })
            })
            .collect();
        Ok(SkillIndex {
            schema: value
                .get("schema")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("discovery/0.2.0")
                .to_string(),
            skills,
            fetched_at_ms,
        })
    }
}

impl Default for AgentskillsSource {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl SkillSource for AgentskillsSource {
    async fn fetch_index(&self) -> Result<SkillIndex> {
        // Fresh cache wins: no network on repeated searches.
        let (cached_index, cached_etag) = match self.cache.lock() {
            Ok(cache) => match cache.as_ref() {
                Some(entry) if entry.at.elapsed() < INDEX_TTL => {
                    return Ok(entry.index.clone());
                }
                Some(entry) => (Some(entry.index.clone()), entry.etag.clone()),
                None => (None, None),
            },
            Err(_) => (None, None),
        };
        let url = format!("{}/.well-known/agent-skills/index.json", self.base_url);
        let client = crate::http::client::Client::new();
        let mut request = client.get(&url);
        // Conditional refresh: a 304 means the cached index is still current,
        // so we reset its freshness window instead of re-downloading the body.
        if let Some(etag) = cached_etag.as_deref() {
            request = request.header("If-None-Match", etag);
        }
        let response = request
            .send()
            .await
            .map_err(|err| Error::tool("skill_hub", format!("fetch index: {err}")))?;
        let status = response.status();
        if status == 304 {
            if let Some(index) = cached_index {
                self.store_cache(index, cached_etag);
                // Re-read so the returned index is the just-refreshed snapshot.
                if let Ok(cache) = self.cache.lock()
                    && let Some(entry) = cache.as_ref()
                {
                    return Ok(entry.index.clone());
                }
                return Err(Error::tool("skill_hub", "index cache lost after 304"));
            }
            return Err(Error::tool(
                "skill_hub",
                "index returned 304 without a cached copy",
            ));
        }
        if status != 200 {
            return Err(Error::tool(
                "skill_hub",
                format!("fetch index: status {status}"),
            ));
        }
        let etag = response
            .headers()
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("etag"))
            .map(|(_, value)| value.clone());
        let body = response
            .text_limited(16 * 1024 * 1024)
            .await
            .map_err(|err| Error::tool("skill_hub", format!("fetch index: {err}")))?;
        let fetched_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        let index = Self::parse_index(&body, fetched_at_ms)?;
        self.store_cache(index.clone(), etag);
        Ok(index)
    }

    async fn fetch_skill(&self, name: &str) -> Result<SkillPackage> {
        let index = self.fetch_index().await?;
        let entry = index
            .skills
            .iter()
            .find(|skill| skill.name == name)
            .ok_or_else(|| {
                Error::tool("skill_hub", format!("skill `{name}` not present in index"))
            })?;
        let url = entry.url.clone().ok_or_else(|| {
            Error::tool(
                "skill_hub",
                format!("skill `{name}` carries no download URL"),
            )
        })?;
        let client = crate::http::client::Client::new();
        let response = client
            .get(&url)
            .send()
            .await
            .map_err(|err| Error::tool("skill_hub", format!("download {name}: {err}")))?;
        if response.status() != 200 {
            return Err(Error::tool(
                "skill_hub",
                format!("download {name}: status {}", response.status()),
            ));
        }
        let content = response
            .text_limited(1024 * 1024)
            .await
            .map_err(|err| Error::tool("skill_hub", format!("download {name}: {err}")))?;
        Ok(SkillPackage {
            name: name.to_string(),
            content,
            source: "agentskills".to_string(),
        })
    }

    fn name(&self) -> &'static str {
        "agentskills"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
        "schema": "discovery/0.2.0",
        "skills": [
            {"name": "code-review", "description": "Structured review checklist", "url": "https://example.com/a/SKILL.md"},
            {"name": "release", "description": "Cut a release", "source": "custom"}
        ]
    }"#;

    #[test]
    fn parses_wrapped_index() {
        let index = AgentskillsSource::parse_index(FIXTURE, 42).expect("parse");
        assert_eq!(index.schema, "discovery/0.2.0");
        assert_eq!(index.skills.len(), 2);
        assert_eq!(index.skills[0].name, "code-review");
        assert_eq!(index.skills[0].source, "agentskills");
        assert_eq!(index.skills[1].source, "custom");
        assert_eq!(index.fetched_at_ms, 42);
    }

    #[test]
    fn parses_bare_array_index() {
        let index =
            AgentskillsSource::parse_index(r#"[{"name":"x","description":""}]"#, 1).expect("parse");
        assert_eq!(index.skills.len(), 1);
        assert_eq!(index.skills[0].source, "agentskills");
    }

    #[test]
    fn rejects_malformed_index() {
        assert!(AgentskillsSource::parse_index("{not json", 0).is_err());
        assert!(AgentskillsSource::parse_index(r#"{"schema":"x"}"#, 0).is_err());
    }
}
