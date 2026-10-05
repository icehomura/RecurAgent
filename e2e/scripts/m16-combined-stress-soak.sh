#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"

run_id="${RA_M16_COMBINED_RUN_ID:-m16-combined-stress-$(date -u +%Y%m%dT%H%M%SZ)}"
out_root="${RA_M16_COMBINED_OUT_ROOT:-$repo_root/e2e/test-results-m16-combined-stress}"
out_dir="${RA_M16_COMBINED_OUT_DIR:-$out_root/$run_id}"

usage() {
  cat <<'USAGE'
Usage: e2e/scripts/m16-combined-stress-soak.sh <run|self-test|help>

Runs the M16 combined stress evidence suite:
  1. model-backed native/CLI/MCP specialist visual TUI fanout soak
  2. mid-turn crash/restart ContextManager stdio soak
  3. pressure restart/reconnect visual TUI context soak

The native fanout phase requires a real provider key. Set one of:
  RA_M16_NATIVE_API_KEY, RA_M15_NATIVE_API_KEY, or DEEPSEEK_API_KEY.

For local contract iteration without a provider key:
  RA_M16_COMBINED_SKIP_NATIVE=1 e2e/scripts/m16-combined-stress-soak.sh run
USAGE
}

die() {
  echo "$*" >&2
  exit 1
}

json_bool() {
  case "$1" in
    0|false|False|FALSE|"") printf 'false' ;;
    *) printf 'true' ;;
  esac
}

write_summary() {
  local status="$1"
  local fanout_status="$2"
  local crash_status="$3"
  local pressure_status="$4"
  node - "$out_dir/summary.json" "$status" "$fanout_status" "$crash_status" "$pressure_status" <<'NODE'
const fs = require('fs');
const path = require('path');
const [file, status, fanoutStatus, crashStatus, pressureStatus] = process.argv.slice(2);
const outDir = path.dirname(file);
const value = {
  schema: 'ra.m16.combined_stress_soak.v1',
  generated_at: new Date().toISOString().replace(/\.\d{3}Z$/, 'Z'),
  status,
  output_dir: outDir,
  phases: {
    fanout_tui: {
      status: fanoutStatus,
      artifact_dir: path.join(outDir, 'fanout-tmux'),
    },
    crash_stdio: {
      status: crashStatus,
      artifact_dir: path.join(outDir, 'crash-stdio'),
      summary: path.join(outDir, 'crash-stdio', 'm16-context-crash-stdio-summary.json'),
    },
    pressure_tui: {
      status: pressureStatus,
      artifact_dir: path.join(outDir, 'context-pressure-tmux'),
      validation: path.join(outDir, 'context-pressure-tmux', 'm16-context-restart-tmux-validation.json'),
    },
  },
};
fs.mkdirSync(path.dirname(file), { recursive: true });
fs.writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`);
NODE
}

run_phase() {
  local name="$1"
  shift
  echo "==> $name"
  "$@"
}

run_soak() {
  mkdir -p "$out_dir"

  local skip_native
  skip_native="$(json_bool "${RA_M16_COMBINED_SKIP_NATIVE:-0}")"
  local fanout_status="skipped"
  local crash_status="pending"
  local pressure_status="pending"
  local overall_status="failed"

  if [[ "$skip_native" == "false" ]]; then
    if run_phase "M16 visual native/CLI/MCP fanout" \
      env \
        RA_M16_UX_OUT_DIR="$out_dir/fanout-tmux" \
        RA_M16_UX_RUN_ID="$run_id-fanout" \
        RA_M16_BUILD="${RA_M16_BUILD:-0}" \
        RA_M16_BUILD_TUI="${RA_M16_BUILD_TUI:-0}" \
        "$script_dir/m16-live-tui-tmux-soak.sh" run; then
      fanout_status="passed"
    else
      fanout_status="failed"
    fi
  fi

  if run_phase "M16 mid-turn crash/restart context recovery" \
    env \
      RA_M16_CONTEXT_CRASH_DIR="$out_dir/crash-stdio" \
      RA_M16_CONTEXT_CRASH_POST_TURNS="${RA_M16_CONTEXT_CRASH_POST_TURNS:-1}" \
      RA_M16_CONTEXT_CRASH_PRESSURE_REPEAT="${RA_M16_CONTEXT_CRASH_PRESSURE_REPEAT:-240}" \
      RA_M16_CONTEXT_CRASH_RESPONSE_DELAY_MS="${RA_M16_CONTEXT_CRASH_RESPONSE_DELAY_MS:-8000}" \
      "$script_dir/m16-context-crash-stdio-soak.mjs"; then
    crash_status="passed"
  else
    crash_status="failed"
  fi

  if run_phase "M16 pressure restart/reconnect visual context status" \
    env \
      RA_M16_CONTEXT_TMUX_OUT_DIR="$out_dir/context-pressure-tmux" \
      RA_M16_CONTEXT_TMUX_RUN_ID="$run_id-pressure" \
      RA_M16_CONTEXT_BUILD="${RA_M16_CONTEXT_BUILD:-0}" \
      RA_M16_CONTEXT_BUILD_TUI="${RA_M16_CONTEXT_BUILD_TUI:-0}" \
      RA_M16_CONTEXT_RESTART_PRE_TURNS="${RA_M16_CONTEXT_RESTART_PRE_TURNS:-3}" \
      RA_M16_CONTEXT_RESTART_POST_TURNS="${RA_M16_CONTEXT_RESTART_POST_TURNS:-2}" \
      RA_M16_CONTEXT_RESTART_PRESSURE_REPEAT="${RA_M16_CONTEXT_RESTART_PRESSURE_REPEAT:-240}" \
      "$script_dir/m16-context-restart-tmux-soak.sh" run; then
    pressure_status="passed"
  else
    pressure_status="failed"
  fi

  if [[ "$crash_status" == "passed" && "$pressure_status" == "passed" ]]; then
    if [[ "$skip_native" == "true" || "$fanout_status" == "passed" ]]; then
      overall_status="passed"
    fi
  fi

  write_summary "$overall_status" "$fanout_status" "$crash_status" "$pressure_status"
  echo "M16 combined stress artifacts: $out_dir"
  [[ "$overall_status" == "passed" ]]
}

self_test() {
  bash -n "$0"
  node --check "$script_dir/m16-context-crash-stdio-soak.mjs"
  node --check "$script_dir/m16-context-restart-stdio-soak.mjs"
  bash -n "$script_dir/m16-context-restart-tmux-soak.sh"
  bash -n "$script_dir/m16-live-tui-tmux-soak.sh"
  echo "Self-test passed"
}

case "${1:-help}" in
  run) run_soak ;;
  self-test) self_test ;;
  help|-h|--help) usage ;;
  *) usage; exit 2 ;;
esac
