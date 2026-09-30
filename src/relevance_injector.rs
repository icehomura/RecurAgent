//! Cross-session relevance injection (M3).
//!
//! Retrieves top-k related memories via FTS5 and formats them for injection
//! into the system prompt at session start, with source attribution and a
//! token-budget cap.

use crate::error::Result;

/// A context fragment retrieved for injection into a new session.
#[derive(Debug, Clone)]
pub struct InjectedContext {
    /// The memory content to inject.
    pub content: String,
    /// The memory id the fragment came from (for audit).
    pub memory_id: i64,
    /// Memory kind (`fact` / `lesson` / `preference` / `decision`).
    pub kind: String,
    /// Where the memory came from (e.g. `"memory"`, `"session"`).
    pub source: String,
    /// Creation timestamp in Unix milliseconds, for audit/replay.
    pub created_at_ms: i64,
    /// Relevance score in (0, 1]; higher means more relevant.
    pub relevance_score: f64,
}

impl InjectedContext {
    /// One-line provenance header for this fragment, used both in the prompt
    /// and in the audit details.
    #[must_use]
    pub fn attribution(&self) -> String {
        format!(
            "id={} kind={} source={} created_at_ms={}",
            self.memory_id, self.kind, self.source, self.created_at_ms
        )
    }
}

/// Retrieves and formats relevant cross-session context.
///
/// Uses the existing FTS5 memory recall, ranks results by the memory ranker,
/// and truncates to `top_k` entries within a `max_tokens` budget.
#[derive(Debug, Clone)]
pub struct RelevanceInjector {
    /// Maximum approximate tokens for the formatted output.
    pub max_tokens: usize,
    /// Maximum number of context fragments to return.
    pub top_k: usize,
}

impl Default for RelevanceInjector {
    fn default() -> Self {
        Self {
            max_tokens: 2_000,
            top_k: 10,
        }
    }
}

impl RelevanceInjector {
    /// Create an injector with explicit budget parameters.
    #[must_use]
    pub const fn new(max_tokens: usize, top_k: usize) -> Self {
        Self { max_tokens, top_k }
    }

    /// Retrieve relevant memories for the given query.
    ///
    /// Returns an empty vector when the query is blank or no memories match.
    pub fn inject(
        &self,
        query: &str,
        store: &crate::memory::MemoryStore,
    ) -> Result<Vec<InjectedContext>> {
        let trimmed = query.trim();
        if trimmed.is_empty() || self.top_k == 0 {
            return Ok(Vec::new());
        }
        let memories = store.recall(trimmed, Some(self.top_k))?;
        let total = memories.len();
        let contexts = memories
            .into_iter()
            .enumerate()
            .map(|(i, m)| {
                // Linearly decreasing score: first hit gets 1.0, last gets
                // 1.0/total. Recall already sorted by relevance + recency.
                let denom = f64::from(u32::try_from(total.max(1)).unwrap_or(u32::MAX));
                InjectedContext {
                    content: m.content,
                    memory_id: m.id,
                    kind: m.kind,
                    source: "memory".to_string(),
                    created_at_ms: m.created_at_ms,
                    relevance_score: f64::from(u32::try_from(total - i).unwrap_or(u32::MAX)) / denom,
                }
            })
            .collect();
        Ok(contexts)
    }

    /// Format contexts for system-prompt injection.
    ///
    /// Respects `max_tokens` using a rough 4-chars-per-token estimate and
    /// drops trailing fragments that do not fit. The returned block is a hard
    /// cap: when even the header cannot fit, an empty string is returned so
    /// the caller never injects over-budget text.
    #[must_use]
    pub fn format_for_prompt(&self, contexts: &[InjectedContext]) -> String {
        if contexts.is_empty() {
            return String::new();
        }
        let char_budget = self.max_tokens.saturating_mul(4);
        let header = "Relevant context from previous sessions:\n";
        if header.len() > char_budget {
            return String::new();
        }
        let mut out = String::from(header);
        for ctx in contexts {
            // Source / time attribution stays on the line so every fragment is
            // auditable back to the memory row it came from.
            let line = format!("- [{}] {}\n", ctx.attribution(), ctx.content);
            if out.len() + line.len() > char_budget {
                break;
            }
            out.push_str(&line);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(content: &str, score: f64) -> InjectedContext {
        InjectedContext {
            content: content.to_string(),
            memory_id: 7,
            kind: "lesson".to_string(),
            source: "memory".to_string(),
            created_at_ms: 1_700_000_000_000,
            relevance_score: score,
        }
    }

    #[test]
    fn empty_query_returns_empty() {
        let injector = RelevanceInjector::default();
        // No store available in this unit test; blank query short-circuits
        // before touching the store.
        let store =
            crate::memory::MemoryStore::open(std::path::Path::new(".")).expect("open memory store");
        let result = injector.inject("   ", &store).expect("inject");
        assert!(result.is_empty());
    }

    #[test]
    fn zero_top_k_returns_empty() {
        let injector = RelevanceInjector::new(100, 0);
        let store =
            crate::memory::MemoryStore::open(std::path::Path::new(".")).expect("open memory store");
        assert!(
            injector
                .inject("anything", &store)
                .expect("inject")
                .is_empty()
        );
    }

    #[test]
    fn format_empty_returns_empty_string() {
        let injector = RelevanceInjector::default();
        assert_eq!(injector.format_for_prompt(&[]), "");
    }

    #[test]
    fn format_respects_token_budget() {
        // Budget 44 chars against a 41-byte header ("Relevant context from
        // previous sessions:\n"). At 10 tokens the budget is 40 and the header
        // alone overruns it, so the function returns "" and the header
        // assertion below could never hold.
        let injector = RelevanceInjector::new(11, 10);
        let contexts = vec![
            context(&"x".repeat(200), 1.0),
            context(&"y".repeat(200), 0.5),
        ];
        let out = injector.format_for_prompt(&contexts);
        // Header fits, first line may or may not; second must be dropped.
        assert!(out.len() <= 11 * 4, "output {} exceeds budget", out.len());
        assert!(out.starts_with("Relevant context"));
        assert!(!out.contains("yyyy"), "second fragment should be dropped");
    }

    #[test]
    fn header_larger_than_budget_yields_nothing() {
        // 1 token ≈ 4 chars — far too small for the header, let alone a line.
        let injector = RelevanceInjector::new(1, 10);
        assert_eq!(injector.format_for_prompt(&[context("x", 1.0)]), "");
    }

    #[test]
    fn fragments_carry_source_and_time_attribution() {
        let injector = RelevanceInjector::default();
        let out = injector.format_for_prompt(&[context("remember this", 1.0)]);
        assert!(out.contains("id=7"));
        assert!(out.contains("kind=lesson"));
        assert!(out.contains("source=memory"));
        assert!(out.contains("created_at_ms=1700000000000"));
        assert!(out.contains("remember this"));
    }

    #[test]
    fn scores_are_decreasing() {
        let contexts = [context("a", 1.0), context("b", 0.5)];
        assert!(contexts[0].relevance_score > contexts[1].relevance_score);
    }
}
