# Scope: shared `ra-diagnostics` (ra-tui#182 "Sharing" phase)

**Goal:** the server (`ra-cli`) gets `ra doctor` / `ra update` by sharing the
diagnostics + update *logic* that ra-tui already implements — without
duplicating it and without bloating `ra-core`.

## Decision: a new `ra-diagnostics` crate (NOT feature-gated `ra-core`)

`ra-core` is the minimal base crate (serde/serde_json/chrono/uuid/eyre/tracing —
no network/runtime) and **all 6 other crates depend on it**. Putting reqwest +
axoupdater there, even behind a feature, makes the foundation own network/updater
concerns and risks workspace **feature-unification** leaking those deps into
unrelated crates. A dedicated crate is cleaner.

- **`ra-diagnostics`** (new, in the RecurAgent workspace; `default = []`): the shared
  report model, install-method detection, local checks, GitHub client (feature),
  update *planning*. Depends on `ra-core` for the protocol consts.
- **Exception → `ra-core::ui_protocol`:** the *pure* protocol-compatibility
  comparator (server caps/schema vs `UI_PROTOCOL_SCHEMA_VERSION` + `FEATURE_*`).
  It is protocol semantics and needs **no new deps**, so it belongs in core. The
  `Check`-producing *adapter* over it lives in `ra-diagnostics`.
- **Cross-repo:** ra-tui already git-deps `ra-core`; it adds a second git-dep
  on `ra-diagnostics` at the **same pinned rev**. Add CI asserting the two revs
  match and `cargo tree -d` shows no duplicate `ra-core`.

## The `ProductSpec` seam (what makes it product-agnostic)

Shared code must not hardcode `ra-tui` vs `ra`. Callers pass a `ProductSpec`:
binary name, package name, **current version (passed IN — never `CARGO_PKG_VERSION`
of the shared crate)**, GitHub repo, token env var, brew formula, npm package, cargo
install cmd, cargo-dist app name, installer URL, **asset selector** (`ra-tui-*`
vs `ra-bundle-*`).

## Split

**Share in `ra-diagnostics`:**
- `CheckStatus` / `Check` / `Report` — counts, exit-code policy, `--json`, text render.
- `InstallMethod` + pure `classify_path` + live `detect(&ProductSpec)`; PATH/shadow
  detection; package-manager upgrade advice (incl. the #189 npm-shim handling).
- Semver parse/compare + `update --check` decision types + **`UpdatePlan`**
  (up-to-date | update-available | defer-to-pkg-mgr | self-update-allowed).
- GitHub reachability + latest-release client + asset selection (behind a `github`
  feature → reqwest).
- Generic local checks: current-exe, on-PATH, shadow, data/config-dir writability,
  terminal (TERM/terminfo/locale/CJK/color).
- Protocol diagnostic **adapter** over the core comparator.

**Stays binary-specific:**
- `clap` subcommand wiring + process exit.
- TUI ratatui/onboarding rendering; `TUI_REQUIRED_FEATURES`.
- CLI/server checks: `serve` config, auth store, ports, keychain, skills, API/admin
  health, MCP, data dirs.
- stdio/endpoint config sourcing (a generic command *parser* can be shared; the
  meaning stays client-specific).
- Update **mutation**: asset names, the multi-binary bundle vs single binary, macOS
  codesign target + skill-cleanup lists.

## Network + self-update: share planning, NOT the engine (yet)

The two updaters are genuinely asymmetric:
- ra-tui: single binary, cargo-dist receipt → axoupdater.
- ra-cli: multi-binary **bundle** tarball + skills, rollback, codesign each.

So the shared layer produces an **`UpdatePlan`**; each binary runs its own driver.
`axoupdater` sits behind a narrow feature that **only ra-tui** enables. ra-cli
keeps its existing `crates/ra-cli/src/updater.rs` initially, adapted to consume
the shared release/planning code (and to become install-method-aware).

## Phasing

1. **First (no network, no mutation, highest value / lowest risk):** create
   `ra-diagnostics` (`default = []`); move report/check types, install-method
   classify+detect, PATH/shadow, semver helpers, and the protocol-compat adapter
   (pure comparator → `ra-core::ui_protocol`). Wire `ra doctor` to the shared
   report with **local checks only**.
2. **Next:** add the GitHub reachability/latest-release client behind `github`;
   wire `update --check` (plan only, no mutation) on both binaries.
3. **Defer:** the axoupdater self-update + folding ra-cli's bundle updater, until
   the `UpdatePlan` policy layer is stable.

## Traps (codex-flagged)

Feature unification pulling heavy deps into unrelated crates · duplicate
git-pinned `ra-core` (rev-match CI + `cargo tree -d`) · axoupdater leaking via
defaults (keep it non-default, ra-tui-only) · **`CARGO_PKG_VERSION` is wrong
inside a shared crate** — pass the version in via `ProductSpec` · stale cargo-dist
receipts · hard-coded CLI macOS-arm64 bundle asset names.
