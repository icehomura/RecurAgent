//! Builtin FTS5 memory bank exposed as a [`MemoryProvider`].
//!
//! Delegates to the existing [`MemoryStore`] rather than reimplementing
//! storage: secret screening, audit rows, supersession and the transaction
//! guard all stay in one place (DRY). The namespace/key pair rides in the
//! memory's tag list under a `m7:` prefix, which keeps KV lookup out of the
//! content field and leaves FTS ranking untouched.

use crate::error::Result;
use crate::memory::{MemoryEditOp, MemoryKind, MemoryProvider, MemoryStore};
use std::sync::Arc;

/// Tag prefix marking a row as a provider KV entry.
const TAG_PREFIX: &str = "m7";

/// The builtin FTS5 bank behind the provider trait.
pub struct Fts5Provider {
    store: Arc<MemoryStore>,
}

impl Fts5Provider {
    /// Wrap an open memory store.
    #[must_use]
    pub const fn new(store: Arc<MemoryStore>) -> Self {
        Self { store }
    }

    /// Canonical tag for one KV slot.
    fn slot_tag(namespace: &str, key: &str) -> String {
        format!("{TAG_PREFIX}:{namespace}/{key}")
    }

    /// Locate an active row for a KV slot, newest first.
    fn find_slot(&self, namespace: &str, key: &str) -> Result<Option<i64>> {
        let tag = Self::slot_tag(namespace, key);
        let entries = self.store.list(10_000)?;
        Ok(entries
            .iter()
            .filter(|entry| entry.status == "active" && entry.tags.contains(&tag))
            .map(|entry| entry.id)
            .next())
    }
}

#[async_trait::async_trait]
impl MemoryProvider for Fts5Provider {
    async fn save(&self, namespace: &str, key: &str, value: &str) -> Result<()> {
        let tag = Self::slot_tag(namespace, key);
        if let Some(id) = self.find_slot(namespace, key)? {
            self.store.edit(id, MemoryEditOp::Update, Some(value))
        } else {
            self.store.retain(MemoryKind::Fact, value, &[tag], None)?;
            Ok(())
        }
    }

    async fn load(&self, namespace: &str, key: &str) -> Result<Option<String>> {
        let Some(id) = self.find_slot(namespace, key)? else {
            return Ok(None);
        };
        let entries = self.store.list(10_000)?;
        Ok(entries
            .into_iter()
            .find(|entry| entry.id == id && entry.status == "active")
            .map(|entry| entry.content))
    }

    async fn list_namespaces(&self) -> Result<Vec<String>> {
        let entries = self.store.list(10_000)?;
        let mut namespaces: Vec<String> = entries
            .iter()
            .filter(|entry| entry.status == "active")
            .flat_map(|entry| entry.tags.iter())
            .filter_map(|tag| {
                tag.strip_prefix(&format!("{TAG_PREFIX}:"))
                    .and_then(|rest| rest.split_once('/'))
                    .map(|(namespace, _)| namespace.to_string())
            })
            .collect();
        namespaces.sort();
        namespaces.dedup();
        if !namespaces.iter().any(|namespace| namespace == "project") {
            namespaces.push("project".to_string());
        }
        Ok(namespaces)
    }

    async fn delete(&self, namespace: &str, key: &str) -> Result<()> {
        let Some(id) = self.find_slot(namespace, key)? else {
            return Ok(());
        };
        self.store.edit(id, MemoryEditOp::Forget, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> (tempfile::TempDir, Fts5Provider) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(MemoryStore::open(dir.path()).expect("open store"));
        (dir, Fts5Provider::new(store))
    }

    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        asupersync::runtime::RuntimeBuilder::new()
            .build()
            .expect("runtime")
            .block_on(future)
    }

    #[test]
    fn save_load_roundtrip() {
        let (_dir, provider) = provider();
        block_on(async {
            provider
                .save("project", "greeting", "hello")
                .await
                .expect("save");
            let loaded = provider.load("project", "greeting").await.expect("load");
            assert_eq!(loaded.as_deref(), Some("hello"));
        });
    }

    #[test]
    fn overwrite_replaces_value_in_place() {
        let (_dir, provider) = provider();
        block_on(async {
            provider.save("project", "k", "v1").await.expect("save v1");
            provider.save("project", "k", "v2").await.expect("save v2");
            let loaded = provider.load("project", "k").await.expect("load");
            assert_eq!(loaded.as_deref(), Some("v2"));
        });
    }

    #[test]
    fn delete_removes_slot_and_absent_delete_is_ok() {
        let (_dir, provider) = provider();
        block_on(async {
            provider.save("project", "gone", "x").await.expect("save");
            provider.delete("project", "gone").await.expect("delete");
            let loaded = provider.load("project", "gone").await.expect("load");
            assert!(loaded.is_none());
            provider
                .delete("project", "never-existed")
                .await
                .expect("idempotent");
        });
    }

    #[test]
    fn list_namespaces_includes_project_even_when_empty() {
        let (_dir, provider) = provider();
        block_on(async {
            let namespaces = provider.list_namespaces().await.expect("list");
            assert!(namespaces.iter().any(|n| n == "project"));
        });
    }
}
