# Handoff — DAG view: end-node color + no-ordinal guard

**Session:** 6d8cc877-e457-4714-b37c-f745948d06c3
**Branch:** `main` · local commit `02bbd62f4` (NOT pushed — see Hold)
**Work tree:** `C:\Users\icehomura\workspace\rust\pi_agent_rust\RecurAgent`

## The user's two reports

1. "Why do DAG-mode nodes still show an ordinal? I said not to — at least 3
   chars (`N. `). Whether it's a tool call or a subagent call, no ordinal
   inside the box."
2. "The end node wasn't reached — don't make 结束 green; keep the original
   gray."

## What is DONE

### A. End node stops claiming success early (the actionable fix)
`src/dag_view.rs` `prepare()` (lines ~916-934): the virtual sink `结束` used to
be pushed with `DagViewCellState::Root` unconditionally (green). It is now:

```rust
let reached_end =
    !src.is_empty() && src.iter().all(|node| node.state == DagViewState::Succeeded);
...
state: if reached_end {
    DagViewCellState::Root          // green
} else {
    DagViewCellState::Neutral       // same dim gray as the connectors
},
```

So: running/pending → gray; any `Failed`/`Skipped` → gray; **all** real nodes
`Succeeded` → green. No new enum variant was needed; `Neutral`/`Root` already
exist and `dag_view_styled` in `src/interactive_ftui.rs:1493-1518` needs no
change.

Two regression tests added in `src/dag_view.rs`:
- `end_node_greens_only_after_full_success` — asserts gray for Running/Failed/
  Skipped and green for all-Succeeded.
- `node_labels_have_no_ordinal_prefix` — asserts `1. `/`2. ` never appear in
  the rendered DAG and that `[✓] read` / `[ ] write` do.

### B. The ordinal question
The `N. ` prefix was **already removed** from `src/dag_view.rs` in commit
`db949f184` (2026-10-01 22:10): it covered the box label
(`format!("{} {}", marker, short_name(display, cap))`) and both orientation
legends (`{name} ({tool})`). `git log -S'{}. {}' -- src/dag_view.rs` shows only
the add (`397f7389c`) and the removal (`db949f184`). The current
`target/release/ra.exe` (mtime 2026-10-02 05:06, after `db949f184`) contains no
`{} {}. {}` format string. **Conclusion: if the user still sees boxes with
`1. read`, they are running a binary built before `db949f184`.** Tell them to
rebuild (`just build-tui`, i.e. `cargo build --release --locked --bin ra`).

## Evidence obtained

- `cargo check --lib --locked` → `CHECK_EXIT=0` (17.3s).
- `cargo test --lib --locked dag_view` → **14/14 pass**, including the two new
  tests (`TEST_EXIT=0`). Run before the concurrent `ptc_bridge.rs` rewrite
  landed in the work tree.
- `rustfmt --edition 2024 --check src/dag_view.rs` → `FMT_EXIT=0`.
- `dsr` is **not installed** on this machine; `cargo` was used as the only
  available diagnostic, not as a substitute quality authority.

## What REMAINS / open questions for the user

1. **Ambiguity worth confirming:** the subagent `dag`-mode *result report*
   (not the box tree) uses `## step {node_id}: {agent}` headings
   (`src/subagents.rs` `render_results`, ~line 1554-1580). That is a numeric
   node id inside the subagent tool card. The user's stated format was
   `digit + dot + space`, which matches the removed box label, **not** `step N:`.
   Do not strip it without confirmation: it is the only thing distinguishing
   same-agent nodes. If the user confirms it, the fix needs the node `name`
   plumbed into `SubagentResult` (it currently carries only `step: Option<usize>`).
2. **Confirm the conditional choice.** The end node is green on full success.
   The user's "还按原来的那种灰色" could alternatively mean *always* gray; that is
   a one-line change (`state: DagViewCellState::Neutral` unconditionally) in the
   same spot.
3. **Re-run the interactive_ftui DAG-card tests** once another session's
   in-flight `ptc_bridge.rs` rewrite compiles. The only assertions touching the
   end node are `dag_details_prefix_gates_the_tree_view`
   (`src/interactive_ftui.rs:12943-13040`, checks `detail.contains("结束")`) and
   `dag_tool_end_replaces_tree_with_aggregate_report` (line 13097) — neither
   asserts a color, so they should stay green.

## HOLD — commit is local, push is network-blocked

- `git commit` landed `02bbd62f4` (`1 file changed, 65 insertions(+), 1 deletion(-)`),
  staging **only** `src/dag_view.rs`.
- `git push origin main` failed twice: first `RPC failed; curl 28 Recv failure:
  Connection was reset`, then `Failed to connect to github.com port 443 after
  21083 ms`. `git status -sb` → `## main...origin/main [ahead 2]` (my commit plus
  one earlier unpushed local commit).
- **Do not** `git pull --rebase`: the work tree carries many other agents'
  unstaged edits (`src/agent.rs`, `src/config.rs`, `src/dag_tool.rs`,
  `src/error.rs`, `src/ptc_bridge.rs`, `src/subagents.rs`, `src/tools.rs`) and a
  rebase would refuse / disturb them. Retry `git push origin main` when the
  network is back; nothing else needs doing for this slice.

## Next agent's starting position

`src/dag_view.rs`, `prepare()`, the `reached_end` computation and the `结束`
`LNode` push (~lines 916-934). Tests live at the bottom of the same file
(~lines 1137-1210). Nothing was written outside `src/dag_view.rs`.
