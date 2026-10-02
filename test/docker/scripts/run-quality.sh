#!/usr/bin/env bash
# Run the repository's DSR quality recipe (.dsr/repos.yaml) with a local compile.
#
# The registered recipe routes every Cargo command through `rch` with
# RCH_REQUIRE_REMOTE=1, which fails closed when no SSH worker can take the job.
# A container has no fleet, so this script reads the recipe's check list and
# executes each check verbatim with only the `rch exec --` transport prefix
# stripped — cargo then compiles here instead of on a worker.
#
# The check list is READ from .dsr/repos.yaml rather than copied into this file,
# so it cannot drift from what `dsr quality --tool <tool>` runs. (A hand-copied
# equivalent silently missed `scripts/check_fixture_read_patience.py`.)
#
# Env:
#   REPO_DIR     checkout to test          (default /data/projects/RecurAgent)
#   DSR_RECIPE   recipe file               (default $REPO_DIR/.dsr/repos.yaml)
#   DSR_TOOL     tools.<key> to read       (default: first key in the recipe)
#   CARGO_BUILD_JOBS, TMPDIR, CARGO_*      forwarded to the checks
set -uo pipefail

REPO_DIR="${REPO_DIR:-/data/projects/RecurAgent}"
RECIPE="${DSR_RECIPE:-${REPO_DIR}/.dsr/repos.yaml}"

cd "$REPO_DIR" || { echo "no checkout at ${REPO_DIR}" >&2; exit 2; }

# Bound the target tree the same way the recipe does (72 GB of debug test
# binaries otherwise; see .dsr/repos.yaml and AGENTS.md "Compiler and Test
# Checks"). RCH stays out of the path: this is a plain local compile.
export RCH_DISABLED=1
export CARGO_INCREMENTAL=0
export CARGO_PROFILE_TEST_DEBUG=0
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
export TMPDIR="${TMPDIR:-/tmp}"
# tests/provider_streaming.rs needs an authoritative commit; the clone has .git.
export PI_PROVIDER_REPLAY_GIT_COMMIT="${PI_PROVIDER_REPLAY_GIT_COMMIT:-$(git rev-parse HEAD 2>/dev/null || echo unknown)}"

if ! command -v yq >/dev/null 2>&1; then
  echo "yq is required to read the check list from ${RECIPE}" >&2
  exit 2
fi
if [[ ! -f "$RECIPE" ]]; then
  echo "missing recipe ${RECIPE}" >&2
  exit 2
fi

TOOL="${DSR_TOOL:-}"
if [[ -z "$TOOL" ]] || ! yq -e ".tools.${TOOL}.checks" "$RECIPE" >/dev/null 2>&1; then
  TOOL="$(yq -r '.tools | keys | .[0] // ""' "$RECIPE")"
fi
if [[ -z "$TOOL" ]]; then
  echo "no tools.<key> with checks in ${RECIPE}" >&2
  exit 2
fi

mapfile -t CHECKS < <(yq -r ".tools.${TOOL}.checks[]" "$RECIPE")
if (( ${#CHECKS[@]} == 0 )); then
  echo "recipe ${RECIPE} has no checks for ${TOOL}" >&2
  exit 2
fi

echo "==> recipe: ${RECIPE} (tool=${TOOL}, ${#CHECKS[@]} checks)"
echo "==> checkout: ${REPO_DIR} @ $(git rev-parse --short HEAD 2>/dev/null || echo 'no-git')"

declare -a RESULTS=()
overall=0

step() {
  local name="$1" cmd="$2"
  echo
  echo "===== ${name} ====="
  echo "\$ ${cmd}"
  if bash -c "$cmd"; then
    RESULTS+=("PASS  ${name}")
  else
    RESULTS+=("FAIL  ${name}")
    overall=1
  fi
}

for i in "${!CHECKS[@]}"; do
  raw="${CHECKS[$i]}"
  # Strip the rch transport prefix. Plain bash expansion, not sed: the pattern
  # is a fixed literal and `sed` implementations disagree on \b.
  local_cmd="${raw/rch exec -- /}"
  # Guard against the recipe changing shape (e.g. `rch -- exec`), which would
  # otherwise reach this container as a real rch invocation and fail closed.
  if [[ "$local_cmd" == *"rch exec"* ]]; then
    echo "unhandled rch invocation in check: $raw" >&2
    exit 2
  fi
  step "$((i + 1))/${#CHECKS[@]} ${local_cmd}" "$local_cmd"
done

# Optional extra: not part of the DSR recipe. It is reported, but it never
# changes the verdict — a bug scanner with findings must not turn a green
# seven-check recipe run into a failed gate.
if command -v ubs >/dev/null 2>&1; then
  echo
  echo "===== extra (not part of the recipe): ubs --only=rust . ====="
  echo "\$ ubs --only=rust ."
  if bash -c "ubs --only=rust ."; then
    RESULTS+=("extra PASS  ubs --only=rust .")
  else
    RESULTS+=("extra FAIL  ubs --only=rust . (advisory, verdict unchanged)")
  fi
fi

echo
echo "===== summary ====="
printf '%s\n' "${RESULTS[@]}"
if (( overall != 0 )); then
  echo "==> at least one required check failed"
else
  echo "==> all required checks passed"
fi
exit "$overall"
