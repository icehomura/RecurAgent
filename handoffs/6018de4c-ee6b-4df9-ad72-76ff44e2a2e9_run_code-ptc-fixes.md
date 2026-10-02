# Incomplete Handoff — `run_code` (PTC bridge) defect fixes

**Handoff id:** `6018de4c-ee6b-4df9-ad72-76ff44e2a2e9`
**Slug:** `run_code-ptc-fixes`
**Date:** 2026-10-01
**Status:** Code changes APPLIED and COMMITTED; **library type-check PASSES and all 40
`ptc_bridge` unit tests PASS** (2026-10-01). Full DSR gate (all-targets / clippy / fmt /
conformance) still NOT run.
**Commit:** `af8eeee03` — `fix(run_code): promise-returning sdk helpers, zero-arg ls, safe console, deadline, visible truncation`

---

## 1. What was asked

Full test of the `run_code` tool, then a detailed fix (via subagents) of the defects found. The
defect list and the agreed scope are in section 3.

## 2. What is DONE

All edits are confined to **`src/ptc_bridge.rs`** and are committed in `af8eeee03`:

1. **P0-1 — `sdk.*` helpers now return real Promises** (were synchronous `String`).
   - `set_sdk_helper` (`src/ptc_bridge.rs` ~line 622) builds `Promise::new(&ctx)` and settles it
     synchronously with the host reply.
   - `sdk.call` escape hatch converted identically (~line 588).
   - New `settle_bridge_promise` resolves with the host text or rejects with a real JS `Error`
     (via the global `Error` constructor) so `await sdk.x()` throws an object whose `.message` is
     the host error text.
   - Free `bridge_call` changed from `rquickjs::Result<String>` (threw) to
     `std::result::Result<String, String>` (returns); its `Ctx` param is now `_ctx`.
   - Enables `.then`/`.catch`; **note: calls remain serialized (no true concurrency)** — that was
     deliberately out of scope (would need an async host pipeline).
2. **P0-2 — zero-argument `sdk.ls()` works.** `normalize_args` takes `Option<&JsValue>`
   (~line 814); `None` + `optional` → `Ok(json!({}))`, else a clear "got undefined" error.
   The helper closures now take `first: Opt<JsValue>` instead of a required `JsValue`.
3. **P0-3 — `console.log` can never throw.** Console sink is now
   `move |ctx: Ctx<'js>, parts: Rest<JsValue<'js>>|` (~line 513) using new
   `render_console_arg` / `render_console_text` (~line 770). Symbols short-circuit, BigInt/
   circular `json_stringify` failures are cleared with `ctx.catch()` and fall back to text.
4. **P1-1 — CPU-spin timeouts report `PTC_DEADLINE`.** New `error_or_stop` (~line 1036) prefers
   `PTC_CANCELLED`/`PTC_DEADLINE` over a raw `InternalError: interrupted`; both `run_program`
   failure call sites (~lines 926, 981) use it.
5. **P1-2 — truncation is visible.** Success path appends
   `\n\n[truncated: kept first N of M lines, X of Y bytes]` **only when `truncation.truncated`**
   (~line 1329). `details` unchanged.
6. **P1-3 — captured console surfaces on FAILURE.** Error content is prefixed with
   `[console output]…[end console output]` (~line 1311). **Success content is still exactly the
   return value** (an existing test pins this).
7. **P1-4 — network claim corrected.** Module doc (~line 47) and `description()` (~line 1190) now
   say the realm has no network API *of its own*, and that `sdk.read` can fetch `http(s)` URLs.

Seven unit tests were added at the end of the `#[cfg(test)] mod tests` block:
`helpers_return_promises`, `ls_accepts_zero_arguments`, `then_chaining_works`,
`console_never_throws_on_exotic_values`, `cpu_spin_timeout_reports_deadline`,
`truncation_is_visible`, `console_surfaces_on_failure`.

**Not changed (deliberate):** `effects()` still returns `ToolEffects::process()`; an existing test
(`granting_bash_escalates_declared_effects`) pins that, and relaxing it would be a security-posture
change, not a correctness fix.

## 3. What is NOT done / NOT verified

### 3.0 VERIFIED on 2026-10-01 (supersedes the "not verified" note below for the lib lane)

A background `cargo check` finished and isolated two real compile errors in `ptc_bridge.rs`
(`E0284` on the two `Func::from` closures — ambiguous error type now that they return
`Ok(promise)`). Those closures now carry an explicit
`-> rquickjs::Result<Promise<'js>>` return annotation (this fix was present in the working tree
and landed inside commit `e21a6553f`). Then, against a warm private target dir:

```
CARGO_TARGET_DIR=/tmp/codeg-acp/.../ptc-verify-target \
  cargo check --locked --lib --no-default-features --features sqlite-sessions
# => Finished `dev` profile ... check exit: 0

  cargo test  --locked --lib --no-default-features --features sqlite-sessions,tui ptc_bridge
# => test result: ok. 40 passed; 0 failed; 0 ignored; 10139 filtered out; finished in 2.88s
```

All 40 `ptc_bridge` tests pass, including every new one: `helpers_return_promises`,
`ls_accepts_zero_arguments`, `then_chaining_works`, `console_never_throws_on_exotic_values`,
`cpu_spin_timeout_reports_deadline`, `truncation_is_visible`, `console_surfaces_on_failure`,
`read_only_extras_are_reachable`, and the effects-invariant `whitelist_is_read_only`.

Notes on the environment: `--no-default-features` alone produces 4 unrelated
`crate::session_sqlite` unresolved-import errors (the module is gated behind `sqlite-sessions`);
add `--features sqlite-sessions` to clear them. The lib **test** target additionally needs `tui`
(otherwise `src/autocomplete.rs` test code cannot find `crate::interactive`), so tests were run
with `--features sqlite-sessions,tui`. Neither is caused by this change.

### 3.1 Still NOT verified

- **The code has not been type-checked or tested.** `dsr` is not on `PATH` in this environment, so
  `dsr quality --tool recur_agent` could not be run. A private `cargo check` was started in the
  background (see section 5) but had **not finished** when this handoff was written.
  A parse-level check passed: `rustc --edition 2024 -Zunpretty=ast-tree src/ptc_bridge.rs` exits 0.
- **Highest remaining compile risk:** `Promise::new(&ctx)`, `impl IntoJs for Promise`, closure
  return `Result<Promise<'js>, rquickjs::Error>`, and `Func::from` with `Ctx` first + `Rest<JsValue>`
  last. The one earlier concern (`Ctx::json_stringify` 1-arg) is **void**: it is pre-existing
  baseline code in `json_arg` and therefore already compiles.
- **Verifier finding not yet addressed (small hardening):** `error_or_stop` returns
  `deadline_error()`/`cancelled_error()` **without** calling `ctx.catch()` first, so a pending
  QuickJS interrupt exception is left on the context when the realm tears down. Add
  `let _ = ctx.catch();` before each of those two early returns (do **not** call it before
  delegating to `error_payload`, which consumes the exception itself).
- **Deferred intentionally (documented, not bugs):** true call concurrency; `effects()`
  read-only-relaxation; `on_update` progress streaming; pass-through of `read` image blocks
  (currently dropped — `bridge_call` only concatenates `ContentBlock::Text`).
- **`README.md` / `docs/` were not updated.** The auditor confirmed no golden/fixture pins
  `run_code`'s description or content shape, so docs are optional.

## 4. Evidence / artifacts

- Independent static audit (read-only child) returned PASS on all seven requirements against the
  actual code and listed the exact existing tests that remain satisfied
  (`console_output_is_captured`, `unrepresentable_returns_fall_back_to_text`,
  `return_value_is_the_only_output`, `host_tool_errors_reject_the_await`,
  `program_timeout_is_enforced`).
- `rustc -Zunpretty=ast-tree` on the edited file: exit 0, no diagnostics.
- Background compile log: `/tmp/ptc-check.log` (private target dir `/tmp/ptc-verify-target`).

## 5. Next agent's starting position

1. Check the background compile:
   ```bash
   tail -40 /tmp/ptc-check.log
   ```
   If the job died with the session, restart the type-check with a private target dir (do **not**
   use the shared `target/`, it blocks other agents):
   ```bash
   CARGO_TARGET_DIR=/tmp/ptc-verify-target cargo check --locked --lib --no-default-features \
     --message-format short 2>&1 | tail -40
   ```
2. Fix any compile errors, then apply the `ctx.catch()` hardening from section 3.
3. Run the authoritative gate: `dsr quality --tool recur_agent`. Do **not** claim a pass unless it
   actually ran; if it is load-blocked, record the exact hold and leave this handoff open.
4. If green, optionally update `README.md`'s `run_code` row.

## 6. Cautions / caveats

- **Commit `af8eeee03` swept in another agent's staged change.** `src/interactive_ftui.rs`
  (+90 lines) was already in the git index when this commit was made and was included (I staged
  only `src/ptc_bridge.rs`). No work was lost — it is committed, not reverted — but its author
  should know their change is now inside `af8eeee03`. No history rewrite was attempted
  (`git reset`/rebase not run).
- The working tree is shared with many other agents; do not stash/revert/clean anything.
- Rule 1: the private target dir `/tmp/ptc-verify-target` is created outside the repo; remove it
  only with explicit permission.

---

## 7. Follow-up requirement added 2026-10-01 (user)

> "直接给 run_code 授权之后，这些都得能用。如果是 YOLO 的话，甚至都不需要给 run_code 授权。"
> "一次授权就行。" "无需嵌套授权。"

**Requirement:** authorizing `run_code` at the outer gate must make the bridge able to call the
internal tools (`ast_grep`, `ast_edit`, `web_search`, `bash`, `sessions`, grep, …) without any
*nested* / per-inner-tool approval. Under `yolo`, no authorization is needed at all.

### 7.1 Landed in this follow-up (UNVERIFIED — same gate hold as section 3)

All in `src/ptc_bridge.rs`:

- `BRIDGE_WHITELIST` 4 → 7: added **`ast_grep`, `json_query`, `current_time`** (all declare
  `ToolEffects::read()`).
- `GRANTABLE_TOOLS` 3 → 6: added **`ast_edit`** (write), **`sessions`** (write), **`web_search`**
  (network).
- `bridge_tool` builds all six new tools; `install_globals` adds named helpers
  `sdk.astGrep`, `sdk.jsonQuery`, `sdk.currentTime`.
- `effects()` now unions the effects of each granted tool (so a granted `web_search` declares
  `network`), then still unions `write` for any grant (keeps the existing
  `granting_bash_escalates_declared_effects` test green).
- Tests: rewrote `whitelist_is_read_only` into an effects invariant (every whitelist member must
  declare no write/append/process/network) and added `read_only_extras_are_reachable`.
- Module doc + `description()` updated.

### 7.2 NOT landed — the actual "one authorization ⇒ all tools" plumbing

Needed to fully satisfy the request (this is the remaining work):

1. **Shared, session-scoped grant.** Add to `ToolRegistry` (`src/tools.rs`, struct at line 5372,
   `tools: Vec<Arc<dyn Tool>>`) a field such as `ptc_bridge_grant: Arc<AtomicBool>` (or a
   `Arc<RwLock<HashSet<String>>>` for a per-tool set, plus `"all"`). Pass a clone into
   `RunCodeTool` via a new `with_bridge_grant(...)`. `RunCodeTool::allowed()` (line ~281) becomes
   `BRIDGE_WHITELIST.contains(..) || capabilities.contains(..) || grant.load()`.
2. **Agent flips the grant at the outer approval.** In `src/agent.rs`, where a tool call is
   resolved to `AutoApproved` / user-approved (see `ApprovalEvaluation` handling around lines
   5368–5470) and before `execute_tool_owned`, if `tool_call.name == "run_code"` call
   `self.tools.authorize_ptc_bridge()`. The agent field is `tools: SharedToolRegistry`
   (`src/agent.rs:1800`). Under `yolo` this fires automatically; in `always-ask` it fires once the
   user approves the `run_code` call — exactly "one authorization".
3. **Registry construction seam not yet located.** There is no `build_registry` fn; find the real
   constructor of `ToolRegistry` (the `run_code` arm is at `src/tools.rs:5640`) and thread the
   shared grant through it. `PTC_CAPABILITIES` (env) stays as the non-interactive/headless path
   and can keep working unchanged, or be folded into the same grant.

### 7.3 Non-negotiable safety caveat for 7.2

Do **not** implement "run_code executed ⇒ full reach" by simply injecting a granted flag for every
`run_code` execution. `run_code` is in the tool allowlist of the built-in `explore` / `verify` /
`implement` subagents, and children run headless. If mere execution granted full bridge reach, an
`explore` child could reach `write`/`bash` and defeat its read-only contract
(`src/subagents.rs`). The grant must bind to the **operator's authorization decision** (top-level
interactive approval / explicit `--yolo` / `PTC_CAPABILITIES`), not to the fact that some agent
executed `run_code`. Children should inherit only the operator grant, never a parent session's
runtime approval. Verify the child approval mode in `src/subagents.rs` before wiring this.

### 7.4 Verification for section 7

Same as section 3: `dsr` unavailable here, so nothing above is compile- or test-verified. Re-run
`dsr quality --tool recur_agent` (or the private `cargo check` in section 5) after wiring 7.2.
