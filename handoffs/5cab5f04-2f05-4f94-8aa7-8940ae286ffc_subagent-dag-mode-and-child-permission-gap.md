# Handoff — subagent `dag` mode (DONE) + child write-permission gap (OPEN)

- **Handoff id:** 5cab5f04-2f05-4f94-8aa7-8940ae286ffc
- **Slug:** subagent-dag-mode-and-child-permission-gap
- **Repo:** C:\Users\icehomura\workspace\rust\pi_agent_rust\RecurAgent
- **Status:** dag mode shipped + verified. Child write-permission gap diagnosed, NOT fixed.

---

## 1. DONE — `subagent` `dag` mode (layered scheduling)

Commits `3f79b6506` (feature) and `d6a0a4842` (subprocess tests), both pushed.

`subagent` now has a fourth mutually exclusive mode, `dag`:
```jsonc
{"dag": [
  {"id": 1, "agent": "explore", "task": "find X"},
  {"id": 2, "agent": "verify",  "task": "judge from {{node.1.content}}", "dependsOn": [1]}
]}
```
It reuses **the same** graph model + scheduler as the `dag` tool rather than a second
implementation: `TaskGraph::build` (Kahn layering, cycle/depth/width validation),
`DagScheduler` (bounded concurrency, skip-downstream-on-failure, output store, template
resolution). Only the `NodeExecutor` differs: `SubagentDagExecutor` runs a child subagent
(`ChildRunner::run_one`) instead of a tool. A node may carry `name`/`cwd`/`isolation`/`isoApply`/
`outputSchema`/`schemaMode`; every node shares the request `deadline`.

Naming decision (asked and answered this session): the shared graph is a **Task Graph**
(`TaskGraph`/`TaskNode`, node = unit of work; the executor is pluggable), NOT an "Agent Graph".
Mode names are `single` / `parallel` / `chain` / `dag`.

### Verified
- `cargo check --lib` -> exit 0 (Windows).
- `cargo test --lib subagent` -> **57 passed, 0 failed** (Windows; includes the new
  `dag_is_a_mutually_exclusive_mode`).
- `cargo test --lib subagents::execution_tests` -> **21 passed, 0 failed**, run in **WSL**
  (Linux target, pinned `nightly-2026-08-31`) because that module is `#![cfg(unix)]` and is
  not compiled on Windows. Includes `dag_resolves_an_upstream_node_into_a_dependent_task` and
  `dag_skips_downstream_nodes_when_a_dependency_fails`.
- **Tip for the next agent:** this checkout is on Windows but WSL has the exact pinned nightly
  and a warm Linux `target/`; `wsl.exe -e bash -lc 'cd /mnt/c/Users/icehomura/workspace/rust/pi_agent_rust/RecurAgent && cargo test …'`
  is how to exercise `cfg(unix)` tests. (A cold WSL build took ~32 min; it is warm now.)

### Not done for dag mode
- No conformance fixture for `dag` yet (the `dag` tool has its own; a subagent `dag` fixture
  would need the fake-child fixture, which only the Rust test harness provides).
- `dsr quality --tool recur_agent` was not run (dsr not on PATH this session).

---

## 2. OPEN DEFECT — child subagents cannot write in the default configuration

**Symptom observed this session:** a `subagent` delegation to the writer role failed with
`RECUR_AGENT_SUBAGENT_FAILED: child emitted an error event`; the child reported every mutating
tool (`edit`/`write`/`hashline_edit`/`ast_edit`/`bash`/`run_code`) refused with
`approval required for '<tool>' but this session has no approval surface; re-run with
--approval-mode yolo …`. Read-only tools worked, so children are effectively reconnaissance-only.

**Mechanism (all verified by reading the code):**
1. `child_args` (`src/subagents.rs:1063-1113`) builds the child argv as
   `--mode json --print --no-session --tools <list> [--model] [--thinking] [--skill]
   [--append-system-prompt] "Task: …"`. It passes **no `--approval-mode`, no `--yolo`**, and
   sets no approval-related env var (`subagents.rs` contains no approval code at all).
2. Approval mode is resolved in `src/main.rs:2079-2091`: CLI flag > `config.approval_mode()`,
   whose default is `ApprovalMode::AlwaysAsk` (`src/approval.rs:579`).
3. A headless (`--print`) run has no interactive approval surface, so `AlwaysAsk` routes to
   `approval_handler_via_ask`, which fails closed with
   `"approval required for `X` but this session has no approval surface"` (`src/ask.rs:687`).
   The host then errors the run and suggests `--approval-mode yolo` (`src/main.rs:111-121`).

**Impact:** the `subagent` tool's writer roles (`implement`, and the old `general`) and any
`--tools`-bearing child cannot actually modify anything unless the operator launched the
*parent* in a way whose mode reaches the child — and today nothing reaches the child, so even
`ra --yolo` does not help (the flag is not inherited by a fresh `ra` process).

**Fix options (pick one; this is a security-posture change, so decide deliberately):**
- **(a) Pass `--approval-mode` to the child** in `child_args`, propagating the parent's
  effective mode; or at minimum `--approval-mode write` for writer children.
- **(b) Pass `--approval-mode yolo`** unconditionally for children. Rationale: the parent's
  decision to spawn an autonomous child *is* the authorization, and a child that can never
  write makes the tool's writer roles a lie. Simplest, bluntest.
- **(c) Thread an explicit "child approval mode" through `SubagentTool`/`with_paths`** so
  hermetic tests can set it and the CLI can map it from the parent.

Whatever is chosen, add a subprocess test in `src/subagents/execution_tests.rs` (the fake child
can assert the argv it received) and check no existing test pins the current argv shape.
`git grep -n "approval" src/subagents.rs` is empty today, so the surface is new.

---

## 3. Session ledger (all pushed)
- `b2c6a81fd` pending tool-card spinner -> dark yellow (Ok green / Err red).
- `db949f184` dropped the `N.` ordinal from DAG node labels.
- `6f61f6419` default prompt: tool-preference ladder + `dag` trigger + fan-out boundary.
- `533909e82` renamed the subagent parallel-mode key `tasks` -> `parallel`.
- `3f79b6506` `subagent` `dag` mode.
- `d6a0a4842` dag-mode subprocess tests.

## 4. Next agent's starting position
1. Fix the child write-permission gap (section 2) — highest value, it silently disables a
   shipped tool's writer roles.
2. Optionally add a `dag`-mode conformance fixture.
3. Run `dsr quality --tool recur_agent` to close the authoritative gate.
4. Never `git add -A`: this checkout always carries several sessions' dirty files
   (`README.md`, `src/interactive/tree_ui.rs`, `src/subagents.rs` non-dag hunks, `.beads/*`).
