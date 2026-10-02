#!/usr/bin/env bash
# docker compose wrapper for this directory that supplies GITHUB_TOKEN from the
# gh CLI, so installers resolving releases through api.github.com (slb has no
# fallback) work. api.github.com allows only 60 anonymous requests per hour per
# IP; a token lifts that to 5000.
#
#   ./compose.sh build                 # build the image
#   ./compose.sh run --rm gate         # run the quality gate
#   ./compose.sh build --build-arg TOOLS_STRATEGY=none
#
# The token is read at run time and exported for this process only: it is never
# written to disk, never passed as a literal on the command line, and never
# enters the repository. An existing GITHUB_TOKEN/GH_TOKEN in the environment
# wins, so `GITHUB_TOKEN=... ./compose.sh build` also works.
set -euo pipefail
cd "$(dirname "$0")"

if [[ -z "${GITHUB_TOKEN:-}" && -z "${GH_TOKEN:-}" ]]; then
  # No `command -v gh` gate: on Windows/MSYS gh.exe can resolve for the shell
  # without `command -v` agreeing. Set GH_BIN to an absolute path when the
  # lookup fails, e.g.
  #   GH_BIN='/c/Program Files/GitHub CLI/gh.exe' ./compose.sh build
  gh_bin="${GH_BIN:-gh}"
  # `gh auth status` can report an invalid keyring while `gh auth token` still
  # returns a usable token, so trust the token, not the status.
  if tok="$("$gh_bin" auth token 2>/dev/null)" && [[ -n "$tok" ]]; then
    export GITHUB_TOKEN="$tok"
    # BuildKit excludes secret mounts from the cache key; this marker makes a
    # token build a distinct cache entry so it actually re-runs.
    export GH_TOKEN_PRESENT=1
    echo "==> GITHUB_TOKEN supplied by $gh_bin (not persisted)"
  else
    echo "==> no GitHub token available ($gh_bin auth token failed);" >&2
    echo "    continuing unauthenticated (60 req/h)" >&2
  fi
fi

if [[ $# -eq 0 ]]; then
  set -- build
fi
exec docker compose "$@"
