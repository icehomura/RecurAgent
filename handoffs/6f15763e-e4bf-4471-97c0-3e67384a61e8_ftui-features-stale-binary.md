# Handoff — ftui features "invisible" = stale Windows binary (INCOMPLETE)

**Status one-liner:** Both requested ftui features ARE in source and compile into a fresh `ra.exe`, but the user's running Windows PE (`D:\cargo-target\pi_agent\release\ra.exe`, built 2026‑10‑01 20:02) predates the commits; the release rebuild did not finish this session.

## User's report (verbatim intent)

1. Mouse text selection only works over part of the screen ("光标选择文字只能从上半页选择到下半页"); user asked for the selection region to follow the terminal size.
2. When scrolled up there should be a centered color block at the bottom row that scrolls back to the tail; user never sees it.

## Diagnosis (do NOT re-derive)

- Both features exist in **`src/interactive_ftui.rs`** (the default stack):
  - Selection hit-test now uses the last *rendered* frame, not the stale `term` default:
    `frame_area()` ~L3275, `body_rect()` ~L3284, `handle_mouse()` ~L3361; `rendered_size` / `rendered_body` set in `render_frame` ~L5921/L5928. Commits `846c0bb1f` (2026‑10‑01 23:22) and `a74bb52ba` (23:24).
  - Animated "scroll to bottom" badge: `scroll_hint_rect()` ~L3253, render ~L6014, click handling ~L3374, label `scroll_hint_label()` ~L6686. Commit `b1039e001` (23:15). i18n: `interactive_scroll_to_bottom` = "↓ scroll to bottom" / "↓ 回到底部".
- **Root cause is a stale binary**, not missing code:
  - The Windows executable actually run is `D:\cargo-target\pi_agent\release\ra.exe`, PE32+, built **2026‑10‑01 20:02** — before the feature commits.
  - Proof: that PE contains **0** occurrences of `interactive_scroll_to_bottom`, `interactive_copied_chars`, and `scroll to bottom`. A locally built artifact (`RecurAgent/target/release/ra.exe`) contains 3/3/1 — but that one is a **Linux ELF misnamed `.exe`** (`file` → ELF 64‑bit; embedded paths `/home/icehomura/.cargo/...`), so it is **not runnable on Windows**. Ignore it; the real Windows artifact lives under `D:\cargo-target\pi_agent\release\`.
  - `CARGO_TARGET_DIR=D:/cargo-target/pi_agent` in the agent shell; native Windows cargo/rustc is `nightly-2026-08-31-x86_64-pc-windows-msvc`.
  - **9 running `ra.exe` processes** all resolve to `D:\cargo-target\pi_agent\release\ra.exe` (verified via `Get-CimInstance Win32_Process`). One may be the session hosting the user. On Windows a running `.exe` cannot be overwritten (sharing violation) — this is why the user asked to rename / change the output name.

## What was done this session

- Renamed the locked stale PE **out of the way** (preserved, reversible):
  `D:\cargo-target\pi_agent\release\ra.exe` → `D:\cargo-target\pi_agent\release\ra-old-20261001.exe`.
  Running processes are unaffected (their mapping survives the rename).
- Started `cargo build --release --locked --bin ra` (background job `job-62bfcf9415224504afa2bba999121277`, cargo PID 29128, rustc child 54608).
  - It did **not** finish: after ~18 min it was still in the final single-CGU LTO compile (`[profile.release]` = `lto=true`, `codegen-units=1`, `opt-level="z"`), competing with other agents' `cargo test` runs.
  - The job was launched with `| tail -60`, so its log stayed 0 bytes until EOF (bad telemetry choice). Job killed at handoff. **No new `ra.exe` was produced.**
- **No source changes were made.** No commit is warranted. `git status` is full of *other* agents' in-flight edits (`src/agent.rs`, `src/config.rs`, `src/dag_tool.rs`, `src/dag_view.rs`, `src/error.rs`, `src/ptc_bridge.rs`, `src/subagents.rs`, `src/tools.rs`, `.beads/*`) — leave them alone, do not `git add -A`.

## Next agent — starting position

1. **Confirm whether the user's shell sets `CARGO_TARGET_DIR`.** If not, the canonical build is `RecurAgent/target/release/`; if yes, `D:\cargo-target\pi_agent\release\`. Make sure the rebuild targets the dir the user actually launches.
2. **Rebuild, with output redirected to a real log** (no `| tail`):
   ```bash
   cd "C:/Users/icehomura/workspace/rust/pi_agent_rust/RecurAgent"
   cargo build --release --locked --bin ra > /tmp/ra-build.log 2>&1
   ```
   `D:\...\release\ra.exe` is currently free (we renamed it), so the link can write there. If a *different* running copy blocks it, rename that one too.
   - **Faster fallback if LTO is too slow / rustc crashes** (see `handoffs/18cc6960-..._tui-i18n-rust-i18n.md` for a prior rustc monomorphization stack overflow):
     ```bash
     CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
       cargo build --release --locked --bin ra
     ```
3. **Verify the new PE carries the features:**
   ```bash
   grep -a -c interactive_scroll_to_bottom D:/cargo-target/pi_agent/release/ra.exe   # must be > 0 (stale = 0)
   ```
4. **Respect the user's request to name the output distinctly.** If they want a separate name, copy the fresh PE to `ra-new.exe` next to it. Decide with them whether `ra.exe` should be the new build (foolproof) or stay old with the new one at `ra-new.exe`.
5. **The user MUST exit and relaunch all 9 old `ra.exe` sessions** — a running process keeps the old image until restarted. Only then will they see:
   - the centered badge ("↓ 回到底部" / "↓ scroll to bottom") on the body's bottom row while scrolled up, clickable to jump to the tail; and
   - drag-selection spanning the full terminal height (hit-test against the rendered frame).
6. If the badge still does not appear after a fresh binary + restart, next checks: (a) `--no-mouse-capture` / `disableMouseCapture` / `RECUR_AGENT_NO_MOUSE_CAPTURE=1` (app never receives wheel events, so `scroll_from_tail` stays 0 — but PageUp/Shift+Up should still show the badge), (b) transcript fits one screen (`max_scroll_from_tail() == 0`), (c) the user is parked at the tail.

## Gotchas

- Windows cannot overwrite/replace a running `.exe`; rename it (allowed) or build to a new name.
- Do not `grep -r` over `$HOME/.config` — a huge embedded config dump will blow the tool output (cost this session a 54 MB artifact).
- `RecurAgent/target/release/ra.exe` is an ELF, not a Windows binary despite the extension.
- Suggested skills: none special; follow `AGENTS.md` (DSR is not installed on this Windows host — `command -v dsr` is empty).
