//! M7 memory provider plugins.
//!
//! The builtin FTS5 bank is adapted as the first [`MemoryProvider`]; external
//! providers (mem0, supermemory, …) join the same module as they land. All
//! providers share one contract so [`crate::memory::lattice`] can fuse their
//! results without knowing the transport.
//!
//! This module also hosts the M7 *lifecycle signal* surface: each registered
//! provider carries an availability tag (`connected` / `degraded`) plus the
//! `score` / `confidence` / `last_seen_days` triple the fusion ranking reads.
//! Preflight never fails — an unavailable provider degrades and the agent's
//! main flow continues with whatever providers did answer.

pub mod builtin;
pub mod http;
pub mod mem0;
pub mod supermemory;

pub use builtin::Fts5Provider;
pub use mem0::{Mem0Config, Mem0Provider};
pub use supermemory::{SupermemoryConfig, SupermemoryProvider};

use crate::memory::{MemoryProvider, MemoryStore};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Health of one provider as observed by the last preflight/use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderAvailability {
    /// Reachable and answering.
    Connected,
    /// Not reachable, disabled, or misconfigured; requests degrade to no-ops.
    Degraded,
}

impl ProviderAvailability {
    /// Whether the provider answered the last probe.
    #[must_use]
    pub const fn is_connected(self) -> bool {
        matches!(self, Self::Connected)
    }
}

/// Lifecycle signal for one provider (ultra `MemoryProviderSignal` parity).
///
/// `score` / `confidence` feed `LatticeCandidate::fused_rank`; `last_seen_days`
/// drives the hot/warm/archive decay used by [`lifecycle_tier`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSignal {
    /// Provider name (e.g. `"builtin"`, `"mem0"`).
    pub provider: String,
    /// Whether the provider is usable.
    pub availability: ProviderAvailability,
    /// Raw provider score, higher is better.
    pub score: f64,
    /// Confidence in `(0, 1]`.
    pub confidence: f64,
    /// Days since this provider last produced a candidate.
    pub last_seen_days: u32,
    /// How many independent memories this provider contributed.
    pub evidence_count: u32,
}

impl ProviderSignal {
    /// A healthy signal at full-ish confidence with recent evidence.
    #[must_use]
    pub fn connected(provider: impl Into<String>, evidence_count: u32) -> Self {
        Self {
            provider: provider.into(),
            availability: ProviderAvailability::Connected,
            score: 1.0,
            confidence: 0.8,
            last_seen_days: 0,
            evidence_count,
        }
    }

    /// A degraded signal: the provider is registered but not answering.
    #[must_use]
    pub fn degraded(provider: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            availability: ProviderAvailability::Degraded,
            score: 0.0,
            confidence: 0.0,
            last_seen_days: u32::MAX,
            evidence_count: 0,
        }
    }

    /// Whether this signal may contribute candidates to fusion.
    #[must_use]
    pub const fn is_usable(&self) -> bool {
        self.availability.is_connected()
    }
}

/// Decay tier from the ultra lifecycle policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LifecycleTier {
    /// Recently seen or high confidence — always injected.
    Hot,
    /// Cooling down but still relevant.
    Warm,
    /// Dormant unless explicitly recalled.
    Archive,
}

/// Classify one signal into a decay tier.
///
/// Thresholds mirror the ultra snapshot: unavailable → archive; a
/// ContextLattice-backed or high-score/high-confidence/fresh provider → hot;
/// seen within a month or with repeated evidence → warm; otherwise archive.
#[must_use]
pub fn lifecycle_tier(signal: &ProviderSignal) -> LifecycleTier {
    if !signal.is_usable() {
        return LifecycleTier::Archive;
    }
    let name = signal.provider.trim().to_ascii_lowercase();
    if name == "contextlattice"
        || signal.score >= 1.20
        || signal.confidence >= 0.82
        || signal.last_seen_days <= 7
    {
        LifecycleTier::Hot
    } else if signal.last_seen_days <= 30 || signal.evidence_count >= 2 {
        LifecycleTier::Warm
    } else {
        LifecycleTier::Archive
    }
}

/// Registered + probed providers. Registry order is stable for reporting.
pub struct ProviderRegistry {
    providers: Vec<(String, Arc<dyn MemoryProvider>)>,
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderRegistry {
    /// An empty registry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    /// Register a provider under a stable name.
    pub fn register(&mut self, name: impl Into<String>, provider: Arc<dyn MemoryProvider>) {
        self.providers.push((name.into(), provider));
    }

    /// Number of registered providers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// Whether no providers are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// Registered provider names in registration order.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.providers
            .iter()
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Look up one provider by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn MemoryProvider>> {
        self.providers
            .iter()
            .find(|(registered, _)| registered == name)
            .map(|(_, provider)| Arc::clone(provider))
    }

    /// Preflight every provider: a provider that answers `list_namespaces`
    /// connects; anything slower is reported as degraded by the caller's
    /// timeout. Errors are recorded, never propagated — preflight must not
    /// block the agent (M7: degraded, not fatal).
    pub async fn preflight(&self) -> Vec<ProviderSignal> {
        let mut signals = Vec::with_capacity(self.providers.len());
        for (name, provider) in &self.providers {
            match provider.list_namespaces().await {
                Ok(namespaces) => {
                    let evidence: u32 = namespaces.len().try_into().unwrap_or(u32::MAX);
                    signals.push(ProviderSignal::connected(name.clone(), evidence));
                }
                Err(error) => {
                    tracing::debug!("memory provider {name} preflight failed: {error}");
                    signals.push(ProviderSignal::degraded(name.clone()));
                }
            }
        }
        signals
    }
}

/// Build the default registry from config + environment.
///
/// Every external provider defaults off; without explicit opt-in the registry
/// contains only the builtin bank (when a store is supplied), so existing
/// behavior is unchanged.
#[must_use]
pub fn default_registry(store: Option<Arc<MemoryStore>>) -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    if let Some(store) = store {
        registry.register("builtin", Arc::new(Fts5Provider::new(store)));
    }
    let mem0 = Mem0Config::load();
    if mem0.is_active() {
        registry.register("mem0", Arc::new(Mem0Provider::new(mem0)));
    }
    let supermemory = SupermemoryConfig::load();
    if supermemory.is_active() {
        registry.register(
            "supermemory",
            Arc::new(SupermemoryProvider::new(supermemory)),
        );
    }
    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Test double: succeeds `ok_calls` times, then starts failing.
    struct FlakyProvider {
        calls: AtomicUsize,
        ok_calls: usize,
    }

    #[async_trait::async_trait]
    impl MemoryProvider for FlakyProvider {
        async fn save(
            &self,
            _namespace: &str,
            _key: &str,
            _value: &str,
        ) -> crate::error::Result<()> {
            Ok(())
        }
        async fn load(&self, _namespace: &str, _key: &str) -> crate::error::Result<Option<String>> {
            Ok(None)
        }
        async fn list_namespaces(&self) -> crate::error::Result<Vec<String>> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call < self.ok_calls {
                Ok(vec!["project".to_string()])
            } else {
                Err(crate::error::Error::tool("flaky", "offline"))
            }
        }
        async fn delete(&self, _namespace: &str, _key: &str) -> crate::error::Result<()> {
            Ok(())
        }
    }

    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        asupersync::runtime::RuntimeBuilder::new()
            .build()
            .expect("runtime")
            .block_on(future)
    }

    #[test]
    fn empty_registry_is_empty() {
        let registry = ProviderRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn registers_multiple_providers_in_order() {
        let mut registry = ProviderRegistry::new();
        registry.register(
            "builtin",
            Arc::new(FlakyProvider {
                calls: AtomicUsize::new(0),
                ok_calls: 1,
            }),
        );
        registry.register(
            "mem0",
            Arc::new(FlakyProvider {
                calls: AtomicUsize::new(0),
                ok_calls: 1,
            }),
        );
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.names(), vec!["builtin", "mem0"]);
        assert!(registry.get("mem0").is_some());
        assert!(registry.get("missing").is_none());
    }

    #[test]
    fn preflight_reports_connected_and_degraded_without_erroring() {
        let mut registry = ProviderRegistry::new();
        registry.register(
            "healthy",
            Arc::new(FlakyProvider {
                calls: AtomicUsize::new(0),
                ok_calls: 1,
            }),
        );
        registry.register(
            "sick",
            Arc::new(FlakyProvider {
                calls: AtomicUsize::new(0),
                ok_calls: 0,
            }),
        );
        let signals = block_on(registry.preflight());
        assert_eq!(signals.len(), 2, "preflight must report every provider");
        let healthy = signals.iter().find(|s| s.provider == "healthy").unwrap();
        let sick = signals.iter().find(|s| s.provider == "sick").unwrap();
        assert!(healthy.is_usable());
        assert!(!sick.is_usable(), "a failing provider must degrade");
        assert_eq!(sick.availability, ProviderAvailability::Degraded);
    }

    #[test]
    fn degraded_signal_is_archive_tier() {
        assert_eq!(
            lifecycle_tier(&ProviderSignal::degraded("mem0")),
            LifecycleTier::Archive
        );
    }

    #[test]
    fn fresh_provider_is_hot_and_stale_low_evidence_is_archive() {
        let hot = ProviderSignal::connected("builtin", 1);
        assert_eq!(lifecycle_tier(&hot), LifecycleTier::Hot);
        let stale = ProviderSignal {
            provider: "supermemory".to_string(),
            availability: ProviderAvailability::Connected,
            score: 0.1,
            confidence: 0.2,
            last_seen_days: 90,
            evidence_count: 0,
        };
        assert_eq!(lifecycle_tier(&stale), LifecycleTier::Archive);
        let warm = ProviderSignal {
            last_seen_days: 20,
            ..stale
        };
        assert_eq!(lifecycle_tier(&warm), LifecycleTier::Warm);
    }

    #[test]
    fn default_registry_defaults_every_external_provider_off() {
        // No store and no env opt-in: registry is empty, proving providers
        // default off and existing behavior is untouched.
        let registry = default_registry(None);
        assert!(
            registry.get("mem0").is_none(),
            "mem0 must be off by default"
        );
        assert!(
            registry.get("supermemory").is_none(),
            "supermemory must be off by default"
        );
        assert!(registry.get("builtin").is_none());
    }
}
