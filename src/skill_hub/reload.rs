//! Hot-reload coordination for M8 skills.
//!
//! Producers (e.g. a successful skill install) call [`SkillsReloadHandle::mark_dirty`].
//! The agent polls [`SkillsReloadHandle::take_dirty`] before each LLM call; when it
//! returns `true`, the skill set is reloaded and the system prompt skill section is
//! rebuilt via the existing `set_system_prompt` inject/run/restore path.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

/// Shared handle used to signal that skills need reloading.
///
/// Cloning is cheap: all clones observe the same dirty flag and version counter.
#[derive(Debug, Clone)]
pub struct SkillsReloadHandle {
    inner: Arc<ReloadInner>,
}

#[derive(Debug)]
struct ReloadInner {
    dirty: AtomicBool,
    version: AtomicU64,
}

impl SkillsReloadHandle {
    /// Creates a handle that is initially clean at version 0.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ReloadInner {
                dirty: AtomicBool::new(false),
                version: AtomicU64::new(0),
            }),
        }
    }

    /// Marks skills as needing a reload and bumps the version counter.
    pub fn mark_dirty(&self) {
        self.inner.version.fetch_add(1, Ordering::AcqRel);
        self.inner.dirty.store(true, Ordering::Release);
    }

    /// Atomically takes the dirty flag: returns `true` once per dirty period.
    pub fn take_dirty(&self) -> bool {
        self.inner.dirty.swap(false, Ordering::AcqRel)
    }

    /// Returns the current version counter.
    pub fn version(&self) -> u64 {
        self.inner.version.load(Ordering::Acquire)
    }

    /// Read-only check of the dirty flag.
    pub fn is_dirty(&self) -> bool {
        self.inner.dirty.load(Ordering::Acquire)
    }

    /// The process-wide handle. The install tool marks this one dirty and the
    /// agent loop polls the same handle before each turn, so a mark raised in
    /// one turn is observed by the next without either side sharing state
    /// through the session object.
    #[must_use]
    pub fn shared() -> Self {
        static SHARED: OnceLock<SkillsReloadHandle> = OnceLock::new();
        SHARED.get_or_init(Self::new).clone()
    }
}

impl Default for SkillsReloadHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of a skills reload attempt, carried back to the caller.
#[derive(Debug, Clone)]
pub struct SkillsReloadOutcome {
    /// Whether a reload actually ran.
    pub reloaded: bool,
    /// Number of skills loaded after the reload.
    pub skill_count: usize,
    /// Handle version at the time of the reload.
    pub version: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_is_not_dirty() {
        let handle = SkillsReloadHandle::new();
        assert!(!handle.is_dirty());
        assert!(!handle.take_dirty());
        assert_eq!(handle.version(), 0);
    }

    #[test]
    fn take_dirty_is_one_shot() {
        let handle = SkillsReloadHandle::new();
        handle.mark_dirty();
        assert!(handle.is_dirty());
        assert!(handle.take_dirty());
        assert!(!handle.take_dirty());
        assert!(!handle.is_dirty());
    }

    #[test]
    fn version_increments_on_mark_dirty() {
        let handle = SkillsReloadHandle::new();
        assert_eq!(handle.version(), 0);
        handle.mark_dirty();
        assert_eq!(handle.version(), 1);
        handle.mark_dirty();
        assert_eq!(handle.version(), 2);
    }

    #[test]
    fn clones_share_state() {
        let a = SkillsReloadHandle::new();
        let b = a.clone();
        a.mark_dirty();
        assert!(b.is_dirty());
        assert_eq!(b.version(), 1);
        assert!(b.take_dirty());
        assert!(!a.take_dirty());
    }

    #[test]
    fn shared_handle_is_a_singleton() {
        let a = SkillsReloadHandle::shared();
        let b = SkillsReloadHandle::shared();
        assert_eq!(a.version(), b.version());
        a.mark_dirty();
        assert!(b.is_dirty(), "clones of shared observe the same flag");
        // Drain so later tests in the same process are not polluted.
        let _ = b.take_dirty();
    }
}
