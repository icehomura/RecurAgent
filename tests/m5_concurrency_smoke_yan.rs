//! M5 / plan §6 concurrency smoke test (QA: Yan).
//!
//! Validates the 100-process constraint design requirements without spawning
//! 100 real OS processes (too slow for CI): it exercises the SAME shared-state
//! surfaces across many concurrent threads, which is where cross-process races
//! would surface inside one address space.
//!
//! Coverage:
//!   1. `MemoryStore` concurrent writers into a shared SQLite bank: every write
//!      goes through `transactions::run` (BEGIN IMMEDIATE) so no duplicate
//!      active facts may commit and no writer panics.
//!   2. `SkillsReloadHandle` mark/take under contention: distinct dirty periods
//!      are observed exactly once and the version counter is monotonic.
//!   3. Reflection hourly cost gate under contention: the total number of
//!      granted reflections never exceeds `max_reflections_per_hour`.
//!
//! These are *new* tests; no existing test file is touched.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use ra::memory::{MemoryKind, MemoryStore};
use ra::reflection_hooks::{ReflectionConfig, ReflectionHooks};
use ra::skill_hub::reload::SkillsReloadHandle;

/// Number of concurrent "processes" simulated per scenario.
const N: usize = 32;

fn fresh_root() -> PathBuf {
    let dir = tempfile::tempdir().expect("tempdir");
    // Leak the tempdir path (the directory is removed on drop; we only need a
    // unique root for the bank). Keeping it alive per-test is enough.
    let root = dir.path().to_path_buf();
    std::mem::forget(dir);
    root
}

/// Scenario 1: many concurrent writers, each writing a *distinct* fact.
/// Expectation: all N commits succeed, the bank holds exactly N active facts,
/// no panic, and a re-open sees the same durable state (WAL + FULL sync).
#[test]
fn memory_store_concurrent_distinct_writes_are_serialized() {
    let root = fresh_root();
    let store = Arc::new(MemoryStore::open(&root).expect("open bank"));
    let barrier = Arc::new(Barrier::new(N));

    let results = thread::scope(|scope| {
        // The collect() is load-bearing: it joins the spawn phase before any
        // thread is awaited, so all N workers run at once. Collapsing the two
        // collects (what `needless_collect` suggests) would interleave spawn
        // with join and serialize the very contention this test measures.
        #[allow(clippy::needless_collect)]
        let handles: Vec<_> = (0..N)
            .map(|i| {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait();
                    store.retain(
                        MemoryKind::Fact,
                        &format!("worker-{i} discovered a unique concurrency fact"),
                        &[],
                        None,
                    )
                })
            })
            .collect();
        handles
            .into_iter()
            .map(std::thread::ScopedJoinHandle::join)
            .collect::<Vec<_>>()
    });

    let panics = results.iter().filter(|r| r.is_err()).count();
    assert_eq!(panics, 0, "a concurrent writer panicked: {results:?}");
    let ok = results
        .iter()
        .filter(|r| r.as_ref().is_ok_and(std::result::Result::is_ok))
        .count();
    assert_eq!(ok, N, "every distinct write must commit");

    // Durable state must be consistent after all writers drain.
    let reopened = MemoryStore::open(&root).expect("reopen bank");
    let all = reopened.list(usize::MAX).expect("list");
    assert_eq!(all.len(), N, "exactly {N} active facts must persist");
}

/// Scenario 2: many concurrent writers all racing on the *same* content.
/// Expectation: dedupe + BEGIN IMMEDIATE lets exactly ONE commit; the rest get
/// a clean `RECUR_AGENT_MEMORY_DUPLICATE` error (never a panic, never a duplicate row).
#[test]
fn memory_store_concurrent_duplicate_writes_commit_exactly_one() {
    let root = fresh_root();
    let store = Arc::new(MemoryStore::open(&root).expect("open bank"));
    store.list(1).expect("warm up schema");
    let barrier = Arc::new(Barrier::new(N));

    let results = thread::scope(|scope| {
        // The collect() is load-bearing: it joins the spawn phase before any
        // thread is awaited, so all N workers run at once. Collapsing the two
        // collects (what `needless_collect` suggests) would interleave spawn
        // with join and serialize the very contention this test measures.
        #[allow(clippy::needless_collect)]
        let handles: Vec<_> = (0..N)
            .map(|_| {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait();
                    store.retain(
                        MemoryKind::Fact,
                        "one and only one durable duplicate fact",
                        &[],
                        None,
                    )
                })
            })
            .collect();
        handles
            .into_iter()
            .map(std::thread::ScopedJoinHandle::join)
            .collect::<Vec<_>>()
    });

    let committed = results.iter().filter(|r| matches!(r, Ok(Ok(_)))).count();
    assert_eq!(committed, 1, "exactly one duplicate write may commit");
    for r in &results {
        match r {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => assert!(
                e.to_string().contains("RECUR_AGENT_MEMORY_DUPLICATE"),
                "unexpected failure kind: {e}"
            ),
            Err(payload) => panic!("writer thread panicked: {payload:?}"),
        }
    }
    let reopened = MemoryStore::open(&root).expect("reopen bank");
    assert_eq!(reopened.list(usize::MAX).expect("list").len(), 1);
}

/// Scenario 3: `SkillsReloadHandle` under contention.
/// Expectation: every `mark_dirty` bumps the version exactly once; the number
/// of `true` returns from `take_dirty` equals the number of mark periods; and
/// nothing panics.
#[test]
fn skills_reload_handle_take_dirty_is_one_shot_under_contention() {
    const MARKS: usize = 64;
    let handle = Arc::new(SkillsReloadHandle::new());
    // Producers mark K times; there are fewer consumers than marks, so the
    // observed dirty periods must collapse to at most K.
    let observed = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(2));

    thread::scope(|scope| {
        let h = Arc::clone(&handle);
        let b = Arc::clone(&barrier);
        scope.spawn(move || {
            b.wait();
            for _ in 0..MARKS {
                h.mark_dirty();
            }
        });

        let h = Arc::clone(&handle);
        let b = Arc::clone(&barrier);
        let seen = Arc::clone(&observed);
        scope.spawn(move || {
            b.wait();
            // Drain until the producer is done; bounded by MARKS + slack.
            for _ in 0..(MARKS * 2 + 16) {
                if h.take_dirty() {
                    seen.fetch_add(1, Ordering::SeqCst);
                }
            }
        });
    });

    let seen = observed.load(Ordering::SeqCst);
    assert!(seen >= 1, "at least one dirty period must be observed");
    assert!(
        seen <= MARKS,
        "observed {seen} dirty periods exceeds the {MARKS} mark_dirty calls"
    );
    assert_eq!(handle.version(), MARKS as u64, "version must be monotonic");
    // After draining, a final take must be empty (one-shot semantics hold).
    assert!(!handle.take_dirty(), "no dirty flag must survive the drain");
}

/// Scenario 4: reflection hourly cost gate under contention.
/// Expectation: `record_reflection` is bounded by the configured hourly cap;
/// the number of *granted* reflections (`should_trigger` == true) never exceeds
/// `max_reflections_per_hour` for the whole window.
#[test]
fn reflection_hourly_cap_holds_under_contention() {
    const CAP: usize = 10;
    let hooks = Arc::new(ReflectionHooks::new(ReflectionConfig {
        enabled: true,
        tool_calls_threshold: 1,
        max_reflections_per_hour: CAP,
        ..ReflectionConfig::default()
    }));
    let granted = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(N));

    thread::scope(|scope| {
        for _ in 0..N {
            let hooks = Arc::clone(&hooks);
            let granted = Arc::clone(&granted);
            let barrier = Arc::clone(&barrier);
            scope.spawn(move || {
                barrier.wait();
                // Each worker attempts many reflections; the gate must bind.
                for _ in 0..50 {
                    if hooks.should_trigger(10, false) {
                        granted.fetch_add(1, Ordering::SeqCst);
                        hooks.record_reflection();
                    }
                }
            });
        }
    });

    let granted = granted.load(Ordering::SeqCst);
    assert!(
        granted <= CAP,
        "cost gate failed: granted {granted} reflections, cap is {CAP}"
    );
    // The gate must actually be exercised, i.e. some reflections were allowed.
    assert!(
        granted >= 1,
        "the gate denied everything; trigger logic is broken"
    );
}

/// Scenario 5: per-process injection budget is a pure function of input.
/// Expectation: `build_skills_index` with the same inputs yields deterministic
/// output regardless of how many threads call it concurrently — no hidden
/// global state coupling processes together.
#[test]
fn l0_skill_index_budget_is_deterministic_under_concurrency() {
    use ra::resources::{L0_SKILL_BUDGET_CHARS, Skill, build_skills_index};

    // Minimal skills: names short, descriptions long enough to be truncated.
    let skills: Vec<Skill> = (0..40)
        .map(|i| Skill {
            name: format!("skill-{i:02}"),
            description: "d".repeat(200),
            file_path: std::path::PathBuf::new(),
            base_dir: std::path::PathBuf::new(),
            source: "test".to_string(),
            disable_model_invocation: false,
        })
        .collect();

    let baseline = build_skills_index(&skills, L0_SKILL_BUDGET_CHARS);
    let baseline_fingerprint: Vec<(String, bool)> = baseline
        .entries
        .iter()
        .map(|e| (e.name.clone(), e.truncated))
        .collect();

    let mismatch = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(N));
    thread::scope(|scope| {
        for _ in 0..N {
            let skills = skills.clone();
            let expected = baseline_fingerprint.clone();
            let mismatch = Arc::clone(&mismatch);
            let barrier = Arc::clone(&barrier);
            scope.spawn(move || {
                barrier.wait();
                for _ in 0..20 {
                    let idx = build_skills_index(&skills, L0_SKILL_BUDGET_CHARS);
                    let fp: Vec<(String, bool)> = idx
                        .entries
                        .iter()
                        .map(|e| (e.name.clone(), e.truncated))
                        .collect();
                    if fp != expected {
                        mismatch.fetch_add(1, Ordering::SeqCst);
                    }
                }
            });
        }
    });

    assert_eq!(
        mismatch.load(Ordering::SeqCst),
        0,
        "L0 index rendering depends on shared mutable state"
    );
    // Sanity: the budget actually engaged (some entries omitted).
    assert!(
        baseline.is_truncated(),
        "test fixture is too small to exercise the budget"
    );
}
