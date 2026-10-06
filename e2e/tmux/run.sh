#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

# shellcheck source=../../scripts/tmux-cli-driver.sh
if [ -f "$ROOT_DIR/scripts/tmux-cli-driver.sh" ]; then
  source "$ROOT_DIR/scripts/tmux-cli-driver.sh"
else
  RA_TMUX_ROOT="${RA_TMUX_ROOT:-$ROOT_DIR}"
  RA_TMUX_COLS="${RA_TMUX_COLS:-140}"
  RA_TMUX_ROWS="${RA_TMUX_ROWS:-40}"
  RA_TMUX_PREFIX="${RA_TMUX_PREFIX:-ra-tmux-}"
  RA_TMUX_RUN_ID="${RA_TMUX_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)-$$}"
  RA_TMUX_RUN_PREFIX="${RA_TMUX_PREFIX}${RA_TMUX_RUN_ID}-"
  RA_TMUX_ARTIFACT_ROOT="${RA_TMUX_ARTIFACT_ROOT:-${RA_TMUX_ROOT}/e2e/test-results-tmux}"
  RA_TMUX_ARTIFACT_DIR="${RA_TMUX_ARTIFACT_DIR:-${RA_TMUX_ARTIFACT_ROOT}/${RA_TMUX_RUN_ID}}"
  RA_TMUX_KEEP="${RA_TMUX_KEEP:-0}"

  declare -a RA_TMUX_SESSIONS=()
  declare -i RA_TMUX_CAPTURE_SEQ=0

  tmux_log() {
    printf '[tmux] %s\n' "$*"
  }

  tmux_skip() {
    printf 'SKIP: %s\n' "$*"
    exit 0
  }

  tmux_require() {
    if ! command -v tmux >/dev/null 2>&1; then
      tmux_skip "tmux is not installed; skipping tmux CLI/TUI harness"
    fi
  }

  tmux_init_artifacts() {
    mkdir -p "$RA_TMUX_ARTIFACT_DIR"
  }

  tmux_session_name() {
    local slug="${1:-session}"
    slug="${slug//[^a-zA-Z0-9_-]/-}"
    printf '%s%s' "$RA_TMUX_RUN_PREFIX" "$slug"
  }

  tmux_quote_command() {
    local quoted=""
    local arg
    local part

    if [ "$#" -eq 0 ]; then
      printf ':'
      return 0
    fi

    for arg in "$@"; do
      printf -v part '%q' "$arg"
      if [ -z "$quoted" ]; then
        quoted="$part"
      else
        quoted="$quoted $part"
      fi
    done
    printf '%s' "$quoted"
  }

  tmux_register_session() {
    local session="$1"
    RA_TMUX_SESSIONS+=("$session")
  }

  tmux_unregister_session() {
    local session="$1"
    local next=()
    local item

    for item in "${RA_TMUX_SESSIONS[@]}"; do
      if [ "$item" != "$session" ]; then
        next+=("$item")
      fi
    done
    if [ "${#next[@]}" -eq 0 ]; then
      RA_TMUX_SESSIONS=()
    else
      RA_TMUX_SESSIONS=("${next[@]}")
    fi
  }

  tmux_new() {
    local session="$1"
    local cols="$2"
    local rows="$3"
    shift 3

    if tmux has-session -t "$session" 2>/dev/null; then
      tmux kill-session -t "$session" 2>/dev/null || true
    fi

    local command
    command="$(tmux_quote_command "$@")"
    tmux_register_session "$session"
    tmux new-session -d -s "$session" -x "$cols" -y "$rows" "$command"
  }

  tmux_new_default() {
    local session="$1"
    shift
    tmux_new "$session" "$RA_TMUX_COLS" "$RA_TMUX_ROWS" "$@"
  }

  tmux_send() {
    local session="$1"
    local text="$2"
    tmux send-keys -t "$session" -l "$text"
  }

  tmux_key() {
    local session="$1"
    local key="$2"
    tmux send-keys -t "$session" "$key"
  }

  tmux_redact() {
    perl -0pe '
      BEGIN {
        @tokens = grep { defined && length } ($ENV{RA_AUTH_TOKEN}, $ENV{RA_TMUX_AUTH_TOKEN});
      }
      for my $token (@tokens) {
        s/\Q$token\E/[REDACTED]/g;
      }
      s/(Authorization:\s*Bearer\s+)[^\s]+/${1}[REDACTED]/gi;
      s/(--auth-token(?:=|\s+))[^\s]+/${1}[REDACTED]/g;
      s/(RA_AUTH_TOKEN=)[^\s]+/${1}[REDACTED]/g;
    '
  }

  tmux_strip_ansi() {
    perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g; s/\e\][^\a]*(?:\a|\e\\)//g; s/\e[@-Z\\-_]//g'
  }

  tmux_artifact_path() {
    local session="$1"
    local label="$2"
    local suffix="$3"
    local safe_session="${session//[^a-zA-Z0-9_.-]/-}"
    local safe_label="${label//[^a-zA-Z0-9_.-]/-}"
    printf '%s/%03d-%s-%s.%s.log' \
      "$RA_TMUX_ARTIFACT_DIR" \
      "$RA_TMUX_CAPTURE_SEQ" \
      "$safe_session" \
      "$safe_label" \
      "$suffix"
  }

  tmux_capture() {
    local session="$1"
    local label="${2:-capture}"

    tmux_init_artifacts
    RA_TMUX_CAPTURE_SEQ=$((RA_TMUX_CAPTURE_SEQ + 1))

    local raw_path
    raw_path="$(tmux_artifact_path "$session" "$label" "raw")"

    if tmux has-session -t "$session" 2>/dev/null; then
      tmux capture-pane -t "$session" -p -e -J -S - | tmux_redact >"$raw_path"
    else
      printf 'tmux session not found: %s\n' "$session" | tmux_redact >"$raw_path"
      cat "$raw_path"
      return 1
    fi

    cat "$raw_path"
  }

  tmux_capture_clean() {
    local session="$1"
    local label="${2:-capture}"

    tmux_init_artifacts
    RA_TMUX_CAPTURE_SEQ=$((RA_TMUX_CAPTURE_SEQ + 1))

    local raw_path
    local clean_path
    raw_path="$(tmux_artifact_path "$session" "$label" "raw")"
    clean_path="$(tmux_artifact_path "$session" "$label" "clean")"

    if tmux has-session -t "$session" 2>/dev/null; then
      tmux capture-pane -t "$session" -p -e -J -S - | tmux_redact >"$raw_path"
      tmux_strip_ansi <"$raw_path" >"$clean_path"
    else
      printf 'tmux session not found: %s\n' "$session" | tmux_redact >"$raw_path"
      tmux_strip_ansi <"$raw_path" >"$clean_path"
      cat "$clean_path"
      return 1
    fi

    cat "$clean_path"
  }

  tmux_print_failure_capture() {
    local session="$1"
    local message="$2"
    printf '\nFAIL: %s\n' "$message" >&2
    printf -- '--- last clean tmux capture: %s ---\n' "$session" >&2
    tmux_capture_clean "$session" "failure" >&2 || true
    printf -- '--- artifacts: %s ---\n' "$RA_TMUX_ARTIFACT_DIR" >&2
  }

  tmux_fail() {
    local session="$1"
    local message="$2"
    tmux_print_failure_capture "$session" "$message"
    exit 1
  }

  tmux_wait_for() {
    local session="$1"
    local regex="$2"
    local timeout="${3:-10}"
    local deadline=$((SECONDS + timeout))
    local capture

    while [ "$SECONDS" -le "$deadline" ]; do
      capture="$(tmux_capture_clean "$session" "wait" || true)"
      if printf '%s\n' "$capture" | grep -E -q -- "$regex"; then
        return 0
      fi
      sleep 0.2
    done

    tmux_fail "$session" "timed out waiting ${timeout}s for regex: ${regex}"
  }

  tmux_assert_capture() {
    local session="$1"
    local regex="$2"
    local capture
    capture="$(tmux_capture_clean "$session" "assert" || true)"
    if ! printf '%s\n' "$capture" | grep -E -q -- "$regex"; then
      tmux_fail "$session" "expected capture to match regex: ${regex}"
    fi
  }

  tmux_assert_not_capture() {
    local session="$1"
    local regex="$2"
    local capture
    capture="$(tmux_capture_clean "$session" "assert-not" || true)"
    if printf '%s\n' "$capture" | grep -E -q -- "$regex"; then
      tmux_fail "$session" "expected capture not to match regex: ${regex}"
    fi
  }

  tmux_kill() {
    local session="$1"
    if tmux has-session -t "$session" 2>/dev/null; then
      tmux kill-session -t "$session" 2>/dev/null || true
    fi
    tmux_unregister_session "$session"
  }

  tmux_cleanup() {
    local session
    if [ "${RA_TMUX_KEEP:-0}" = "1" ]; then
      set +u
      if [ "${#RA_TMUX_SESSIONS[@]}" -gt 0 ]; then
        tmux_log "RA_TMUX_KEEP=1; keeping sessions: ${RA_TMUX_SESSIONS[*]}"
      fi
      set -u
      return 0
    fi

    set +u
    for session in "${RA_TMUX_SESSIONS[@]}"; do
      tmux kill-session -t "$session" 2>/dev/null || true
    done
    set -u
  }

  tmux_install_cleanup_trap() {
    trap tmux_cleanup EXIT INT TERM
  }

  tmux_run_line_command() {
    local session="$1"
    shift

    local command
    command="$(tmux_quote_command "$@")"
    local wrapped
    printf -v wrapped '%s\nrc=$?\nprintf "\\n__ra_TMUX_EXIT:%%s__\\n" "$rc"\nwhile :; do sleep 3600; done\n' "$command"
    tmux_new_default "$session" bash -lc "$wrapped"
  }

  tmux_wait_for_exit() {
    local session="$1"
    local timeout="${2:-20}"
    tmux_wait_for "$session" '__ra_TMUX_EXIT:[0-9]+__' "$timeout"
  }

  tmux_assert_exit_status() {
    local session="$1"
    local expected="$2"
    local capture
    capture="$(tmux_capture_clean "$session" "exit-status" || true)"
    if ! printf '%s\n' "$capture" | grep -E -q -- "__ra_TMUX_EXIT:${expected}__"; then
      tmux_fail "$session" "expected exit status ${expected}"
    fi
  }

  tmux_assert_no_registered_sessions() {
    local live=()
    local session
    set +u
    for session in "${RA_TMUX_SESSIONS[@]}"; do
      if tmux has-session -t "$session" 2>/dev/null; then
        live+=("$session")
      fi
    done
    set -u
    if [ "${#live[@]}" -gt 0 ]; then
      printf 'ASSERTION FAILED: registered tmux sessions remain:\n' >&2
      printf '  %s\n' "${live[@]}" >&2
      return 1
    fi
  }

  tmux_assert_no_orphan_sessions() {
    local live=()
    local session
    while IFS= read -r session; do
      if [[ "$session" == "$RA_TMUX_RUN_PREFIX"* ]]; then
        live+=("$session")
      fi
    done < <(tmux list-sessions -F '#S' 2>/dev/null || true)

    if [ "${#live[@]}" -gt 0 ]; then
      printf 'ASSERTION FAILED: tmux sessions remain for this run:\n' >&2
      printf '  %s\n' "${live[@]}" >&2
      return 1
    fi
  }
fi

LANE="${1:-default}"
RA_TMUX_AUTH_TOKEN="${RA_TMUX_AUTH_TOKEN:-ra-tmux-secret-token-2026}"

shell_quote() {
  printf '%q' "$1"
}

if [ -n "${RA_BIN:-}" ]; then
  RA_BIN_CMD="$RA_BIN"
elif [ -x "$ROOT_DIR/target/debug/ra" ]; then
  RA_BIN_CMD="$ROOT_DIR/target/debug/ra"
elif [ -f "$ROOT_DIR/Cargo.toml" ]; then
  RA_BIN_CMD="cargo run --manifest-path $(printf '%q' "$ROOT_DIR/Cargo.toml") -p ra-cli --features api --bin ra --"
else
  echo "Unable to locate ra CLI. Set RA_BIN." >&2
  exit 2
fi

RA_TUI_BIN_CMD=""

resolve_ra_tui_bin_cmd() {
  if [ -n "$RA_TUI_BIN_CMD" ]; then
    return 0
  fi

  if [ -n "${RA_TUI_BIN:-}" ]; then
    RA_TUI_BIN_CMD="$RA_TUI_BIN"
  elif [ -x "$ROOT_DIR/target/debug/ra" ]; then
    RA_TUI_BIN_CMD="$ROOT_DIR/target/debug/ra"
  elif [ -f "$ROOT_DIR/Cargo.toml" ]; then
    RA_TUI_BIN_CMD="cargo run --manifest-path $(printf '%q' "$ROOT_DIR/Cargo.toml") -p ra-cli --bin ra --"
  else
    echo "Unable to locate the merged ra TUI binary. Set RA_TUI_BIN." >&2
    exit 2
  fi
}

tmux_install_cleanup_trap

run_line_capture() {
  local name="$1"
  local command="$2"
  local session
  shift 2
  session="$(tmux_session_name "$name")"
  tmux_run_line_command "$session" bash -lc "$command"
  tmux_wait_for_exit "$session" 45
  local expect
  for expect in "$@"; do
    tmux_assert_capture "$session" "$expect"
  done
  tmux_assert_exit_status "$session" 0
  tmux_capture_clean "$session" "$name" >/dev/null
  tmux_kill "$session"
}

run_line_expect_failure() {
  local name="$1"
  local command="$2"
  local session
  shift 2
  session="$(tmux_session_name "$name")"
  tmux_run_line_command "$session" bash -lc "$command"
  tmux_wait_for_exit "$session" 45
  local expect
  for expect in "$@"; do
    tmux_assert_capture "$session" "$expect"
  done
  tmux_assert_not_capture "$session" "__ra_TMUX_EXIT:0__"
  tmux_capture_clean "$session" "$name" >/dev/null
  tmux_kill "$session"
}

run_tui_mock() {
  local name="mock-tui"
  local session
  session="$(tmux_session_name "$name")"
  tmux_new_default "$session" bash -lc "$RA_TUI_BIN_CMD --mode mock"
  tmux_wait_for "$session" "Opened coding:local:prototype#m9|Ask ra to change code" 45
  tmux_assert_capture "$session" "Opened coding:local:prototype#m9|Ask ra to change code"
  tmux_assert_capture "$session" "Composer"
  tmux_assert_capture "$session" "Tab inspector"
  tmux_assert_capture "$session" "model coding"
  tmux_assert_capture "$session" "usage"
  tmux_assert_capture "$session" "approval gated"
  tmux_assert_capture "$session" "Ask ra to change code"
  tmux_assert_not_capture "$session" "▌"
  tmux_assert_not_capture "$session" "Current Tasks|tasks/status"
  tmux_send "$session" "complete m9 contract"
  tmux_key "$session" Enter
  tmux_wait_for "$session" "Approval Requested" 20
  tmux_assert_capture "$session" "complete m9 cont"
  tmux_assert_capture "$session" "command cargo test -p ra-core ui_protocol"
  tmux_assert_capture "$session" "cwd .*ra"
  tmux_assert_not_capture "$session" "INFO calling LLM|parallel_tools|result_sizes|tool_ids="
  tmux_capture_clean "$session" "$name" >/dev/null
  tmux_key "$session" C-q
  sleep 0.5
  tmux_kill "$session"
}

run_tui_mock_approval_kind() {
  local kind="$1"
  local expect="$2"
  local name="mock-approval-${kind//_/-}"
  local session
  session="$(tmux_session_name "$name")"
  tmux_new_default "$session" bash -lc \
    "RA_TUI_MOCK_APPROVAL_KIND=$(shell_quote "$kind") $RA_TUI_BIN_CMD --mode mock"
  tmux_wait_for "$session" "Opened coding:local:prototype#m9|Ask ra to change code" 45
  tmux_send "$session" "approval ${kind}"
  tmux_key "$session" Enter
  tmux_wait_for "$session" "Approval Requested|\\[approval\\]|kind ${kind}|Mock .*approval|Diff Preview" 20
  tmux_assert_capture "$session" "$expect"
  tmux_assert_capture "$session" "kind ${kind}|\\[approval\\].*${kind}|${kind}"
  if [ "$kind" = "diff" ]; then
    tmux_wait_for "$session" "Mock approval diff|src/coding_loop.rs" 20
    tmux_assert_not_capture "$session" "press[[:space:]]+d|diff[[:space:]]+overlay|diff[[:space:]]+modal"
  else
    tmux_key "$session" n
    tmux_wait_for "$session" "Mock approval response recorded: Deny|Approval denied" 20
  fi
  tmux_assert_not_capture "$session" "INFO calling LLM|parallel_tools|result_sizes|tool_ids="
  tmux_capture_clean "$session" "$name" >/dev/null
  tmux_key "$session" C-q
  sleep 0.5
  tmux_kill "$session"
}

run_tui_protocol_readonly() {
  local name="protocol-readonly"
  local session
  local capture
  session="$(tmux_session_name "$name")"
  tmux_new_default "$session" bash -lc \
    "$RA_TUI_BIN_CMD --mode protocol --endpoint ws://127.0.0.1:9/api/ui-protocol/ws --session 'coding:local:prototype#m9' --profile-id coding --auth-token '$RA_TMUX_AUTH_TOKEN' --readonly"
  tmux_wait_for "$session" "read-only|no network connection opened|Protocol backend read-only" 45
  tmux_assert_capture "$session" "Protocol backend read-only|no network connection ope"
  tmux_assert_capture "$session" "read-only"
  tmux_assert_capture "$session" "Composer"
  tmux_assert_capture "$session" "Tab inspector"
  tmux_assert_capture "$session" "sends disabled"
  tmux_assert_capture "$session" "coding:local:prototype#m9|read-only"
  tmux_assert_not_capture "$session" "Current Tasks|tasks/status"
  tmux_assert_not_capture "$session" "INFO calling LLM|parallel_tools|result_sizes|tool_ids="
  tmux_send "$session" "blocked prompt"
  tmux_key "$session" Enter
  tmux_wait_for "$session" "Read-only mode|turn/start disabled|read-only" 10
  capture="$(tmux_capture_clean "$session" "$name")"
  if printf '%s\n' "$capture" | grep -q "$RA_TMUX_AUTH_TOKEN"; then
    tmux_fail "$session" "auth token leaked into cleaned capture"
  fi
  tmux_key "$session" C-q
  sleep 0.5
  tmux_kill "$session"
}

run_protocol_reconnect_interrupt_probe() {
  local endpoint="$1"
  local session_id="$2"
  local profile_id="$3"
  local name="live-protocol-probe"
  local session
  local command
  session="$(tmux_session_name "$name")"
  command="RA_TMUX_WS_ENDPOINT=$(shell_quote "${endpoint}?token=${RA_TMUX_AUTH_TOKEN}") RA_TMUX_SESSION_ID=$(shell_quote "$session_id") RA_TMUX_PROFILE_ID=$(shell_quote "$profile_id") node <<'NODE'
const endpoint = process.env.RA_TMUX_WS_ENDPOINT;
const sessionId = process.env.RA_TMUX_SESSION_ID;
const profileId = process.env.RA_TMUX_PROFILE_ID;

function openSocket() {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(endpoint);
    const timer = setTimeout(() => reject(new Error('WebSocket open timeout')), 10000);
    ws.addEventListener('open', () => {
      clearTimeout(timer);
      resolve(ws);
    }, { once: true });
    ws.addEventListener('error', () => {
      clearTimeout(timer);
      reject(new Error('WebSocket error'));
    }, { once: true });
  });
}

function rpc(ws, id, method, params) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      ws.removeEventListener('message', onMessage);
      reject(new Error(method + ' timeout'));
    }, 10000);
    function onMessage(event) {
      const frame = JSON.parse(event.data);
      if (frame.id !== id) return;
      clearTimeout(timer);
      ws.removeEventListener('message', onMessage);
      resolve(frame);
    }
    ws.addEventListener('message', onMessage);
    ws.send(JSON.stringify({ jsonrpc: '2.0', id, method, params }));
  });
}

function assertOk(condition, message) {
  if (!condition) throw new Error(message);
}

(async () => {
  const first = await openSocket();
  const open1 = await rpc(first, 'open-1', 'session/open', {
    session_id: sessionId,
    profile_id: profileId,
  });
  assertOk(open1.result && open1.result.opened, 'first session/open failed');
  console.log('PROBE reconnect-open-1');
  first.close();

  const second = await openSocket();
  const open2 = await rpc(second, 'open-2', 'session/open', {
    session_id: sessionId,
    profile_id: profileId,
  });
  assertOk(open2.result && open2.result.opened, 'second session/open failed');
  console.log('PROBE reconnect-open-2');

  const turnId = crypto.randomUUID();
  const started = await rpc(second, 'turn-1', 'turn/start', {
    session_id: sessionId,
    turn_id: turnId,
    input: [{ kind: 'text', text: 'tmux protocol no-provider probe' }],
  });
  if (started.error) {
    assertOk(
      started.error.data && started.error.data.kind === 'runtime_unavailable',
      'turn/start returned unexpected error: ' + JSON.stringify(started.error),
    );
    console.log('PROBE no-provider-runtime-unavailable');
  } else {
    assertOk(started.result && started.result.accepted === true, 'turn/start did not accept');
    console.log('PROBE turn-start-accepted');
  }

  const interrupted = await rpc(second, 'interrupt-1', 'turn/interrupt', {
    session_id: sessionId,
    turn_id: turnId,
  });
  assertOk(interrupted.result && typeof interrupted.result.interrupted === 'boolean', 'interrupt result missing');
  console.log('PROBE interrupt-result=' + interrupted.result.interrupted);
  second.close();
})().catch((error) => {
  console.error('PROBE failed:', error.message);
  process.exit(1);
});
NODE"
  run_line_capture \
    "$name" \
    "$command" \
    "PROBE reconnect-open-1" \
    "PROBE reconnect-open-2" \
    "PROBE no-provider-runtime-unavailable|PROBE turn-start-accepted" \
    "PROBE interrupt-result=(false|true)"
}

run_default() {
  tmux_require
  resolve_ra_tui_bin_cmd
  tmux_init_artifacts
  tmux_log "artifacts: $RA_TMUX_ARTIFACT_DIR"

  run_line_capture "ra-help" "$RA_BIN_CMD --help" "Usage: ra" "Commands:" "serve"
  run_line_capture \
    "ra-tui-help" \
    "$RA_TUI_BIN_CMD --mode protocol --help" \
    "Usage: ra.*\[OPTIONS\]" \
    "--mode" \
    "--endpoint" \
    "--session" \
    "--profile-id" \
    "--cwd" \
    "--auth-token" \
    "--readonly"
  run_tui_mock
  run_tui_mock_approval_kind "command" "cargo test -p ra-core ui_protocol"
  run_tui_mock_approval_kind "diff" "Mock approval diff|src/coding_loop.rs|Diff Preview"
  run_tui_mock_approval_kind "filesystem" "/tmp/ra-mock-approval.txt"
  run_tui_mock_approval_kind "network" "https://example.com"
  run_tui_mock_approval_kind "sandbox_escalation" "danger-full-access"
  run_tui_protocol_readonly
  run_line_expect_failure \
    "ra-tui-bad-endpoint" \
    "$RA_TUI_BIN_CMD --mode protocol --endpoint https://example.test/ui-protocol --readonly" \
    "endpoint must be a WebSocket URL"

  tmux_assert_no_registered_sessions
  tmux_assert_no_orphan_sessions
  tmux_log "default lane passed"
}

run_live() {
  tmux_require
  resolve_ra_tui_bin_cmd
  if [ "${RA_TMUX_LIVE:-0}" != "1" ]; then
    tmux_log "SKIP: live lane requires RA_TMUX_LIVE=1"
    exit 0
  fi

  tmux_init_artifacts
  tmux_log "artifacts: $RA_TMUX_ARTIFACT_DIR"

  local host="${RA_TMUX_LIVE_HOST:-127.0.0.1}"
  local port="${RA_TMUX_LIVE_PORT:-50190}"
  local data_dir="${RA_TMUX_LIVE_DATA_DIR:-$RA_TMUX_ARTIFACT_DIR/live-data}"
  local session_id="${RA_TMUX_LIVE_SESSION:-coding:local:prototype#m9-live}"
  local profile_id="${RA_TMUX_LIVE_PROFILE_ID:-coding}"
  local endpoint="ws://${host}:${port}/api/ui-protocol/ws"
  local server_session
  local tui_session
  local server_cmd
  local tui_cmd
  local capture

  server_session="$(tmux_session_name "live-server")"
  tui_session="$(tmux_session_name "live-tui")"
  server_cmd="$RA_BIN_CMD serve --host $(shell_quote "$host") --port $(shell_quote "$port") --data-dir $(shell_quote "$data_dir") --cwd $(shell_quote "$ROOT_DIR") --auth-token $(shell_quote "$RA_TMUX_AUTH_TOKEN")"
  tui_cmd="$RA_TUI_BIN_CMD --mode protocol --endpoint $(shell_quote "$endpoint") --cwd $(shell_quote "$ROOT_DIR") --auth-token $(shell_quote "$RA_TMUX_AUTH_TOKEN")"
  if [ -n "$session_id" ]; then
    tui_cmd="$tui_cmd --session $(shell_quote "$session_id") --profile-id $(shell_quote "$profile_id")"
  fi

  tmux_new_default "$server_session" bash -lc "$server_cmd"
  tmux_wait_for "$server_session" "Listening: http://${host}:${port}" 45

  tmux_new_default "$tui_session" bash -lc "$tui_cmd"
  tmux_wait_for "$tui_session" "Protocol backend connected|Connected to ra-ui|Opened" 45
  tmux_assert_capture "$tui_session" "Protocol backend connected|Connected to ra-ui|Opened"
  tmux_send "$tui_session" "tmux live protocol smoke"
  tmux_key "$tui_session" Enter
  tmux_wait_for "$tui_session" "tmux live protocol smoke|Turn started|turn/start request|Agent not available" 20
  run_protocol_reconnect_interrupt_probe "$endpoint" "$session_id" "$profile_id"

  capture="$(tmux_capture_clean "$tui_session" "live-tui")"
  if printf '%s\n' "$capture" | grep -q "$RA_TMUX_AUTH_TOKEN"; then
    tmux_fail "$tui_session" "auth token leaked into live TUI capture"
  fi
  if printf '%s\n' "$capture" | grep -Eq "INFO calling LLM|parallel_tools|result_sizes|tool_ids="; then
    tmux_fail "$tui_session" "trace log line leaked into live TUI capture"
  fi

  tmux_key "$tui_session" C-q
  sleep 0.5
  tmux_kill "$tui_session"
  tmux_kill "$server_session"

  tmux_assert_no_registered_sessions
  tmux_assert_no_orphan_sessions
  tmux_log "live lane passed"
}

# M9-FIX-09: wire-level protocol harness lane. Boots `ra serve` headless
# and drives the UI Protocol v1 directly through `e2e/tests/m9-protocol-*.spec.ts`,
# bypassing the TUI binary. Use this to catch wire regressions without
# depending on a working TUI build.
run_m9_protocol() {
  tmux_require
  tmux_init_artifacts
  tmux_log "artifacts: $RA_TMUX_ARTIFACT_DIR"

  if [ -z "${RA_BIN:-}" ] && [ ! -x "$ROOT_DIR/target/debug/ra" ] && [ -f "$ROOT_DIR/Cargo.toml" ]; then
    tmux_log "building ra CLI before starting tmux server"
    cargo build --manifest-path "$ROOT_DIR/Cargo.toml" -p ra-cli --features api --bin ra
    RA_BIN_CMD="$ROOT_DIR/target/debug/ra"
  fi

  local host="${RA_TMUX_M9_HOST:-127.0.0.1}"
  local port="${RA_TMUX_M9_PORT:-50191}"
  local data_dir="${RA_TMUX_M9_DATA_DIR:-${TMPDIR:-/tmp}/ra-m9-data-${RA_TMUX_RUN_ID}}"
  local server_session
  local server_cmd
  local server_wrapped
  local server_start_timeout="${RA_TMUX_M9_SERVER_START_TIMEOUT:-120}"
  local llm_api_key="${RA_TMUX_M9_LLM_API_KEY:-ra-m9-dummy-key}"
  server_session="$(tmux_session_name "m9-server")"
  server_cmd="RA_M9_PROTOCOL_FIXTURES=1 OPENAI_API_KEY=$(shell_quote "$llm_api_key") $RA_BIN_CMD serve --host $(shell_quote "$host") --port $(shell_quote "$port") --data-dir $(shell_quote "$data_dir") --cwd $(shell_quote "$ROOT_DIR") --auth-token $(shell_quote "$RA_TMUX_AUTH_TOKEN") --provider openai --model gpt-4o --no-retry"
  printf -v server_wrapped '%s\nrc=$?\nprintf "\\n__ra_TMUX_SERVER_EXIT:%%s__\\n" "$rc"\nwhile :; do sleep 3600; done\n' "$server_cmd"

  tmux_new_default "$server_session" bash -lc "$server_wrapped"
  tmux_wait_for "$server_session" "Listening: http://${host}:${port}|__ra_TMUX_SERVER_EXIT:[0-9]+__" "$server_start_timeout"
  tmux_assert_not_capture "$server_session" "__ra_TMUX_SERVER_EXIT:"
  tmux_assert_capture "$server_session" "Listening: http://${host}:${port}"

  pushd "$ROOT_DIR/e2e" >/dev/null
  if [ ! -d node_modules ]; then
    tmux_log "installing e2e deps"
    npm install >/dev/null
  fi
  RA_LIVE_URL="http://${host}:${port}" \
  RA_LIVE_TOKEN="$RA_TMUX_AUTH_TOKEN" \
  RA_M9_APPROVAL_FIXTURE=1 \
  RA_M9_REPLAY_LOSSY_FIXTURE=1 \
    npx playwright test --workers=1 tests/m9-protocol-*.spec.ts --reporter=line
  popd >/dev/null

  tmux_kill "$server_session"
  tmux_assert_no_registered_sessions
  tmux_assert_no_orphan_sessions
  tmux_log "m9-protocol lane passed"
}

case "$LANE" in
  default)
    run_default
    ;;
  live)
    run_live
    ;;
  m9-protocol)
    run_m9_protocol
    ;;
  *)
    echo "usage: $0 [default|live|m9-protocol]" >&2
    exit 2
    ;;
esac
