#!/usr/bin/env bash
#
# Run the compile and lint halves of the quality gate against
# x86_64-pc-windows-msvc on a Windows operator host (bd-o6hte).
#
# `dsr quality --tool recur_agent` only ever builds the rch worker's own
# triple, and this repository runs no CI, so nothing else notices when the
# Windows target stops compiling. It stopped somewhere after v0.3.0 and was
# not noticed until the v0.5.0 release build, by which point the release
# commit and tag already existed; v0.5.1 then found the target still broken
# under `--all-targets`. Run this BEFORE cutting a release commit.
#
#   scripts/check_windows_target.sh [<git-ref>]
#
# Defaults to the current `HEAD` commit. The ref must already be pushed —
# the Windows host fetches it from `origin`, it does not receive a work tree.
#
# Environment:
#   RECUR_AGENT_WINDOWS_HOST   ssh host to drive (default: wsurf)
#   RECUR_AGENT_WINDOWS_ROOT   checkout on that host (default: C:/Users/jeffr/pi-win-gate)
#   RECUR_AGENT_WINDOWS_POLL   seconds between progress polls (default: 120)
#   RECUR_AGENT_WINDOWS_STALL  polls with no log growth before giving up (default: 15)
#
# Exit status is the Windows cargo exit status: 0 only when both
# `cargo check --all-targets` and `cargo clippy --all-targets -- -D warnings`
# pass for the Windows triple.
#
# Expect roughly 25 minutes with warm dependencies and well over an hour from
# cold. The work runs as a Windows scheduled task so it outlives the ssh
# connection; this script only starts it and polls the log.

set -euo pipefail

HOST="${RECUR_AGENT_WINDOWS_HOST:-wsurf}"
REMOTE_ROOT="${RECUR_AGENT_WINDOWS_ROOT:-C:/Users/jeffr/pi-win-gate}"
POLL_SECONDS="${RECUR_AGENT_WINDOWS_POLL:-120}"
STALL_POLLS="${RECUR_AGENT_WINDOWS_STALL:-15}"
TASK_NAME="pi-windows-target-gate"

ref="${1:-HEAD}"
commit="$(git rev-parse "${ref}^{commit}")"

if ! git branch -r --contains "$commit" >/dev/null 2>&1 \
  || [ -z "$(git branch -r --contains "$commit" 2>/dev/null)" ]; then
  echo "error: $commit is not on any remote branch." >&2
  echo "       The Windows host fetches from origin; push it first." >&2
  exit 2
fi

# Windows paths for the parts the batch file needs; the remote root is given
# with forward slashes for scp and backslashes for cmd.
remote_root_win="${REMOTE_ROOT//\//\\}"
remote_log_win='C:\Users\jeffr\pi-windows-target-gate.log'
remote_log_posix='/c/Users/jeffr/pi-windows-target-gate.log'
remote_cmd_posix='C:/Users/jeffr/pi-windows-target-gate.cmd'

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT
batch="$workdir/gate.cmd"

# `vcvars64.bat` has to be sourced for the MSVC linker; the Community and
# BuildTools layouts are both tried because operator hosts have differed.
{
  printf '@echo off\r\n'
  printf 'setlocal\r\n'
  printf 'set ROOT=%s\r\n' "$remote_root_win"
  printf 'set LOG=%s\r\n' "$remote_log_win"
  printf 'set COMMIT=%s\r\n' "$commit"
  printf 'echo === pi windows target gate %%DATE%% %%TIME%% > "%%LOG%%"\r\n'
  printf 'if not exist "%%ROOT%%\\.git" git clone https://github.com/Dicklesworthstone/recur_agent.git "%%ROOT%%" >> "%%LOG%%" 2>&1\r\n'
  printf 'cd /d "%%ROOT%%"\r\n'
  printf 'git fetch origin >> "%%LOG%%" 2>&1\r\n'
  printf 'git checkout -f %%COMMIT%% >> "%%LOG%%" 2>&1\r\n'
  printf 'git rev-parse HEAD >> "%%LOG%%" 2>&1\r\n'
  # Best-effort only: the in-repo .cargo/config.toml pins rust-lld, so neither
  # `check` nor `clippy` needs the MSVC linker on PATH. Both `call`s printing
  # "The system cannot find the path specified" is normal on a host without
  # Visual Studio and must not stop the run.
  printf 'call "C:\\Program Files\\Microsoft Visual Studio\\2022\\Community\\VC\\Auxiliary\\Build\\vcvars64.bat" >> "%%LOG%%" 2>&1\r\n'
  printf 'if errorlevel 1 call "C:\\Program Files\\Microsoft Visual Studio\\2022\\BuildTools\\VC\\Auxiliary\\Build\\vcvars64.bat" >> "%%LOG%%" 2>&1\r\n'
  # A failed `call` leaves errorlevel set, and nothing between here and the
  # first cargo invocation clears it. Reset it so the step checks below report
  # their own command rather than inheriting this one.
  printf 'cmd /c "exit /b 0"\r\n'
  # rch's cargo shim would try to ship this to a Linux worker.
  printf 'set RCH_DISABLED=1\r\n'
  printf 'set RCH_CARGO_WRAPPER_BYPASS=1\r\n'
  printf 'set CARGO_INCREMENTAL=0\r\n'
  # A target directory of this gate's own. Sharing one with ad-hoc builds on
  # the host means blocking on its lock for however long that build runs, and
  # a leftover process from an abandoned run can hold it indefinitely.
  printf 'set CARGO_TARGET_DIR=%%ROOT%%\\target-windows-gate\r\n'
  # Every step records its own exit code. A run that dies between steps then
  # shows up as a missing marker rather than as a log that simply stops, which
  # is what made an earlier failure here impossible to diagnose from the log.
  printf 'echo ==== cargo check --all-targets >> "%%LOG%%"\r\n'
  printf 'cargo check --locked --all-targets --target x86_64-pc-windows-msvc --message-format short >> "%%LOG%%" 2>&1\r\n'
  printf 'set CHECK_STATUS=%%ERRORLEVEL%%\r\n'
  printf 'echo ==== check exit=%%CHECK_STATUS%% >> "%%LOG%%"\r\n'
  printf 'if not "%%CHECK_STATUS%%"=="0" (\r\n'
  printf '  echo === exit=%%CHECK_STATUS%% >> "%%LOG%%"\r\n'
  printf '  endlocal\r\n'
  printf '  exit /b 1\r\n'
  printf ')\r\n'
  printf 'echo ==== cargo clippy --all-targets -D warnings >> "%%LOG%%"\r\n'
  printf 'cargo clippy --locked --all-targets --target x86_64-pc-windows-msvc --message-format short -- -D warnings >> "%%LOG%%" 2>&1\r\n'
  printf 'set CLIPPY_STATUS=%%ERRORLEVEL%%\r\n'
  printf 'echo ==== clippy exit=%%CLIPPY_STATUS%% >> "%%LOG%%"\r\n'
  printf 'echo === exit=%%CLIPPY_STATUS%% >> "%%LOG%%"\r\n'
  printf 'endlocal\r\n'
} > "$batch"

# `cmd /c` over this ssh transport mangles switch arguments such as `/create`,
# so every remote invocation goes through PowerShell -EncodedCommand instead.
run_remote() {
  local script="$1"
  local encoded
  encoded="$(printf '%s' "$script" | iconv -f UTF-8 -t UTF-16LE | base64 | tr -d '\n')"
  ssh -o ConnectTimeout=25 "$HOST" "powershell -NoProfile -EncodedCommand $encoded" 2>&1 \
    | grep -v 'CLIXML' | grep -v '^<Objs'
}

echo "Windows target gate: $commit on $HOST"
scp -q "$batch" "${HOST}:${remote_cmd_posix}"

run_remote "schtasks.exe /create /tn ${TASK_NAME} /tr ${remote_cmd_posix//\//\\} /sc once /st 23:55 /f" >/dev/null
run_remote "schtasks.exe /run /tn ${TASK_NAME}" >/dev/null
echo "started; polling every ${POLL_SECONDS}s"

# cargo writes its progress bar with carriage returns, so a `\n`-only tail
# looks frozen for many minutes at a time. Translate them to newlines and
# report the last line that actually says something.
previous_size=""
stalled=0
while true; do
  sleep "$POLL_SECONDS"
  status="$(ssh -o ConnectTimeout=25 "$HOST" \
    "tr -d '\\r' < '${remote_log_posix}' | grep -E '^====|^=== exit|: error|error\\[' | tail -20" 2>/dev/null || true)"
  if printf '%s' "$status" | grep -q '^=== exit'; then
    break
  fi

  size="$(ssh -o ConnectTimeout=25 "$HOST" "wc -c < '${remote_log_posix}'" 2>/dev/null | tr -d ' ' || true)"
  last="$(ssh -o ConnectTimeout=25 "$HOST" \
    "tr '\\r' '\\n' < '${remote_log_posix}' | grep -v '^[[:space:]]*\$' | tail -1" 2>/dev/null || true)"
  printf '  ... %s\n' "${last:-(no output yet)}"

  # The task going Ready without an `=== exit` marker means the batch died
  # between steps. Waiting for a verdict that can no longer arrive just burns
  # the operator's time, so fail immediately with what the log does have.
  task_state="$(run_remote "(Get-ScheduledTask -TaskName '${TASK_NAME}').State" | tr -d '\r' | tr -d ' ')"
  if [ "$task_state" = "Ready" ]; then
    echo >&2
    echo "error: the task on ${HOST} finished without recording a verdict." >&2
    echo "       Last markers:" >&2
    ssh -o ConnectTimeout=25 "$HOST" \
      "tr -d '\\r' < '${remote_log_posix}' | grep -E '^====|^=== exit' | tail -10" >&2 2>/dev/null || true
    echo "       full log: ${HOST}:${remote_log_win}" >&2
    exit 4
  fi

  # A run that stops producing output is usually waiting on another cargo's
  # lock — most often a leftover process from an abandoned run on the host.
  # Say so rather than polling until the operator gives up.
  if [ -n "$size" ] && [ "$size" = "$previous_size" ]; then
    stalled=$((stalled + 1))
    if [ "$stalled" -ge "$STALL_POLLS" ]; then
      echo >&2
      echo "error: no output for $((STALL_POLLS * POLL_SECONDS))s; last line was:" >&2
      echo "       ${last}" >&2
      echo "       A file lock there means another cargo on ${HOST} owns the" >&2
      echo "       directory. Note that 'schtasks /end' ends the task but" >&2
      echo "       leaves its cargo child running, so an abandoned run keeps" >&2
      echo "       the lock until that process is killed. List the owners:" >&2
      echo "         ssh ${HOST} 'powershell -NoProfile -Command \"Get-CimInstance Win32_Process | ? { \\\$_.Name -eq \\\"cargo.exe\\\" } | %% { \\\"{0} {1}\\\" -f \\\$_.ProcessId, \\\$_.CommandLine }\"'" >&2
      echo "       Match on the command line before killing anything: this host" >&2
      echo "       builds other projects too." >&2
      exit 3
    fi
  else
    stalled=0
  fi
  previous_size="$size"
done

echo
echo "$status"
echo

# `grep -c` exits 1 when the count is zero, so the fallback has to live inside
# the remote command; otherwise a clean run appends a second "0" and the
# comparison below sees a two-line string.
errors="$(ssh -o ConnectTimeout=25 "$HOST" \
  "tr -d '\\r' < '${remote_log_posix}' | grep -cE ': error|error\\[' || true" 2>/dev/null \
  | tr -cd '0-9')"
errors="${errors:-0}"

# The `=== exit=N` marker the batch writes is the authoritative verdict; the
# diagnostic count is a cross-check for a run that somehow exits 0 with errors
# in the log.
exit_line="$(printf '%s\n' "$status" | grep '^=== exit' | tail -1)"

if printf '%s' "$exit_line" | grep -q 'exit=0' && [ "$errors" -eq 0 ]; then
  echo "PASS: Windows target compiles and lints clean at $commit"
  exit 0
fi

if [ -z "$exit_line" ]; then
  echo "FAIL: no verdict recorded for $commit" >&2
else
  echo "FAIL: Windows target is not clean at $commit" >&2
  echo "      ${exit_line}, ${errors} diagnostic line(s)" >&2
fi
echo "      full log: ${HOST}:${remote_log_win}" >&2
exit 1
