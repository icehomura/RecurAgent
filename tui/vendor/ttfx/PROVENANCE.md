# Provenance

Vendored copy of the `ttfx` crate (terminal text-effects engine), used by the
startup splash (`src/splash.rs`, spec `specs/task-startup-splash.spec`).

- **Source:** https://github.com/omacom-io/ttfx
- **Revision:** `6e24dac78e3011d89bd7ff24d1ad91dd89e11d8a` (crate version 0.3.1)
- **License:** MIT — see `LICENSE`; upstream porting credits in `NOTICE`
- **Copied:** 2026-10-05, by the no-third-party-git-dependency pass. The TUI
  previously took this exact revision as a git dependency
  (`ttfx = { git = "…/ttfx", rev = "6e24dac…" }`).

Every file below is byte-identical to that revision. Only these paths were
taken: `Cargo.toml`, `src/`, `tests/` (with `tests/fixtures/`), `README.md`,
`plan.md`, `docs/ordering-inventory.md`, `LICENSE`, `NOTICE`.

Left behind deliberately: upstream's `docs/effects/*.gif` (5.3 MB of README
animation previews), `tools/` (Python golden/parity generators), `bin/`,
`packaging/`, `.github/` and the upstream `Cargo.lock` — none is needed to
build or test the crate. `plan.md` and `docs/ordering-inventory.md` are kept
because module doc comments cite them.

The manifest needed no edits: it declares no `workspace = true` inheritance,
no path dependencies and no sibling crates — only the registry crates `clap`,
`clap_complete` and `terminal_size`, at the versions it pins upstream.

`tui/Cargo.toml` excludes this directory from the TUI workspace so the drop
stays a standalone package (its profiles/lints are its own) while still
resolving as a path dependency.

To refresh this copy, clone the URL at the revision above and re-copy those
paths.
