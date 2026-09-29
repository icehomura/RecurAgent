//! Reflection hooks: gate-controlled reflection triggers with frequency
//! limiting. Wraps [`ReflectionTrigger`] from the memory subsystem and adds
//! a global hourly budget plus a simple enabled/disabled toggle.
//!
//! The hook owns a *stateful* [`ReflectionTrigger`] so a repeated workflow
//! (the same tool-call shape seen `same_task_threshold` times) can trigger a
//! reflection even when no single task exceeded the tool-call threshold.
//! [`ReflectionHooks::note_task`] feeds that state; `should_trigger` never
//! mutates it, so the decision is a pure read of already-recorded evidence.

use crate::memory::reflection::{ReflectionTrigger, task_shape};
use crate::memory::{MemoryKind, MemoryStore};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::time::Instant;

/// Maximum bytes of the deterministic lesson body written when no model is
/// bound. Bounds the memory row the same way the reflection tool bounds its
/// answer budget.
const MAX_LESSON_BYTES: usize = 2 * 1024;

/// Configuration for the reflection hook system.
///
/// Reflection is enabled by default (`enabled = true`). Pass
/// `--disable-reflection` on the CLI or set `enabled: false` in settings to
/// opt out. The hourly budget (`max_reflections_per_hour`) acts as a cost
/// governor.
#[derive(Debug, Clone)]
pub struct ReflectionConfig {
    /// Master switch – reflection is completely skipped when `false`.
    pub enabled: bool,
    /// Tool-call count at which a task is considered "complex" enough to
    /// warrant reflection. Forwarded to [`ReflectionTrigger::new`].
    pub tool_calls_threshold: usize,
    /// Number of repeated same-type tasks that trigger automatic reflection.
    /// Forwarded to [`ReflectionTrigger::new`] and enforced against the
    /// per-shape counters fed by [`ReflectionHooks::note_task`].
    pub same_task_threshold: usize,
    /// Hard cap on reflections within any rolling 60-minute window. Acts as
    /// a cost governor; once reached, `should_trigger` returns `false` until
    /// the window rolls over.
    pub max_reflections_per_hour: usize,
    /// When `true`, reflection uses the main (session) model. When `false`,
    /// a dedicated smaller model can be used instead. The field is advisory –
    /// the actual model selection lives in the caller.
    pub use_main_model: bool,
}

impl Default for ReflectionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            tool_calls_threshold: 5,
            same_task_threshold: 3,
            max_reflections_per_hour: 10,
            use_main_model: true,
        }
    }
}

/// Why a reflection was deemed worthwhile, with the evidence behind it.
///
/// This is the auditable half of the hook: it records the trigger reason and
/// the tool-call shape so a lesson can be synthesized later without the
/// original conversation.
#[derive(Debug, Clone)]
pub struct ReflectionOutcome {
    /// Machine reason: `"complex_task"`, `"user_correction"`, or
    /// `"repeated_task"`.
    pub reason: String,
    /// Number of tool calls observed in the completed task.
    pub tool_call_count: usize,
    /// Execution path derived from the tool names (e.g. `read→edit→bash`).
    pub task_shape: Option<String>,
    /// Whether a user-correction signal was present.
    pub correction_signal: bool,
    /// How many times this exact shape has been observed this session.
    pub repeated_occurrences: usize,
}

/// Manages reflection hook registration and invocation.
///
/// The manager is cheaply cloneable via [`Arc`] and tracks the number of
/// reflections triggered in the current hour window. When the window rolls
/// over, the counter is reset automatically.
#[derive(Debug, Clone)]
pub struct ReflectionHooks {
    config: ReflectionConfig,
    reflection_count_this_hour: Arc<AtomicUsize>,
    last_hour_start: Arc<Mutex<Instant>>,
    /// Stateful trigger: counts completed task shapes across the session.
    /// Held as a field (not rebuilt per call) so `same_task_threshold` has
    /// real semantics.
    trigger: Arc<Mutex<ReflectionTrigger>>,
}

impl ReflectionHooks {
    /// Create a new hook manager from the given configuration.
    #[must_use]
    pub fn new(config: ReflectionConfig) -> Self {
        let trigger =
            ReflectionTrigger::new(config.tool_calls_threshold, config.same_task_threshold);
        Self {
            config,
            reflection_count_this_hour: Arc::new(AtomicUsize::new(0)),
            last_hour_start: Arc::new(Mutex::new(Instant::now())),
            trigger: Arc::new(Mutex::new(trigger)),
        }
    }

    /// The configuration this hook was built with.
    #[must_use]
    pub const fn config(&self) -> &ReflectionConfig {
        &self.config
    }

    /// Record a completed task's tool-call shape so repeated workflows can be
    /// detected. Call this once per finished task, before [`Self::should_trigger`].
    pub fn note_task(&self, tool_names: &[String]) {
        if !self.config.enabled {
            return;
        }
        let shape = task_shape(tool_names);
        let mut trigger = self
            .trigger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        trigger.observe_task(shape.as_deref());
    }

    /// Decide whether a reflection should fire for the current task context.
    ///
    /// Returns `false` immediately when `enabled` is `false` or the hourly
    /// budget is exhausted. Otherwise evaluates the session's stateful
    /// trigger: complex task, user correction, or a repeatedly observed shape.
    #[must_use]
    pub fn should_trigger(&self, tool_call_count: usize, correction_signal: bool) -> bool {
        self.decide(tool_call_count, correction_signal, None)
            .is_some()
    }

    /// Like [`Self::should_trigger`] but returns the full evidence.
    ///
    /// `task_shape` overrides the shape recorded by [`Self::note_task`]; pass
    /// `Some(shape)` when the caller already knows the execution path.
    #[must_use]
    pub fn decide(
        &self,
        tool_call_count: usize,
        correction_signal: bool,
        task_shape: Option<&str>,
    ) -> Option<ReflectionOutcome> {
        if !self.config.enabled {
            return None;
        }
        self.reset_hourly_counter_if_needed();
        if self.reflection_count_this_hour.load(Ordering::Relaxed)
            >= self.config.max_reflections_per_hour
        {
            return None;
        }
        // Single lock acquisition: read the shape's observation count and
        // evaluate against the same snapshot.
        let (shape, occurrences, decision) = {
            let trigger = self
                .trigger
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let shape = task_shape.map(str::to_string);
            let occurrences = shape
                .as_deref()
                .map_or(0, |shape| trigger.observed_count(shape));
            let decision = trigger.evaluate(
                tool_call_count,
                shape.as_deref().unwrap_or(""),
                correction_signal,
            );
            drop(trigger);
            (shape, occurrences, decision)
        };
        if !decision.should_reflect {
            return None;
        }
        Some(ReflectionOutcome {
            reason: decision.reason,
            tool_call_count,
            task_shape: shape,
            correction_signal,
            repeated_occurrences: occurrences,
        })
    }

    /// Record that a reflection has occurred, incrementing the hourly counter.
    pub fn record_reflection(&self) {
        self.reflection_count_this_hour
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Reset the hourly counter if the current window has elapsed (60 minutes).
    fn reset_hourly_counter_if_needed(&self) {
        let mut last_start = self.last_hour_start.lock().unwrap();
        if last_start.elapsed() >= Duration::from_secs(3600) {
            self.reflection_count_this_hour.store(0, Ordering::Relaxed);
            *last_start = Instant::now();
        }
    }
}

/// Build the deterministic lesson body for a reflection outcome.
///
/// This is the fallback used when no provider is bound for reflection (and
/// the default in tests): it produces a real, auditable lesson from the
/// execution evidence rather than invoking a model. The wording is stable so
/// `MemoryStore::retain`'s exact-duplicate check collapses repeats instead of
/// piling up near-identical rows.
#[must_use]
pub fn build_deterministic_lesson(outcome: &ReflectionOutcome) -> String {
    let shape = outcome.task_shape.as_deref().unwrap_or("(no tools)");
    let mut body = format!(
        "Lesson (auto-reflection, reason={}): task with {} tool call(s) followed the workflow {}. \
         Reuse this workflow when a similar request recurs; it completed without an explicit user correction.",
        outcome.reason, outcome.tool_call_count, shape
    );
    if outcome.correction_signal {
        body.push_str(" A user-correction signal was present, so verify assumptions before repeating the steps.");
    }
    if outcome.reason == "repeated_task" {
        // Push the pieces directly: `push_str(&format!(..))` reallocates twice
        // and trips clippy's `format_push_string`.
        body.push_str(" This workflow has now been observed ");
        body.push_str(&outcome.repeated_occurrences.to_string());
        body.push_str(" time(s), so promote it to a reusable procedure.");
    }
    truncate_lesson(&mut body);
    body
}

/// Truncate a lesson body to [`MAX_LESSON_BYTES`] on a UTF-8 boundary.
fn truncate_lesson(body: &mut String) {
    if body.len() <= MAX_LESSON_BYTES {
        return;
    }
    let mut end = MAX_LESSON_BYTES;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    body.truncate(end);
}

/// Tags attached to an auto-reflected lesson, used for provenance and for
/// cheap filtering by later recall.
#[must_use]
pub fn reflection_tags(outcome: &ReflectionOutcome) -> Vec<String> {
    let mut tags = vec!["auto-reflection".to_string(), outcome.reason.clone()];
    if let Some(shape) = &outcome.task_shape {
        tags.push(format!("workflow:{shape}"));
    }
    tags
}

/// Persist a reflection outcome as a `lesson` memory.
///
/// Deduplication is delegated to [`MemoryStore::retain`], which rejects an
/// exactly-equal active row with `RECUR_AGENT_MEMORY_DUPLICATE`. A duplicate is not an
/// error here — it means the lesson is already known, which is exactly the
/// desired "same lesson is not written twice" behavior.
///
/// # Errors
/// Store failures other than an exact duplicate.
pub fn persist_lesson(
    store: &MemoryStore,
    outcome: &ReflectionOutcome,
    session_id: Option<&str>,
) -> crate::error::Result<Option<crate::memory::Memory>> {
    let body = build_deterministic_lesson(outcome);
    let tags = reflection_tags(outcome);
    match store.retain(MemoryKind::Lesson, &body, &tags, session_id) {
        Ok(memory) => Ok(Some(memory)),
        Err(error) if error.to_string().contains("RECUR_AGENT_MEMORY_DUPLICATE") => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(tools: &[&str]) -> Vec<String> {
        tools.iter().map(|t| (*t).to_string()).collect()
    }

    fn hooks_with(threshold: usize, same_task: usize) -> ReflectionHooks {
        ReflectionHooks::new(ReflectionConfig {
            enabled: true,
            tool_calls_threshold: threshold,
            same_task_threshold: same_task,
            ..ReflectionConfig::default()
        })
    }

    #[test]
    fn disabled_config_never_triggers() {
        let hooks = ReflectionHooks::new(ReflectionConfig {
            enabled: false,
            ..ReflectionConfig::default()
        });
        assert!(!hooks.should_trigger(100, true));
    }

    #[test]
    fn hourly_cap_blocks_after_limit() {
        let hooks = ReflectionHooks::new(ReflectionConfig {
            enabled: true,
            tool_calls_threshold: 1,
            max_reflections_per_hour: 2,
            ..ReflectionConfig::default()
        });
        // First two calls should trigger (threshold met with tool_call_count=2).
        assert!(hooks.should_trigger(2, false));
        hooks.record_reflection();
        assert!(hooks.should_trigger(2, false));
        hooks.record_reflection();
        // Third call should be blocked by the hourly cap.
        assert!(!hooks.should_trigger(2, false));
    }

    #[test]
    fn triggers_on_correction_signal() {
        let hooks = ReflectionHooks::new(ReflectionConfig {
            enabled: true,
            tool_calls_threshold: 10,
            ..ReflectionConfig::default()
        });
        // tool_call_count=1 < threshold=10, but correction_signal=true.
        assert!(hooks.should_trigger(1, true));
    }

    #[test]
    fn triggers_on_complex_task() {
        let hooks = ReflectionHooks::new(ReflectionConfig {
            enabled: true,
            tool_calls_threshold: 3,
            ..ReflectionConfig::default()
        });
        // tool_call_count=5 >= threshold=3.
        assert!(hooks.should_trigger(5, false));
    }

    #[test]
    fn simple_task_below_threshold_does_not_trigger() {
        let hooks = hooks_with(5, 3);
        assert!(!hooks.should_trigger(4, false));
        let outcome = hooks.decide(4, false, None);
        assert!(outcome.is_none());
    }

    #[test]
    fn repeated_same_task_shape_triggers_without_complexity() {
        let hooks = hooks_with(5, 3);
        for _ in 0..3 {
            hooks.note_task(&names(&["read", "edit", "bash"]));
        }
        // Two tool calls only – far below the complexity threshold – but the
        // third observation of the same workflow fires a reflection.
        let outcome = hooks
            .decide(2, false, Some("read→edit→bash"))
            .expect("repeated task must trigger");
        assert_eq!(outcome.reason, "repeated_task");
        assert_eq!(outcome.repeated_occurrences, 3);
        assert_eq!(outcome.task_shape.as_deref(), Some("read→edit→bash"));
    }

    #[test]
    fn task_shape_counter_is_stateful_across_calls() {
        let hooks = hooks_with(5, 3);
        assert!(hooks.decide(1, false, Some("a→b")).is_none());
        hooks.note_task(&names(&["a", "b"]));
        hooks.note_task(&names(&["a", "b"]));
        // only 2 observations recorded so far
        assert!(hooks.decide(1, false, Some("a→b")).is_none());
        hooks.note_task(&names(&["a", "b"]));
        assert!(hooks.decide(1, false, Some("a→b")).is_some());
    }

    #[test]
    fn deterministic_lesson_is_stable_and_tagged() {
        let outcome = ReflectionOutcome {
            reason: "complex_task".to_string(),
            tool_call_count: 6,
            task_shape: Some("read→edit".to_string()),
            correction_signal: false,
            repeated_occurrences: 0,
        };
        let first = build_deterministic_lesson(&outcome);
        let second = build_deterministic_lesson(&outcome);
        assert_eq!(first, second, "lesson wording must be stable for dedupe");
        assert!(first.contains("read→edit"));
        assert!(first.contains("complex_task"));
        let tags = reflection_tags(&outcome);
        assert!(tags.contains(&"auto-reflection".to_string()));
        assert!(tags.contains(&"workflow:read→edit".to_string()));
    }

    #[test]
    fn lesson_is_bounded() {
        let outcome = ReflectionOutcome {
            reason: "x".repeat(MAX_LESSON_BYTES * 2),
            tool_call_count: 1,
            task_shape: None,
            correction_signal: false,
            repeated_occurrences: 0,
        };
        assert!(build_deterministic_lesson(&outcome).len() <= MAX_LESSON_BYTES);
    }

    #[test]
    fn notes_are_ignored_while_disabled() {
        let hooks = ReflectionHooks::new(ReflectionConfig {
            enabled: false,
            tool_calls_threshold: 5,
            same_task_threshold: 1,
            ..ReflectionConfig::default()
        });
        for _ in 0..5 {
            hooks.note_task(&names(&["a"]));
        }
        assert!(!hooks.should_trigger(1, false));
    }
}
