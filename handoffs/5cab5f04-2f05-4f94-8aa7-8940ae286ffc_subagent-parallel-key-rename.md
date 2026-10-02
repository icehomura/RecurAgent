# Handoff — `subagent` parallel-key rename (done) + `graph` DAG mode (unstarted)

- **Handoff id:** 5cab5f04-2f05-4f94-8aa7-8940ae286ffc
- **Slug:** subagent-parallel-key-rename
- **Repo:** C:\Users\icehomura\workspace\rust\pi_agent_rust\RecurAgent
- **Written because:** tool-iteration budget reached the >=80% handoff threshold.

---

## 1. DONE and PUSHED — `tasks` -> `parallel` rename

Commit `533909e82` `refactor(subagent): rename the parallel-mode key from tasks to parallel`
(7 files, 30 insertions / 30 deletions). Pushed to `origin/main`.

What changed: the native `subagent` tool's parallel-mode wire key `tasks` (and the Rust field
serde maps it to) is now `parallel`. The mode's *reported* name was already `"parallel"`
(`mode_name()`), so only the input key + error text changed. Sites updated:
`src/subagents.rs` (schema property, `SubagentRequest.parallel`, `mode()` reads, the
`parallel must contain 1-8 entries` and `Provide exactly one of agent+task, parallel, or chain`
messages, 2 unit tests), `src/interactive/agent.rs` (the `ToolInvocationRenderer::Subagent`
summary reads `parallel`; 2 tests), `src/app.rs` (prompt prose), `src/subagents/deadline_tests.rs`,
`src/subagents/execution_tests.rs`, `tests/conformance/fixtures/subagent_tool.json`,
`tests/worktree_iso.rs`.

**Deliberately NOT touched** (other owners / legacy evidence):
- `src/todo.rs`, `tests/conformance/fixtures/todo_tool.json` — the `todo` tool has its own
  unrelated `tasks` key. Verified still 10 `"tasks"` occurrences in `src/todo.rs`.
- `tests/ext_conformance/ts_oracle/dynamic_validation_results.json` — legacy TS oracle snapshot.
- `src/extensions_js.rs` (`tasks:` in a `pijs_pi_subagents_0_34_0` validator fixture with a
  `config` field) — proven unrelated: that test passes unchanged.

### Verification actually run (all green)
- `cargo check --lib` -> exit 0.
- `cargo test --lib subagent` -> **56 passed, 0 failed**.
- `cargo test --lib default_system_prompt` -> **6 passed, 0 failed**.
- `cargo test --lib subagent` includes
  `extensions_js::tests::pijs_pi_subagents_0_34_0_import_contract_and_validation_semantics` -> ok,
  which is what proves the legacy fixture is not the native schema.
- **NOT run:** the full `cargo test --test conformance_fixtures` (the run that consumes
  `tests/conformance/fixtures/subagent_tool.json`) was interrupted at the budget limit; a filtered
  `--test conformance_fixtures subagent` matched 0 tests (the harness is data-driven, so filter by
  nothing and run the whole target). Also `dsr quality --tool recur_agent` was not run (dsr not on
  PATH this session). **Do these before calling the rename fully gate-verified.**

### Note on committing
`src/subagents.rs` contained **another session's uncommitted work** (renaming the built-in
`general` agent to `implement`, giving `explore` a shell, new prompts/tests). The rename was staged
**hunk-by-hunk** (only hunks containing `parallel`) so none of that was swept. `src/interactive/
model_selector_ui.rs`, `README.md`, `locales/messages.yml`, `scripts/check_i18n_keys.py` are still
dirty from other sessions — leave them.

---

## 2. NOT STARTED — `subagent` `graph` (DAG) scheduling mode

The user asked for a DAG mode on the `subagent` tool: layered scheduling where each node is a
subagent task, the same way the `dag` tool layers tool calls.

The scheduler is already generic, so this is **one new `NodeExecutor`**, not a new scheduler:

- `src/task_dag.rs` — `TaskNode { id: TaskNodeId, tool_name, name, args, depends_on, effects }`,
  `TaskGraph::build()` does Kahn layering + cycle detection. Bounds `MAX_DAG_NODES=256`,
  `MAX_DAG_DEPTH=100`, `MAX_LAYER_WIDTH=64`; `FORBIDDEN_NODE_TOOLS=["dag"]` (subagent NOT forbidden).
- `src/dag_scheduler.rs` — `trait NodeExecutor { fn execute(&self, node: TaskNode, resolved_args:
  Value) -> Pin<Box<dyn Future<Output=Result<ToolOutput,String>> + Send>> }`; `DagScheduler`
  handles concurrency/retry/`on_state`/output store. Templates are `{{node.<id>.content}}` and
  `{{node.<id>.data.<path>}}`, resolved by the scheduler, scoped to `depends_on`.
- `src/dag_tool.rs::RegistryExecutor` is the template to copy: it wraps tool execution and maps
  `ToolOutput`.

Sketch: add `graph: Option<Vec<SubagentGraphNode>>` to `SubagentRequest` (4th mutually-exclusive
mode), map nodes to `TaskNode { tool_name: "subagent", args: {agent, task, ...}, depends_on }`,
implement `SubagentNodeExecutor` that builds a `SubagentTask` from `resolved_args` and calls
`run_one(...)` (`src/subagents.rs:455`), then run it through `DagScheduler` sharing the request
`deadline`. Per-node fields already exist on `SubagentTask` (cwd/isolation/isoApply/outputSchema/
schemaMode). `isolation` is only accepted on `tasks`/`chain` **entries**, not on the single
`agent`+`task` form — a single-element `tasks`/`graph` node is how you get worktree isolation.

### Note: the `subagent` tool could not be used to do this
A worktree-isolated `subagent` delegation was attempted for the rename and **failed**:
`RECUR_AGENT_SUBAGENT_FAILED: child emitted an error event` — the child reported that every
mutating tool (`edit`/`write`/`hashline_edit`/`ast_edit`/`bash`/`run_code`) was refused with
"approval required ... no approval surface; re-run with --approval-mode yolo". So child agents in
this session are read-only. Do the `graph` work in the parent, or run the child with an approval
mode that permits writes.

---

## 3. Session ledger (all pushed)
- `b2c6a81fd` — pending tool-card spinner is dark yellow (was dark green); `Ok` green / `Err` red.
- `db949f184` — dropped the `N.` ordinal from DAG node labels (box + both legends).
- `6f61f6419` — default system prompt: tool-preference ladder (`dag` > `run_code` >
  `ast_grep`/`ast_edit` > `grep`/`find`/`ls` > `bash`), `dag` trigger rule, and the subagent
  fan-out boundary (scope-splitting guard + worktree isolation for same-file children).
- `533909e82` — this rename.
- DAG "running" colour needed no change: `DagViewState::Running == (229,192,123) == palette.warning`
  exactly.

## 4. Next agent's starting position
1. Run `cargo test --test conformance_fixtures` (whole target) and `dsr quality --tool recur_agent`
   to finish verifying the rename.
2. Then build the `graph` mode per section 2.
3. Never `git add -A`: this checkout always carries several sessions' dirty files.
