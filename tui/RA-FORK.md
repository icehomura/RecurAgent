# RecurAgent fork notes

This crate is the **RecurAgent terminal client**. It was imported from
`https://github.com/icehomura/ra-tui` (`main`, 2026-09-30, 395 tracked files, Apache-2.0) into
`tui/` so that the RecurAgent kernel and its terminal UI live in one repo and ship from one tree.

## What changed on import

| Change | Why |
|---|---|
| `tui/` is its **own cargo workspace** (`[workspace]` in `tui/Cargo.toml`), excluded from the kernel workspace via `exclude = ["tui"]` in the root `Cargo.toml` | the client has its own release train, `dist-workspace.toml`, packaging, `build.rs`, locales and lockfile; folding it into the kernel workspace would merge two release cadences |
| package and binary renamed `ra-tui` → `ra-tui` | RecurAgent naming |
| `ra-core` (git dep pinned to `icehomura/ra@f4a31d9a`) → `ra-core = { path = "../crates/ra-core" }` | builds against this repo's kernel; the pin is kept in a comment as the protocol revision the client was validated against |
| 24 `E0063 missing field` initializers widened with `None` / `Vec::new()` | the kernel gained serialised-protocol fields between the pin (2026-09-24) and this tree (2026-10-03). Values are behaviour-preserving defaults, *not* feature wiring |

## Architecture decision

The kernel (`ra`, root workspace) owns the agent loop, tools, sandbox, sessions and the UI Protocol
server (`ra serve`). The TUI is a **protocol client**: it attaches to a running server over
WebSocket, or spawns `ra serve --stdio` as a child (`src/backend_ensure.rs`). This mirrors upstream
and is why the TUI can be rebuilt without touching the kernel.

## Follow-ups

1. ~~**Backend identity**~~ — **done.** `src/backend_ensure.rs` resolves **`ra`**/`ra.exe` in order:
   (a) a sibling of the running TUI binary, (b) `ra` on `PATH`, (c) the install dir `~/.ra/bin/ra`
   (`$RA_PREFIX`, with the legacy `$ra_PREFIX` env fallback). `DEFAULT_STDIO_COMMAND` is
   `ra serve --stdio --solo`; the version parser reads the leading `X.Y.Z` from `ra 0.1.0 (…)`; and no
   upstream install is attempted — a missing backend errors with `cargo build --bin ra` /
   `--stdio-command` guidance. The upstream installer/download helpers were **deleted**. The child-PATH
   prepend is derived from the *chosen* resolution (`OnPath` ⇒ prepend nothing, `AtPath(p)` ⇒
   `p.parent()`), so an outdated sibling that lost to a `PATH` `ra` can never be prepended back into the
   launch; on Windows, if the resolved binary's stem differs from the command token, only that token is
   rewritten (never an embedded path, which `cmd /C` mangles). `doctor` reports which candidate resolves
   and scans the same set (sibling, `PATH`, `~/.ra/bin`).
2. **Brand/env sweep** — `RA_TUI_*` / `RA_*` env vars → `RA_TUI_*` / `RA_*` with dual-read
   fallback (**env vars only**). State paths are **new spelling only** — `~/.ra`, `~/.config/ra-tui`,
   `.ra-workspace.toml`; legacy locations (`.ra`, `.config/ra-tui`) are neither read, migrated nor
   cleared. User-visible strings (`ra-tui`, `Ra`) → `ra`/`ra`.
3. ~~**Protocol opt-ins**~~ — **done.** The client now uses the three fields the kernel gained after the
   imported pin: (a) `SessionOpenParams.client_commands` is populated from the client's own slash-command
   registry (`menu::registry::client_command_names()`), so the kernel can list the client-handled commands
   in the session prompt; (b) a person-started turn (composer submit, startup `--prompt`) carries
   `TurnStartParams.origin = Some(TurnOrigin { kind: Person, label: None })` — the shape the kernel's own
   host client uses (`crates/ra-cli/src/api/ui_protocol_transport.rs`); reconnect re-issues are not
   stamped; (c) rehydrated `HydratedMessage.tool_calls`/`tool_call_id`/`tool_name` are mapped into the
   message model and rebuilt as the same activity chips the live stream produces, so a resumed session
   keeps its tool-call rows.
4. **Smoke test** the interactive path against a locally built `ra` (`ra serve --stdio` attach, plus
   the WS path against `ra serve`).
