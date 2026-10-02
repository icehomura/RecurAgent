#!/usr/bin/env bash
# Install the author's "Flywheel" tool chain.
#
# Strategy (env TOOLS_STRATEGY):
#   acfs   - run the author's own bootstrap with --profile "${ACFS_PROFILE}"
#            (default "stack-only": the tool chain, no shell/agents/cloud).
#            Falls back to `direct` when ACFS fails and ACFS_FALLBACK != 0.
#   direct - install each tool from its own upstream install.sh (best effort).
#   none   - do nothing.
#
# GitHub raw files are fetched through a mirror because raw.githubusercontent.com
# is unreachable on some networks (env GH_RAW_MODE):
#   jsdelivr (default) -> https://cdn.jsdelivr.net/gh/OWNER/REPO@REF/PATH
#   ghproxy            -> https://gh-proxy.com/https://raw.githubusercontent.com/...
#   direct             -> https://raw.githubusercontent.com/...
#
# NOTE: the fetched *installers* then download release artifacts and may fetch
# further scripts from raw.githubusercontent.com internally. On a network where
# that host is blocked, supply a proxy at build time
# (--build-arg HTTPS_PROXY=...); GH_RAW_MODE only mirrors the entry-point files.
set -uo pipefail

STRATEGY="${TOOLS_STRATEGY:-direct}"
PROFILE="${ACFS_PROFILE:-stack-only}"
ACFS_REF="${ACFS_REF:-main}"
ACFS_FALLBACK="${ACFS_FALLBACK:-1}"
GH_RAW_MODE="${GH_RAW_MODE:-jsdelivr}"
GH_RELEASE_MODE="${GH_RELEASE_MODE:-ghproxy}"

# The gh-proxy shims are installed at /usr/local/bin (see the Dockerfile), which
# precedes /usr/bin on PATH, so every github.com release download below — and
# inside the installers — already goes through the proxy. Nothing is prepended
# to PATH here: an installer that drops its binary into PATH's first entry must
# find ~/.local/bin, not this script's own directory.
if [[ "$GH_RELEASE_MODE" == "ghproxy" ]]; then
  echo "==> GitHub release downloads via gh-proxy.com (shim at /usr/local/bin/curl)"
fi

LOG_DIR="/tmp/flywheel-install-logs"
mkdir -p "$LOG_DIR"

declare -a INSTALLED=()
declare -a FAILED=()

record_ok()   { INSTALLED+=("$1"); printf '[ ok ]   %s\n' "$1"; }
record_fail() { FAILED+=("$1");    printf '[FAIL]  %s\n' "$1"; }

raw_url() {
  # raw_url <owner> <repo> <path> [ref]
  local owner="$1" repo="$2" path="$3" ref="${4:-main}"
  case "$GH_RAW_MODE" in
    jsdelivr) printf 'https://cdn.jsdelivr.net/gh/%s/%s@%s/%s' "$owner" "$repo" "$ref" "$path" ;;
    ghproxy)  printf 'https://gh-proxy.com/https://raw.githubusercontent.com/%s/%s/%s/%s' "$owner" "$repo" "$ref" "$path" ;;
    direct)   printf 'https://raw.githubusercontent.com/%s/%s/%s/%s' "$owner" "$repo" "$ref" "$path" ;;
    *) echo "unknown GH_RAW_MODE='$GH_RAW_MODE' (expected jsdelivr|ghproxy|direct)" >&2; return 2 ;;
  esac
}

raw_urls() {
  # Every mirror for <owner> <repo> <path> [ref], preferred one first. jsdelivr
  # intermittently 404s a file it has not cached (dcg's install.sh, once) and
  # raw.githubusercontent.com is unreachable on some networks, so the entry file
  # is fetched from whichever mirror answers.
  local owner="$1" repo="$2" path="$3" ref="${4:-main}"
  local primary="" alt
  primary="$(raw_url "$owner" "$repo" "$path" "$ref")" || return 2
  printf '%s\n' "$primary"
  for alt in \
    "https://gh-proxy.com/https://raw.githubusercontent.com/${owner}/${repo}/${ref}/${path}" \
    "https://raw.githubusercontent.com/${owner}/${repo}/${ref}/${path}" \
    "https://cdn.jsdelivr.net/gh/${owner}/${repo}@${ref}/${path}"; do
    [[ "$alt" != "$primary" ]] && printf '%s\n' "$alt"
  done
}

# fetch_installer <dest> <owner> <repo> <path> <ref>
# Writes the first mirror that answers to <dest>. Returns 1 if none do.
fetch_installer() {
  local dest="$1" owner="$2" repo="$3" path="$4" ref="${5:-main}"
  local url
  while IFS= read -r url; do
    if curl -fsSL --connect-timeout 20 --max-time 300 "$url" -o "$dest" && [[ -s "$dest" ]]; then
      return 0
    fi
  done < <(raw_urls "$owner" "$repo" "$path" "$ref")
  return 1
}

# ---------------------------------------------------------------------------
# ACFS: the author's canonical full-toolchain bootstrap
# ---------------------------------------------------------------------------
install_via_acfs() {
  echo "==> Installing Flywheel tool chain via ACFS (profile=${PROFILE}, ref=${ACFS_REF}, raw=${GH_RAW_MODE})"
  local log="$LOG_DIR/acfs.log"
  local src; src="$(mktemp)"
  if ! fetch_installer "$src" Dicklesworthstone agentic_coding_flywheel_setup install.sh "$ACFS_REF"; then
    record_fail "acfs:${PROFILE} (installer unreachable from every mirror)"
    printf '        see %s\n' "$log"
    rm -f "$src"
    return 1
  fi
  # --yes                 non-interactive
  # --profile             stack-only = tool chain only
  # --skip-ubuntu-upgrade do not attempt a distribution upgrade inside the image
  # --no-auto-fix         never block on an auto-fix prompt
  # timeout               bound the run so a hang falls back to direct installers
  local rc=0
  timeout "${ACFS_TIMEOUT:-2400}" bash -s -- --yes --profile "$PROFILE" \
      --skip-ubuntu-upgrade --no-auto-fix < "$src" 2>&1 | tee "$log" || rc=$?
  rm -f "$src"
  if [[ "$rc" == 0 ]]; then
    record_ok "acfs:${PROFILE}"
    return 0
  fi
  record_fail "acfs:${PROFILE}"
  printf '        see %s\n' "$log"
  return 1
}

# ---------------------------------------------------------------------------
# Direct installers (fallback / alternative)
# ---------------------------------------------------------------------------
# install_tool <name> <owner> <repo> <path> [ref] -- [installer args...]
install_tool() {
  local name="$1" owner="$2" repo="$3" path="$4" ref="$5"; shift 5
  [[ "${1:-}" == "--" ]] && shift
  local log="$LOG_DIR/${name}.log"
  local src; src="$(mktemp)"
  if ! fetch_installer "$src" "$owner" "$repo" "$path" "$ref"; then
    record_fail "$name (installer unreachable from every mirror)"
    printf '        see %s\n' "$log"
    rm -f "$src"
    return 1
  fi
  local rc=0
  timeout "${TOOL_TIMEOUT:-900}" bash -s -- "$@" < "$src" 2>&1 | tee "$log" || rc=$?
  rm -f "$src"
  if [[ "$rc" == 0 ]]; then
    record_ok "$name"
    return 0
  fi
  record_fail "$name"
  printf '        see %s\n' "$log"
  return 1
}

# install_url <name> <url> [-- installer args...]
# For tools that publish no installer inside their repository.
install_url() {
  local name="$1" url="$2"; shift 2
  [[ "${1:-}" == "--" ]] && shift
  local log="$LOG_DIR/${name}.log"
  if curl -fsSL "$url" | timeout "${TOOL_TIMEOUT:-900}" bash -s -- "$@" 2>&1 | tee "$log"; then
    record_ok "$name"
    return 0
  fi
  record_fail "$name"
  printf '        see %s\n' "$log"
  return 1
}

install_direct() {
  echo "==> Installing Flywheel tools directly from their own installers"
  # Rust (install.sh defaults to a prebuilt release; --easy-mode adds PATH + doctor)
  install_tool br   Dicklesworthstone beads_rust                 install.sh main            -- || true
  install_tool cass Dicklesworthstone coding_agent_session_search install.sh main           -- --easy-mode --verify || true
  install_tool dcg  Dicklesworthstone destructive_command_guard  install.sh master          -- --easy-mode || true
  install_tool rch  Dicklesworthstone remote_compilation_helper   install.sh main           -- --easy-mode || true
  install_tool pt   Dicklesworthstone process_triage             install.sh main            -- || true

  # Shell
  install_tool dsr  Dicklesworthstone doodlestein_self_releaser  install.sh main            -- --no-configure --easy-mode || true
  install_tool ru   Dicklesworthstone repo_updater               install.sh main            -- || true
  install_tool srps Dicklesworthstone system_resource_protection_script install.sh main     -- || true

  # Go
  install_tool bv   Dicklesworthstone beads_viewer               install.sh main            -- || true
  install_tool ntm  Dicklesworthstone ntm                        install.sh main            -- --easy-mode || true
  install_tool slb  Dicklesworthstone slb                        scripts/install.sh main    -- || true
  install_tool caam Dicklesworthstone coding_agent_account_manager install.sh main          -- || true

  # Python (needs uv, installed by the image)
  install_tool ubs  Dicklesworthstone ultimate_bug_scanner        install.sh main           -- || true
  # --no-start: without it the installer finishes by running the MCP server in
  # the foreground and the tool timeout kills it (the install itself succeeded).
  install_tool am   Dicklesworthstone mcp_agent_mail              scripts/install.sh main   -- --yes --no-start || true
  install_tool cm   Dicklesworthstone cass_memory_system          install.sh main           -- --easy-mode --verify || true

  # TypeScript / other
  # No --verify: brenner's post-install `brenner doctor --json` fails in a
  # container (no operator config) and marks an otherwise good install failed.
  install_tool brenner Dicklesworthstone brenner_bot              install.sh main           -- --easy-mode || true
  install_tool apr     Dicklesworthstone automated_plan_reviser_pro install.sh main         -- || true
  # jfp publishes no installer in its repository; the project site serves the
  # canonical one (verified HTTP 200, takes no flags).
  install_url  jfp     https://jeffreysprompts.com/install-cli.sh || true
}

print_summary() {
  echo
  echo "==> Flywheel install summary"
  if ((${#INSTALLED[@]})); then
    printf '    installed: %s\n' "${INSTALLED[*]}"
  fi
  if ((${#FAILED[@]})); then
    printf '    failed   : %s\n' "${FAILED[*]}"
    printf '    logs     : %s\n' "$LOG_DIR"
  else
    echo "    all requested tools installed"
  fi
}

case "$STRATEGY" in
  acfs)
    install_via_acfs || {
      if [[ "$ACFS_FALLBACK" != "0" ]]; then
        echo "==> ACFS failed; falling back to direct installers"
        install_direct
      fi
    }
    ;;
  direct)
    install_direct
    ;;
  none)
    echo "==> TOOLS_STRATEGY=none: skipping third-party tools"
    ;;
  *)
    echo "unknown TOOLS_STRATEGY='$STRATEGY' (expected acfs|direct|none)" >&2
    exit 2
    ;;
esac

print_summary
exit 0
