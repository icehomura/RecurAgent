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

## Follow-ups (not yet done)

1. **Backend identity** — `src/backend_ensure.rs` still looks for the upstream binary: `ra`/`ra.exe`
   on `PATH` and in `~/.ra/bin`, with auto-provisioning from
   `github.com/octos-org/octos/releases`, brew `octos-org/tap/octos` and npm `@octos-org/octos`.
   It must resolve **`ra`** (sibling of the TUI binary, then `PATH`) and stop offering upstream
   downloads.
2. **Brand/env sweep** — `OCTOSCODE_*` / `OCTOS_*` env vars → `RA_TUI_*` / `RA_*` with dual-read
   fallback; `~/.ra` and `.ra/` → `~/.ra` and `.ra/` with legacy fallback; user-visible
   strings (`octoscode`, `Octos`) → `ra`/`ra`.
3. **Protocol opt-ins** the alignment defaults skipped: `client_commands` on session open (slash-command
   discovery for the agent), `origin: TurnOriginKind::Person` on user turns, and rendering
   `HydratedMessage.tool_calls` from rehydrated history.
4. **Smoke test** the interactive path against a locally built `ra` (`ra serve --stdio` attach, plus
   the WS path against `ra serve`).
