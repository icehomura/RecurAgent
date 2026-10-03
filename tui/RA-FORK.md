# ra fork notes

This crate is the **ra terminal client**. It was imported from
`https://github.com/octos-org/octoscode` (`main`, 2026-09-30, 395 tracked files, Apache-2.0) into
`tui/` so that the ra kernel and its terminal UI live in one repo and ship from one tree.

## What changed on import

| Change | Why |
|---|---|
| `tui/` is its **own cargo workspace** (`[workspace]` in `tui/Cargo.toml`), excluded from the kernel workspace via `exclude = ["tui"]` in the root `Cargo.toml` | the client has its own release train, `dist-workspace.toml`, packaging, `build.rs`, locales and lockfile; folding it into the kernel workspace would merge two release cadences |
| package and binary renamed `octoscode` → `ra-tui` | ra naming |
| `octos-core` (git dep pinned to `octos-org/octos@f4a31d9a`) → `ra-core = { path = "../crates/ra-core" }` | builds against this repo's kernel; the pin is kept in a comment as the protocol revision the client was validated against |
| 24 `E0063 missing field` initializers widened with `None` / `Vec::new()` | the kernel gained serialised-protocol fields between the pin (2026-09-24) and this tree (2026-10-03). Values are behaviour-preserving defaults, *not* feature wiring |

## Architecture decision

The kernel (`ra`, root workspace) owns the agent loop, tools, sandbox, sessions and the UI Protocol
server (`ra serve`). The TUI is a **protocol client**: it attaches to a running server over
WebSocket, or spawns `ra serve --stdio` as a child (`src/backend_ensure.rs`). This mirrors upstream
and is why the TUI can be rebuilt without touching the kernel.

## Follow-ups

1. ~~**Backend identity**~~ — **done.** `src/backend_ensure.rs` resolves **`ra`**/`ra.exe` in order:
   (a) a sibling of the running TUI binary, (b) `ra` on `PATH`, (c) the install dir `~/.ra/bin/ra`
   (`$RA_PREFIX`), (d) a legacy `~/.ra/bin/ra` (protocol-compatible). `DEFAULT_STDIO_COMMAND` is
   `ra serve --stdio --solo`; the version parser reads the leading `X.Y.Z` from
   `ra 2.0.3-rc.13 (…)`; and no upstream install is attempted — a missing backend errors with
   `cargo build --bin ra` / `--stdio-command` guidance. The upstream installer/download helpers were
   **deleted** (nothing is auto-installed). The child-PATH prepend is derived from the *chosen*
   resolution (`OnPath` ⇒ prepend nothing, `AtPath(p)` ⇒ `p.parent()`), so an outdated sibling that
   lost to a `PATH` `ra` can never be prepended back into the launch; on Windows a legacy-only backend
   is launched by rewriting just the program token (`ra` → `ra`) and prepending its dir, since
   `cmd /C` can't take an embedded path. `doctor` reports which candidate resolves and scans the same
   set (sibling, `PATH`, `~/.ra/bin`, legacy `~/.ra/bin`).
2. **Brand/env sweep** — `OCTOSCODE_*` / `OCTOS_*` env vars → `RA_TUI_*` / `RA_*` with dual-read
   fallback; `~/.ra` and `.ra/` → `~/.ra` and `.ra/` with legacy fallback; user-visible
   strings (`octoscode`, `Octos`) → `ra`/`ra`.
3. **Protocol opt-ins** the alignment defaults skipped: `client_commands` on session open (slash-command
   discovery for the agent), `origin: TurnOriginKind::Person` on user turns, and rendering
   `HydratedMessage.tool_calls` from rehydrated history.
4. **Smoke test** the interactive path against a locally built `ra` (`ra serve --stdio` attach, plus
   the WS path against `ra serve`).
