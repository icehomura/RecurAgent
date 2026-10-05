# Vendored crates

`ra-research`'s `metasearch` feature compiles Rascript engines on the
bounded `rascript-core` runtime, whose VM is `makepad-script`. Those crates
used to be pulled in as pinned git dependencies; they now live here so the
build needs no external git fetch. The vendored code is the *same* revision
the git dependencies pinned, so engine behaviour is unchanged.

Vendored on **2026-10-05** by copying the upstream trees at the revisions
below, dropping `.git`, and pruning dev-only directories (`tests/`,
`benches/`, `examples/`, `fuzz/`, upstream sub-workspaces). Vendored
`Cargo.toml` files were adapted:

- sibling dependencies are `path =` deps inside this tree (flattened);
- upstream `[workspace.package]` inheritance and `[lints] workspace = true`
  are inlined so the crates are self-contained;
- dev-only targets, dev-dependencies and nested `[workspace]`/`[profile]`
  sections are dropped.

| vendored dir | upstream repo | rev | upstream path | license | vendored |
| --- | --- | --- | --- | --- | --- |
| `rascript-core/` | https://github.com/icehomura/Rascript | `e152f8bc1fd7dae1ce51993cab1c5fcb0fdc8b23` (2026-09-26) | `crates/rascript-core` | Apache-2.0 | 2026-10-05 |
| `rascript-ui-l0/` | https://github.com/icehomura/Rascript | `e152f8bc1fd7dae1ce51993cab1c5fcb0fdc8b23` (2026-09-26) | `crates/rascript-ui-l0` | Apache-2.0 | 2026-10-05 |
| `makepad-script/` | https://github.com/icehomura/makepad | `6e5898fecbe5931c16fb7b1e54357104dd54e825` (2026-09-18) | `platform/script` | MIT OR Apache-2.0 | 2026-10-05 |
| `makepad-script-derive/` | https://github.com/icehomura/makepad | `6e5898fecbe5931c16fb7b1e54357104dd54e825` | `platform/script/derive` | MIT OR Apache-2.0 | 2026-10-05 |
| `makepad-error-log/` | https://github.com/icehomura/makepad | `6e5898fecbe5931c16fb7b1e54357104dd54e825` | `libs/error_log` | MIT OR Apache-2.0 | 2026-10-05 |
| `makepad-stitch/` | https://github.com/icehomura/makepad | `6e5898fecbe5931c16fb7b1e54357104dd54e825` | `libs/stitch` | MIT OR Apache-2.0 | 2026-10-05 |
| `makepad-math/` | https://github.com/icehomura/makepad | `6e5898fecbe5931c16fb7b1e54357104dd54e825` | `libs/math` | MIT OR Apache-2.0 | 2026-10-05 |
| `makepad-live-id/` | https://github.com/icehomura/makepad | `6e5898fecbe5931c16fb7b1e54357104dd54e825` | `libs/live_id` | MIT OR Apache-2.0 | 2026-10-05 |
| `makepad-live-id-macros/` | https://github.com/icehomura/makepad | `6e5898fecbe5931c16fb7b1e54357104dd54e825` | `libs/live_id/id_macros` | MIT OR Apache-2.0 | 2026-10-05 |
| `makepad-micro-proc-macro/` | https://github.com/icehomura/makepad | `6e5898fecbe5931c16fb7b1e54357104dd54e825` | `libs/micro_proc_macro` | MIT OR Apache-2.0 | 2026-10-05 |
| `makepad-micro-serde/` | https://github.com/icehomura/makepad | `6e5898fecbe5931c16fb7b1e54357104dd54e825` | `libs/micro_serde` | MIT OR Apache-2.0 | 2026-10-05 |
| `makepad-micro-serde-derive/` | https://github.com/icehomura/makepad | `6e5898fecbe5931c16fb7b1e54357104dd54e825` | `libs/micro_serde/derive` | MIT OR Apache-2.0 | 2026-10-05 |
| `makepad-regex/` | https://github.com/icehomura/makepad | `6e5898fecbe5931c16fb7b1e54357104dd54e825` | `libs/regex` | MIT OR Apache-2.0 | 2026-10-05 |
| `makepad-html/` | https://github.com/icehomura/makepad | `6e5898fecbe5931c16fb7b1e54357104dd54e825` | `libs/html` | MIT OR Apache-2.0 | 2026-10-05 |
| `smallvec/` | https://github.com/icehomura/makepad (vendored there from crates.io `smallvec` 1.15.1) | `6e5898fecbe5931c16fb7b1e54357104dd54e825` | `libs/smallvec` | MIT OR Apache-2.0 | 2026-10-05 |

`rascript-core` additionally keeps upstream's `tests/fixtures/` because
`src/lib.rs` embeds one fixture in a `#[cfg(test)]` test; all other test
sources were pruned. `makepad-stitch` keeps its `README.md`.

Licenses live in `licenses/`:

- `makepad-LICENSE-MIT`, `makepad-LICENSE-APACHE` — from the makepad repo root
  (covers all `makepad-*` crates above).
- `rascript-LICENSE-APACHE`, `rascript-NOTICE` — from the Rascript repo
  root (covers `rascript-core`, `rascript-ui-l0`).
- `smallvec/LICENSE-MIT`, `smallvec/LICENSE-APACHE` — copied with the crate
  (as shipped in the makepad tree).

This tree is its own cargo workspace (`vendor/Cargo.toml`); the root workspace
depends on it only through path deps from `crates/ra-research/Cargo.toml`.

## Updating a vendored crate

Clone the upstream repo at the new revision, copy the crate's `src/` (plus
`build.rs`/`README.md` where present), apply the manifest adaptations listed
above, and update the table. Keep the revisions in lock-step with
`crates/ra-research/Cargo.toml`'s comment.
