# Incomplete Handoff — `run_code` session grant + `sdk.tools()` introspection

**Handoff id:** `6018de4c-ee6b-4df9-ad72-76ff44e2a2e9`
**Slug:** `run_code-grant-and-sdk-tools`
**Date:** 2026-10-01
**Status:** APPLIED + COMMITTED + **compile-verified**; **items 1–3 done** (`sdk.tools()` tests +
docs, xdev/dag paths covered, subagent guard added). Only the full DSR gate remains.
**Commit:** `671e98649` — `feat(run_code): cover xdev+dag bridge paths, gate grant off subagent children`
**Predecessors:** `af8eeee03` (fixes), `e21a6553f` (read-only extras), `e0a56d5c3` (grant + sdk.tools).

---

## 1. What this commit does

Implements the user's requirement: **authorize the outer `run_code` call once (or run under
`yolo`) and the whole bridge becomes reachable — no nested per-tool approval.**

- `src/ptc_bridge.rs`
  - New `pub type BridgeGrant = Arc<AtomicBool>` and `pub fn new_bridge_grant()`.
  - `RunCodeTool` gains `bridge_grant: BridgeGrant` + `with_bridge_grant(...)`
    (default is an unauthorized fresh grant, so every existing test still sees read-only).
  - `allowed()` = `BRIDGE_WHITELIST || capabilities || bridge_grant.load()`.
  - New `reachable_tools()` → read-only set + static grant + (when the grant is set) every
    `GRANTABLE_TOOLS` entry.
  - `JsRealm::spawn` / `realm_thread` / `install_globals` now thread the reachable list; the realm
    exposes **`sdk.tools()`**, returning a JSON array (as a Promise) of what this run can reach.
- `src/tools.rs`
  - `ToolRegistry` gains `ptc_bridge_grant: BridgeGrant`, created in `with_mutation_recorder`,
    stored in the struct literal, carried through `clone_shallow` and `from_tools`, passed to
    `RunCodeTool::new(...).with_bridge_grant(...)`.
  - `ToolRegistry::authorize_ptc_bridge()` / `ptc_bridge_authorized()`, and a forwarding
    `SharedToolRegistry::authorize_ptc_bridge()`.
- `src/agent.rs`
  - `execute_tool_without_hooks` calls `self.tools.authorize_ptc_bridge()` when
    `tool_call.name == "run_code"`, **before** `tool.execute(...)`. Reaching that point means the
    outer call already passed the approval pipeline (human approval, `write` mode, or `yolo`), so
    this is exactly "one authorization".

Resulting reach when the grant is set: `read, grep, find, ls, ast_grep, json_query, current_time,
bash, write, edit, ast_edit, sessions, web_search` (all constructible bridge tools).

## 2. Verification state (important)

- `cargo check --locked --lib --no-default-features --features sqlite-sessions` → **exit 0**
  (run after the full change, including `sdk.tools()`).
- `cargo test --locked --lib --no-default-features --features sqlite-sessions,tui --
  ptc_bridge tool_registry` → **43 passed / 0 failed**, including the new
  `operator_grant_widens_the_bridge`, and the pre-existing `whitelist_is_read_only`,
  `read_only_extras_are_reachable`, plus `tool_registry_builds_every_listed_name`.
- **Caveat:** that 43-test run happened *before* the `sdk.tools()` plumbing landed; the
  `sdk.tools()` code is only type-checked. Add/extend a test (see section 3).
- Full gate NOT run: `dsr` is not on `PATH` here. Clippy `-D warnings`, `cargo fmt --check`,
  `--all-targets`, and conformance are unverified for this change.
- Warm private target dir (reuse it to avoid a cold rebuild):
  `CARGO_TARGET_DIR=/tmp/codeg-acp/51348-c9e530b2/ptc-verify-target`.

## 3. Remaining work / next agent's starting position

**Items 1–3 are DONE in `671e98649`; only the gate (item 4) remains.**

1. ~~Test `sdk.tools()`.~~ **DONE** — `sdk_tools_lists_reachable_set` and
   `sdk_tools_reflects_static_and_runtime_grants` (default excludes `bash`, includes `ast_grep`;
   static `with_capabilities(["bash"])` and a set runtime grant include `bash`/`ast_edit`/
   `sessions`/`web_search`).
2. ~~Document `sdk.tools()`.~~ **DONE** — the `description()` clause "`await sdk.tools()` lists
   the tools this session can currently reach." is applied.
3. ~~Edge paths.~~ **DONE** — the grant is now flipped on the `xdev run` inner dispatch
   (`agent.rs::dispatch_xdev`) and the dag node executor (`dag_tool.rs::RegistryExecutor::execute`),
   in addition to `execute_tool_without_hooks`.
4. **Run the authoritative gate:** `dsr quality --tool recur_agent` (clippy/fmt/all-targets/
   conformance). Not available in the authoring session (no `dsr` on PATH). Do not claim a pass
   unless it actually runs.

Also still open (whoever picks this up):

5. **Behavioral E2E** (unit tests cannot cover): interactive session where the user approves
   `run_code` and then `sdk.call('bash', ...)` succeeds; and a `yolo` session where it succeeds
   with no prompt. Add one integration test if the harness allows.

## 3b. Verification as of `671e98649`

- `cargo check --locked --lib --no-default-features --features sqlite-sessions` → exit 0.
- `cargo test --locked --lib --no-default-features --features sqlite-sessions,tui --
  ptc_bridge dag tool_registry` → **103 passed / 0 failed** (`ptc_bridge` incl. the two new
  `sdk_tools_*` tests, `task_dag`, and `tool_registry_builds_every_listed_name`).
- Warm private target: `CARGO_TARGET_DIR=/tmp/codeg-acp/51348-c9e530b2/ptc-verify-target`.
- Full gate NOT run (see item 4).

## 4. Accepted safety caveats (document them where users will see them)

- Children (`src/subagents.rs::child_args`) are spawned with `--print --no-session` and **no
  approval flag** → default `always-ask` → `run_code` (process effects) is denied in children, so
  the grant never flips there. Safe by default.
- **Closed in `671e98649`:** global `settings.json` `approval.mode: yolo` applies to children too,
  so a child (e.g. the read-only `explore` agent, which lists `run_code`) would otherwise
  auto-approve `run_code` and reach `write`/`sessions`/`web_search` through the bridge. The grant
  is now gated by `ptc_bridge::bridge_runtime_grant_allowed()`, which returns `false` when
  `RECUR_AGENT_SUBAGENT_DEPTH` is a positive depth (set on every child by
  `subagents::execution`). Children therefore inherit **only** the static `PTC_CAPABILITIES`
  grant, never the parent session's run-time approval.
- The grant is a property of the `ToolRegistry`/session; the program can never set it. Static
  `PTC_CAPABILITIES` grants (children inherit via env) still work unchanged.

---

## 8. Scope 2 (2026-10-01): ALL internal tools via the live registry — commit `54bd1a758`

Per the user's follow-up ("口径2，一次性完成" + confirmed implicit delegation), the bridge no longer
rebuilds a fixed subset. It dispatches through the session's **live `ToolRegistry`**:

- New `Tool::bind_shared_registry(&self, &Weak<SharedToolRegistryInner>)` default hook; implemented
  by `RunCodeTool` (stores the `Weak` in a `OnceLock`) to avoid a registry → tool → registry `Arc`
  cycle.
- `SharedToolRegistry::new` binds the handle into every registered tool.
- `RunCodeTool::bridge_call` prefers `shared.snapshot().get(name)` (so `lsp`, `debug`, `eval`,
  `sessions`, memory, `jobs`, `hub`, `browser`, `github`, `hashline_edit`, … are reachable) and
  falls back to `bridge_tool(name)` only for a bare `RunCodeTool` (unit tests).
- `capabilities: Vec<&'static str>` → `Vec<String>`; `with_capabilities` keeps any non-empty name;
  `PTC_CAPABILITIES` accepts any tool name (no longer filtered to a 6-name list).
- `sdk.tools()` now lists every registry tool once the grant is set (fallback list when unbound).
- Recursion guard: `sdk.call('run_code', …)` is refused. Denial message updated.
- Test `live_registry_reaches_tools_the_bridge_cannot_rebuild` proves `hashline_edit` (not
  rebuildable by the bridge) is reached through the live registry, not denied.

### Verification of `54bd1a758`

- `cargo check --locked --lib --no-default-features --features sqlite-sessions` → **exit 0**.
- Combined `cargo test … -- ptc_bridge dag tool_registry` → **104 passed / 1 failed**; the single
  failure is `subagents::tests::dag_is_a_mutually_exclusive_mode`, caused by another agent's
  in-flight `tasks`→`parallel` rename (commit `533909e82`), **not** this change.
- A later isolated `ptc_bridge::` rerun failed to compile with `rustc-LLVM ERROR: out of memory`
  during test-binary codegen — an environment/contention issue (disk 98% full), not a code error.
- Full `dsr` gate still not run (no `dsr` on PATH).

### Remaining after scope 2

1. Re-run `ptc_bridge` tests cleanly once the machine has memory headroom, and run
   `dsr quality --tool recur_agent`.
2. Behavioral E2E: approving `run_code` then `sdk.call('bash'|'lsp'|'sessions', …)` succeeding;
   yolo succeeding with no prompt.
3. Optional: expose the grant via a real config/CLI setting instead of `PTC_CAPABILITIES`.

---

## 9. C-items (2026-10-01) — commit `62114e211`

1. **Config setting.** `Config` gains `ptc: Option<PtcSettings>` (`PtcSettings { capabilities:
   Option<Vec<String>> }`), merged by `merge_ptc`. `tools.rs` passes `config.ptc.capabilities`
   into `RunCodeTool::with_capability_names` (new owned-name entry point), overriding the
   `PTC_CAPABILITIES` env fallback.
2. **Media markers.** `dispatch_bridge_tool` no longer drops `Image`/`Media` blocks silently: it
   appends `[image omitted: <mime>, <bytes>]` / `[media omitted: <name>, <mime>, <bytes>]` (byte
   count approximated from base64 length). True base64/vision pass-through remains out of scope —
   the realm channel is text-only. Test: `image_blocks_are_marked_not_dropped` (fake `image_probe`
   tool through the live registry).
3. **Effects union under grant.** `RunCodeTool::effects()` now unions every live-registry tool's
   effects once `bridge_grant` is set, so a granted run declares its real network/UI/process reach.
4. **README** `run_code` row updated.

Verification: `cargo check --lib` exit 0; `ptc_bridge` **45/45** tests pass.

**Caveat:** the `README.md` half of this commit **swept in another agent's uncommitted README
edits** (the `explore`/`implement` built-in-agent descriptions around lines 583–600). Same pattern
as the earlier `interactive_ftui.rs` sweep in `af8eeee03`; no work lost, no history rewrite
attempted. Whoever owns those edits should know they now live in `62114e211`.

### Still open after C

- `dsr quality --tool recur_agent` (never run; no `dsr` on PATH).
- Behavioral E2E (approve → `sdk.call('bash'|'lsp'|'sessions')`; yolo no prompt).
- Unrelated red: `subagents::tests::dag_is_a_mutually_exclusive_mode` (another agent's
  `tasks`→`parallel` rename).

## 5. Prior context

The earlier defect fixes and the read-only/grantable expansion are in
`handoffs/6018de4c-ee6b-4df9-ad72-76ff44e2a2e9_run_code-ptc-fixes.md` (sections 1–7), including the
full test matrix and the P0/P1 defect list.
