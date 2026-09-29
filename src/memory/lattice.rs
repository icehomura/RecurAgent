//! M7 ContextLattice fusion layer.
//!
//! Aggregates candidates from multiple memory providers, scores them by
//! `score * confidence * ln(1 + evidence_count)`, and formats the fused set
//! for prompt injection. The HTTP orchestrator is optional: when disabled or
//! unreachable, `search` degrades to an empty set instead of blocking the
//! agent (degraded-not-fatal, per the M7 acceptance criteria).

use crate::error::{Error, Result};

/// Connection settings for a ContextLattice orchestrator.
#[derive(Debug, Clone)]
pub struct ContextLatticeConfig {
    /// Orchestrator base URL.
    pub endpoint: String,
    /// Master switch. Default off: no behavior change until opted in.
    pub enabled: bool,
    /// Per-request budget in milliseconds.
    pub timeout_ms: u64,
}

impl Default for ContextLatticeConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://127.0.0.1:8075".to_string(),
            enabled: false,
            timeout_ms: 3_000,
        }
    }
}

/// One memory candidate returned by a provider or the lattice itself.
#[derive(Debug, Clone)]
pub struct LatticeCandidate {
    /// Memory body to inject.
    pub content: String,
    /// Topic path organizing the candidate (e.g. `"rust/testing"`).
    pub topic_path: String,
    /// Raw provider score, higher is better.
    pub score: f64,
    /// Provider confidence in (0, 1].
    pub confidence: f64,
    /// How many independent sources corroborate this candidate.
    pub evidence_count: u32,
    /// Provider name (e.g. `"builtin"`, `"mem0"`).
    pub source: String,
}

impl LatticeCandidate {
    /// Fusion key: `score * confidence * ln(1 + evidence_count)`.
    #[must_use]
    pub fn fused_rank(&self) -> f64 {
        self.score * self.confidence * f64::from(self.evidence_count).ln_1p()
    }
}

/// ContextLattice client + fusion router.
#[derive(Debug, Clone)]
pub struct ContextLattice {
    config: ContextLatticeConfig,
}

impl ContextLattice {
    /// Create a lattice from explicit config.
    #[must_use]
    pub const fn new(config: ContextLatticeConfig) -> Self {
        Self { config }
    }

    /// Whether the orchestrator is configured to be used at all.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// Query the orchestrator for candidates.
    ///
    /// Degrades to `Ok(vec![])` when disabled or unreachable — fusion must
    /// never block the agent's main flow.
    pub async fn search(&self, query: &str, top_k: usize) -> Result<Vec<LatticeCandidate>> {
        if !self.config.enabled || query.trim().is_empty() || top_k == 0 {
            return Ok(Vec::new());
        }
        let url = format!(
            "{}/memory/search",
            self.config.endpoint.trim_end_matches('/')
        );
        let payload = serde_json::json!({ "query": query, "top_k": top_k });
        // Degrade on any transport/parse failure: fusion must never block the
        // agent's main flow.
        let client = crate::http::client::Client::new();
        let response = match client.post(&url).json(&payload) {
            Ok(builder) => builder.send().await,
            Err(_) => return Ok(Vec::new()),
        };
        let response = match response {
            Ok(response) if response.status() == 200 => response,
            _ => return Ok(Vec::new()),
        };
        let Ok(text) = response.text_limited(4 * 1024 * 1024).await else {
            return Ok(Vec::new());
        };
        let parsed: serde_json::Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(_) => return Ok(Vec::new()),
        };
        let empty = Vec::new();
        let items = parsed
            .get("candidates")
            .and_then(serde_json::Value::as_array)
            .unwrap_or(&empty);
        let mut candidates: Vec<LatticeCandidate> = items
            .iter()
            .filter_map(|item| {
                Some(LatticeCandidate {
                    content: item.get("content")?.as_str()?.to_string(),
                    topic_path: item
                        .get("topic_path")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    score: item
                        .get("score")
                        .and_then(serde_json::Value::as_f64)
                        .unwrap_or(0.5),
                    confidence: item
                        .get("confidence")
                        .and_then(serde_json::Value::as_f64)
                        .unwrap_or(0.5),
                    evidence_count: item
                        .get("evidence_count")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(1)
                        .try_into()
                        .unwrap_or(u32::MAX),
                    source: item
                        .get("source")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("lattice")
                        .to_string(),
                })
            })
            .collect();
        candidates.truncate(top_k);
        Ok(candidates)
    }

    /// Fuse candidates: re-rank globally by `fused_rank`, then keep the top
    /// few per topic path so one popular topic cannot crowd out the rest.
    #[must_use]
    pub fn fuse(&self, mut candidates: Vec<LatticeCandidate>) -> Vec<LatticeCandidate> {
        if candidates.is_empty() {
            return candidates;
        }
        candidates.sort_by(|a, b| {
            b.fused_rank()
                .partial_cmp(&a.fused_rank())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        // Keep at most 3 per topic path, preserving global rank order.
        let mut per_topic: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        candidates.retain(|candidate| {
            let count = per_topic.entry(candidate.topic_path.clone()).or_insert(0);
            *count += 1;
            *count <= 3
        });
        candidates
    }

    /// Persist one memory into the orchestrator. Errors surface to the caller
    /// (write failures are not silently swallowed).
    pub async fn write(&self, topic_path: &str, content: &str, confidence: f64) -> Result<()> {
        if !self.config.enabled {
            return Ok(());
        }
        let url = format!(
            "{}/memory/write",
            self.config.endpoint.trim_end_matches('/')
        );
        let payload = serde_json::json!({
            "topic_path": topic_path,
            "content": content,
            "confidence": confidence,
        });
        let client = crate::http::client::Client::new();
        let builder = client
            .post(&url)
            .json(&payload)
            .map_err(|err| Error::tool("memory", format!("lattice write: {err}")))?;
        let response = builder
            .send()
            .await
            .map_err(|err| Error::tool("memory", format!("lattice write: {err}")))?;
        if response.status() != 200 {
            return Err(Error::tool(
                "memory",
                format!("lattice write: status {}", response.status()),
            ));
        }
        Ok(())
    }

    /// Render fused candidates as an injectable prompt block.
    #[must_use]
    pub fn format_for_prompt(candidates: &[LatticeCandidate]) -> String {
        if candidates.is_empty() {
            return String::new();
        }
        let mut out = String::from("Fused context from memory providers:\n");
        for candidate in candidates {
            let _ = std::fmt::write(
                &mut out,
                format_args!(
                    "- [{}] {} (confidence={:.2})\n",
                    candidate.topic_path, candidate.content, candidate.confidence
                ),
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(topic: &str, score: f64, evidence: u32) -> LatticeCandidate {
        LatticeCandidate {
            content: format!("content for {topic}"),
            topic_path: topic.to_string(),
            score,
            confidence: 0.8,
            evidence_count: evidence,
            source: "builtin".to_string(),
        }
    }

    #[test]
    fn fused_rank_orders_by_score_and_evidence() {
        let high = candidate("a", 1.0, 10);
        let low = candidate("a", 0.5, 1);
        assert!(high.fused_rank() > low.fused_rank());
    }

    #[test]
    fn fuse_caps_per_topic() {
        let lattice = ContextLattice::new(ContextLatticeConfig::default());
        let mut candidates = Vec::new();
        for i in 0..5 {
            candidates.push(candidate("hot", 1.0 - f64::from(i) * 0.1, 5));
        }
        candidates.push(candidate("cold", 0.1, 1));
        let fused = lattice.fuse(candidates);
        let hot_count = fused.iter().filter(|c| c.topic_path == "hot").count();
        assert_eq!(hot_count, 3, "one topic must not crowd out the rest");
        assert!(fused.iter().any(|c| c.topic_path == "cold"));
    }

    #[test]
    fn format_is_empty_for_no_candidates() {
        assert_eq!(ContextLattice::format_for_prompt(&[]), "");
    }

    #[test]
    fn disabled_search_returns_empty_without_network() {
        let lattice = ContextLattice::new(ContextLatticeConfig::default()); // enabled=false
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .build()
            .expect("runtime");
        let result = runtime.block_on(lattice.search("rust ownership", 5));
        assert!(result.expect("degrades, never errors").is_empty());
    }

    #[test]
    fn fused_rank_matches_documented_weight() {
        // fused_rank = score * confidence * ln(1 + evidence_count)
        let candidate = LatticeCandidate {
            content: "x".to_string(),
            topic_path: "t".to_string(),
            score: 2.0,
            confidence: 0.5,
            evidence_count: 3,
            source: "builtin".to_string(),
        };
        let expected = 2.0 * 0.5 * 3.0f64.ln_1p();
        assert!((candidate.fused_rank() - expected).abs() < 1e-12);
    }

    #[test]
    fn fuse_sorts_globally_by_fused_rank() {
        let lattice = ContextLattice::new(ContextLatticeConfig::default());
        let high = candidate("a", 1.0, 10);
        let mid = candidate("b", 0.9, 3);
        let low = candidate("c", 0.2, 1);
        let fused = lattice.fuse(vec![low, high, mid]);
        let order: Vec<f64> = fused.iter().map(LatticeCandidate::fused_rank).collect();
        assert!(
            order.windows(2).all(|pair| pair[0] >= pair[1]),
            "fused output must be descending by fused_rank: {order:?}"
        );
        assert_eq!(fused[0].topic_path, "a");
    }

    #[test]
    fn unreachable_orchestrator_degrades_to_empty() {
        // enabled=true but the endpoint is unroutable: search must return an
        // empty set, never an error (degraded, not fatal).
        let lattice = ContextLattice::new(ContextLatticeConfig {
            endpoint: "http://127.0.0.1:1".to_string(),
            enabled: true,
            timeout_ms: 200,
        });
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .build()
            .expect("runtime");
        let result = runtime.block_on(lattice.search("anything", 5));
        assert!(result.expect("degrades, never errors").is_empty());
    }
}
