# M9.33 Visual Parity Harness Contract

Status: accepted for the next parent `ra` tmux harness update.

## Scope

The executable harness lives in the parent `ra` repository because it starts
both `ra serve` and standalone `ra-tui`. This repo owns the UI assertions
that harness must check.

## Required State Matrix

The tmux harness must retain captures for each state:

| State | Trigger | Required Capture Assertions |
|---|---|---|
| `idle` | protocol or mock bootstrap | footer contains `state`, `idle`, model, usage, AppUi version, cwd |
| `running` | submit prompt / active turn | progress card contains spinner + `Thinking`; no trace logs or timestamps |
| `blocked` | approval request | inline approval card visible; composer focused; status/footer contains `blocked` |
| `done` | turn completed | footer state is `done`; session summary/assistant text remains in transcript |
| `error` | protocol/app error | footer state is `error`; error activity is visible |
| `staged` | message sent during active turn | composer shows staged count and `Ctrl+U clear` |
| `diff_context` | diff preview loaded, hunk selected, `c` pressed | composer or pending queue includes selected hunk context |

## Required Artifact Retention

Each parity run must keep:

- raw tmux capture for `ra-tui`
- cleaned/redacted tmux capture for `ra-tui`
- Codex comparison capture when enabled
- server log
- worktree diff
- git status
- cargo/test validation log
- summary metrics showing which state-matrix assertions passed

## Live Watch Contract

When `ra_TMUX_KEEP=1` or `RA_TUI_UX_KEEP_SESSIONS=1` is set, the runner
must print attach commands for both sessions and leave them alive:

```bash
tmux attach -r -t <ra-tui-client-session>
tmux attach -r -t <codex-client-session>
```

Remote hosts should print the SSH-wrapped form:

```bash
ssh -t cloud@<host> 'tmux attach -r -t <session>'
```

## Parent Harness Patch Checklist

Patch `ra/scripts/compare-tui-coding-ux-tmux.sh` and
`ra/e2e/tmux/run.sh` to:

- assert the state matrix above in mock and live lanes
- save all captures under `e2e/test-results-tui-coding-ux/<run-id>/`
- fail if captures contain noisy trace logs such as `INFO calling LLM`,
  `parallel_tools`, `result_sizes`, `tool_ids=`, or timestamp-prefixed log lines
- expose a non-strict mode for exploratory visual runs, but keep strict mode as
  the default release gate
- include the selected diff hunk staging path (`[`/`]`, then `c`) in the mock
  lane

This is a harness contract. It does not change AppUi/UI Protocol.
