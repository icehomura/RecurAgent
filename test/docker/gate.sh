#!/usr/bin/env bash
# Run the project's quality gate in the container — from anywhere in the repo,
# with no host toolchain, no dsr, no rch, and no worker fleet.
#
#   test/docker/gate.sh              # gate the committed source: clone the fork
#                                    # into a named volume and run all 7 checks
#   test/docker/gate.sh --worktree   # gate this working tree (bind mount, so
#                                    # builds land on the host filesystem: slower)
#   test/docker/gate.sh --shell      # interactive shell in the same container
#
# Extra arguments are passed to the service as its command, e.g.
#   test/docker/gate.sh bash -c 'ls /data/projects'
#
# Exits with the gate's status: 0 only when every required check in
# .dsr/repos.yaml passed. The container prints a per-check PASS/FAIL summary.
#
# See AGENTS.md, "Running the Quality Gate in Docker": a missing host tool is
# not a blocker and must not be reported as one — this script is the fallback.
set -euo pipefail
cd "$(dirname "$0")"

mode=clone
extra=()
for arg in "$@"; do
  case "$arg" in
    --worktree) mode=worktree ;;
    --shell)    mode=shell ;;
    *)          extra+=("$arg") ;;
  esac
done

if ! docker info >/dev/null 2>&1; then
  cat >&2 <<'EOF'
error: the Docker daemon is not responding — the only condition that blocks the
container gate. Fix the daemon, do not fall back to "no toolchain on this host":

  - Docker Desktop not started?            docker desktop start
  - WSL2 backend wedged (commands hang with Hcs/E_CONNECTION_TIMEOUT)?
        wsl --shutdown   &&   docker desktop start
  - Host volume full?                      docker builder prune -f
EOF
  exit 2
fi

image="recur-agent-gates:${IMAGE_TAG:-local}"
if ! docker image inspect "$image" >/dev/null 2>&1; then
  echo "==> image ${image} is missing; building it first (network-bound, ~10 min)"
  ./compose.sh build
fi

case "$mode" in
  clone)    cmd=(./compose.sh run --rm gate) ;;
  worktree) cmd=(./compose.sh --profile dev run --rm dev) ;;
  shell)    cmd=(./compose.sh run --rm shell) ;;
esac
cmd+=("${extra[@]}")

exec "${cmd[@]}"
