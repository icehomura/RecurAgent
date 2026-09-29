#!/usr/bin/env bash
# Scratch verification runner for agent sessions (routes cargo through rch).
set -u
cd /Users/jemanuel/projects/recur_agent || exit 1
rch exec -- cargo "$@"
