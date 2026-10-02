#!/usr/bin/env bash
# Clone or refresh the RecurAgent fork, then exec the requested command.
#
# Env:
#   FLYWHEEL_REPO    git URL (default: the icehomura fork)
#   FLYWHEEL_BRANCH  branch (default: main)
#   REPO_DIR         checkout location (default: /data/projects/RecurAgent)
#   CLONE            1 = clone/pull at start, 0 = use what is already there
#   GIT_DEPTH        clone depth (default: 0 = full history)
set -euo pipefail

REPO_DIR="${REPO_DIR:-/data/projects/RecurAgent}"
REPO_URL="${FLYWHEEL_REPO:-https://github.com/icehomura/RecurAgent.git}"
BRANCH="${FLYWHEEL_BRANCH:-main}"
CLONE="${CLONE:-1}"
GIT_DEPTH="${GIT_DEPTH:-0}"

if [[ "$CLONE" == "1" ]]; then
  if [[ -d "$REPO_DIR/.git" ]]; then
    echo "==> Refreshing ${REPO_DIR} (origin=${REPO_URL}, branch=${BRANCH})"
    # A refresh failure must not kill the run: the workspace volume usually
    # already holds a usable checkout, and this image is built for networks
    # where github.com is throttled. Each step warns and continues.
    git -C "$REPO_DIR" remote set-url origin "$REPO_URL" \
      || git -C "$REPO_DIR" remote add origin "$REPO_URL" \
      || echo "==> could not set origin; continuing"
    git -C "$REPO_DIR" fetch --all --prune \
      || echo "==> fetch failed; continuing with the local checkout"
    git -C "$REPO_DIR" checkout "$BRANCH" \
      || echo "==> checkout ${BRANCH} failed; continuing with the current checkout"
    git -C "$REPO_DIR" pull --ff-only \
      || echo "==> pull --ff-only refused (dirty or diverged tree); continuing with current HEAD"
  else
    echo "==> Cloning ${REPO_URL} (branch=${BRANCH}) into ${REPO_DIR}"
    mkdir -p "$(dirname "$REPO_DIR")"
    if [[ "$GIT_DEPTH" != "0" ]]; then
      git clone --branch "$BRANCH" --depth "$GIT_DEPTH" "$REPO_URL" "$REPO_DIR"
    else
      git clone --branch "$BRANCH" "$REPO_URL" "$REPO_DIR"
    fi
  fi
  git -C "$REPO_DIR" --no-pager log -1 --oneline || true
fi

exec "$@"
