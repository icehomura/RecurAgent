# Handoff — tool-card spinner color rule + `dag`/`run_code` prompt guidance

- **Session id:** 5cab5f04-2f05-4f94-8aa7-8940ae286ffc
- **Repo:** C:\Users\icehomura\workspace\rust\pi_agent_rust\RecurAgent
- **Written because:** tool-iteration budget reached the >=80% handoff threshold.

## What is DONE

### 1. Tool-card spinner color rule (verified, in-tree)
Requested rule: while a tool runs the leading spinner is a **darker yellow** (not bright
yellow), success is **green**, failure is **red**.

Change (single line), `src/interactive_ftui.rs` `push_card_block`:

```rust
CardState::Pending => (
    DOTS[spinner_frame % DOTS.len()],
    ftui::Style::new().dim().fg(palette.warning),   // was: palette.success
),
```

Terminal states were already correct and are unchanged:
`CardState::Ok => ("✓", fg(palette.success))`, `CardState::Err => ("✗", bold fg(palette.error))`.

- `palette.warning = rgb(229, 192, 123)` (`src/interactive_ftui.rs:577`), `dim()` keeps the
  "same darkness as the old green", which is the requested look.
- This is the shared pending card head, so it covers ordinary tool cards **and** the `dag`
  card head.
- Verified with `cargo check --lib` → exit 0 (run after the change).
- **Status: staged/committed separately — see "Commit state" below.**

### 2. Investigation result — DAG "running" needs NO change
The DAG per-node "running" color is already the identical dark yellow:

- `src/dag_view.rs:49` → `Self::Running => (229, 192, 123)`
- `src/interactive_ftui.rs:577` → `warning: rgb(229, 192, 123)`
- `Succeeded => (96, 196, 116)` green, `Failed => (220, 80, 80)` red, both already matching
  the rule. The only green a user may see at the top/bottom of a DAG tree is the
  `DagViewCellState::Root` START/END sentinel (`src/interactive_ftui.rs:1506-1508`), which is
  not a running node.

## What REMAINS — the approved 3-place prompt fix (NOT applied)

User approved this; the edit calls were skipped by the runtime. **Prompt text must be English.**

1. `src/app.rs` `default_system_prompt`, `tool_descriptions` array (currently ends with
   `current_time`, ~line 382). Add:
   ```rust
   (
       "dag",
       "Run two or more tool calls as a dependency DAG in parallel inside this session (one call, per-node status, resumable graphId)",
   ),
   (
       "run_code",
       "Execute a JavaScript program against the available tools in one round trip",
   ),
   ```

2. `src/app.rs`, immediately before `if has_tool("subagent") {` (~line 457), add:
   ```rust
   if has_tool("dag") {
       // `dag` is the orchestrator that fans tool calls out INSIDE this session.
       // Its whole value is being the first thing reached for when work
       // parallelizes; left unstated, the model issues the calls one at a time
       // and pays a round trip each. The disambiguation against `subagent` and
       // `run_code` is load-bearing: all three occupy the "parallel work" slot,
       // so naming only two of them leaves `dag` with no trigger condition.
       guidelines_list.push(
           "When a task needs two or more tool calls that are independent of each other and whose results you will read together, prefer ONE `dag` call that fans them out (use `dependsOn` for steps that genuinely need an earlier result, and `resume` + `patch` to repair a partially failed graph) over issuing them one at a time. Division of labor: `dag` runs tool calls concurrently inside this session (write/process nodes are serialized, and the report carries per-node status plus a resumable `graphId`); `subagent` fans out work that deserves its own context window; `run_code` runs a JavaScript program against the tools.".into(),
       );
   }
   ```

3. `src/dag_tool.rs` `description()` (~line 332) — prepend a trigger sentence:
   ```
   "Prefer this over issuing several independent tool calls one at a time when you have two or more calls whose results you will read together. " \
   ```
   (then the existing "Execute a dependency DAG ..." text continues.)

Optional: mirror the same "prefer `dag` for 2+ independent calls" line into `AGENTS.md`.

## Why this is the right fix (diagnosis, for the next agent's context)

"Capability vs habit" gap, all inside this repo:

- `src/app.rs:447` gives `run_code` an imperative guideline; `src/app.rs:457-471` gives
  `subagent` one whose own comment says **"Delegation is an orchestrator, like `dag`"**.
- Nothing pushes a `dag` guideline, and `dag` is absent from `tool_descriptions` (`dag` only
  appears in `src/app.rs` comments). `grep dag src/app.rs` confirms it is never emitted.
- `src/xdev.rs:88-94` states the intent outright: dag's tiering exists because "its whole
  value is being the first thing reached for when work parallelizes", and the neighbouring
  `run_code` comment says "a guideline telling the model to prefer it ... is worthless if the
  tool it names is not in the schema".
- Net effect: `run_code` and `subagent` each hold a "parallel work" trigger; `dag` has none,
  so the model's learned policy skips it.

## Commit state / next agent's starting position

`git status --short` at handoff showed these modified paths, **most of them other sessions'
uncommitted work — do NOT sweep them**:

```
 M README.md
 M benches/search_backends.rs
 M src/dag_tool.rs          # another session's change
 M src/interactive_ftui.rs  # my color line + another session's `ra.dag.node_update.v1` handler (~line 2997)
 M src/subagents.rs
?? handoffs/727b2912-..._ast-grep-vs-grep-bench.md
```

- My color change is a **single line at `src/interactive_ftui.rs:789`**. Because the same file
  also holds another session's `ra.dag.node_update.v1` work, the commit must stage **only**
  that hunk, not the whole file. Mechanism used / to use:
  ```bash
  git diff -- src/interactive_ftui.rs > /tmp/full.patch
  awk 'BEGIN{h=0} /^@@/{h++} {if(h<=1) print}' /tmp/full.patch > /tmp/mine.patch
  git apply --cached --check /tmp/mine.patch && git apply --cached /tmp/mine.patch
  git diff --cached   # must show only the palette.success -> palette.warning line
  ```
- If the agent-mail pre-commit guard refuses because another session holds an exclusive
  reservation on `src/interactive_ftui.rs`: **do not bypass it.** Leave the line uncommitted
  and record that here / in the bead thread.
- After the 3-place prompt fix: re-run the project quality entry point
  (`dsr quality --tool recur_agent`; `dsr` was not on PATH in this session) or at minimum
  `cargo check --lib`. The prompt tests in `src/app.rs` (~3526-3550) only assert the subagent
  roster and do not pin the tools list, so the additions are safe.

## Round 2 (same session) — DAG numbering removal + prompt guidance (APPLIED, UNVERIFIED)

### DAG node numbering removed (`src/dag_view.rs`) — the user's "don't show `N.`" request
The `{id}. ` prefix is gone from all three label sites:
- box label `src/dag_view.rs:904` → `format!("{} {}", node.state.marker(frame), short_name(display, cap))`
  (was `"{} {}. {}"` with `node.id`); boxes now read `[✓] read`, not `[✓] 1. read`.
- vertical legend `:326-330` and horizontal legend `:770-774` → `tool_name` when `name` is empty,
  else `"{name} ({tool_name})"` (was `"{id}. ..."`). The `检索 (search)` full-name legend no longer
  carries an ordinal.
- `node.id` is still read at `src/dag_view.rs:868` (`by_id.insert(node.id, i)`) for dependency
  layout, so nothing went dead.
- Tests updated to match: `src/dag_view.rs` (linear_chain_has_no_jogs, diamond_rejoins_after_both_parents,
  dangling_and_cycle_nodes_do_not_disappear, horizontal_mode_renders_and_auto_prefers_it_when_wide)
  and `src/interactive_ftui.rs` (dag card test: `[ ] 查询`, `查询 (search)`, `[✓] 查询`).

### System-prompt guidance added (`src/app.rs`)
- Tool-preference ladder guideline (gated `has_tool("dag") && has_bash`):
  `dag` > `run_code` > `ast_grep`/`ast_edit` > `grep`/`find`/`ls` > `bash`, with an explicit
  "do not drop to bash for something a higher rung already does".
- `dag` trigger guideline (gated `has_tool("dag")`): prefer ONE `dag` call for 2+ independent calls.
- Subagent fan-out **boundary** added inside the subagent block: single-point changes / pure
  questions are NOT fan-out (scope-splitting); same-file children need `isolation: "worktree"`.
- `tool_descriptions` gained `dag`, `run_code`, `ast_grep`, `ast_edit`.
- New test `default_system_prompt_states_the_tool_preference_ladder_and_dag_trigger`; the existing
  `default_system_prompt_names_every_builtin_subagent_and_the_verify_loop` now also asserts the
  scope-splitting boundary and its absence without `subagent`.

### BLOCKER — cannot compile or run tests on current HEAD (NOT this work)
`src/ptc_bridge.rs` fails to compile with 2× `E0284` (`Func::from` closure inference) at
`:589` and `:639`. The file is **unmodified in the worktree**; the breakage is another session's
committed change `af8eeee03` (`fix(run_code): promise-returning sdk helpers, ...`), whose own
handoff `handoffs/6018de4c-ee6b-4df9-ad72-76ff44e2a2e9_run_code-ptc-fixes.md` states
**"compile/test NOT yet confirmed"**. `59d793d5c feat(dag)` sits on top.

- `cargo check --lib` → `CHECK_EXIT=101`, and the ONLY 2 errors are in `ptc_bridge.rs` (no errors
  reported in `app.rs` / `dag_view.rs` / `interactive_ftui.rs`), which is weak but consistent
  evidence this round's edits are sound.
- `cargo test --lib dag` → could not build the lib-test target for the same reason.
- **Do NOT close this round as verified.** Once `af8eeee03`'s E0284 is fixed, run:
  `cargo test --lib dag` (DAG view tests) and `cargo test --lib default_system_prompt` (prompt tests).

### Commit state for round 2
`git status` shows ` M src/app.rs`, ` M src/dag_view.rs`, ` M src/interactive_ftui.rs` in addition to
other sessions' files. `src/interactive_ftui.rs` still also contains another session's uncommitted
`ra.dag.node_update.v1` handler, so if these are committed, `interactive_ftui.rs` must again be
staged **hunk-by-hunk** (see the mechanism above); `app.rs` and `dag_view.rs` are clean of others' work.

## Verification evidence already obtained

- `cargo check --lib` → `Finished dev profile ... in 53.94s`, `CARGO_EXIT=0` (round 1, before the
  `af8eeee03` regression landed).
- DAG investigation performed with the `dag` tool (4 read-only grep nodes, all succeeded).
- Round 1 color change is committed and pushed: `b2c6a81fd`.

