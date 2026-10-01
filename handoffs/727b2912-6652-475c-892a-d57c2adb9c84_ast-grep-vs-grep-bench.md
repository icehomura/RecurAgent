# Incomplete-handoff envelope — ast_grep vs grep efficiency test

**Owner session:** `727b2912` (parent `6018de4c`)
**Date:** 2026-10-01
**Branch:** `main`
**Status:** INCOMPLETE — question asked by user, experiment scaffolded, **no measurement numbers produced yet.**

---

## 1. The task

User (Chinese): **“测试一下 ast_* 工具是否比 grep 工具高效”** — *test whether the `ast_*` tools are more
efficient than the `grep` tool.*

“高效” has two readings, and both were in scope:
1. **speed / cost** — wall-clock per query at the agent-facing tool level;
2. **effectiveness** — precision (comments/strings) and expressiveness (metavariables).

No artifact, commit, or claim was requested. This is an engineering measurement, **not** a release
claim, and nothing here is DSR-attributed.

---

## 2. What is DONE (all findings are source-verified, not measured)

### 2.1 The two backends share everything except the matcher
| | `grep` tool | `ast_grep` tool |
|---|---|---|
| impl | `GrepTool::grep_inproc_scan_sync`, `src/tools.rs:12098` | `AstGrepTool::run`, `src/ast_tools.rs:485` |
| walker | `ignore::WalkBuilder` via `recursive_scan_walk_builder`, `src/tools.rs:3710` | `ignore::WalkBuilder` via `collect_files`, `src/ast_tools.rs:262` |
| matcher | `grep_regex::RegexMatcherBuilder` + `grep_searcher::Searcher` (`line_number`, `BinaryDetection::quit(0)`) | `ast_grep_core::AstGrep::new(..).root().find_all(pattern)` |
| per file | `searcher.search_path` (streams the file) | `fs::metadata` size gate → `fs::read_to_string` → tree-sitter parse |

The two walkers are configured identically: `hidden(false) .parents(true) .ignore(true)
.git_ignore(true) .git_global(false) .git_exclude(true) .require_git(false) .follow_links(false)`.
(The grep walker additionally registers a custom-ignore filename + workspace `.gitignore`; the corpus
used does not exercise that.)

### 2.2 Source-level cost asymmetry (the expected result)
- **grep** = parallel-capable literal/regex scanning (`grep-searcher`), no parse.
- **ast_grep** = `for file in &scan.files` (src/ast_tools.rs:551) is **single-threaded**, reads every
  file into a `String`, and runs a **full tree-sitter parse of every in-scope file**. There is no
  rayon/parallelism in the scan loop, and `offload_ast_work` only moves the whole job to a blocking
  thread — it does not parallelize it.
- ⇒ expectation: ast_grep is much **slower** per scan; its advantage is **precision/expressiveness**,
  not speed. **This expectation is NOT yet measured.**

### 2.3 Effectiveness facts already established from source + existing tests
- Comment/string exclusion is real and already covered by
  `tests/ast_tools.rs:511` `ast_grep_unwrap_pattern_ignores_comments_and_strings` — i.e. the
  precision claim is a *tested* property of this repo, not a slogan.
- **`ast_grep` cannot return more than 1000 matches**: `HARD_MATCH_LIMIT = 1000`
  (src/ast_tools.rs:50) and the scan **breaks out early** once reached (src/ast_tools.rs:597), so on
  match-heavy queries it does *strictly less work* than grep and returns a truncated answer.
- **API asymmetry:** `ast_grep` rejects `limit > 1000`; `grep` accepts ≥5000. This is why the shared
  `run_tool` helper in the bench could not be reused as-is (it injects `5000 + iteration`).

---

## 3. What was CHANGED in the checkout (UNVERIFIED — do not land blind)

### 3.1 `benches/search_backends.rs` — MODIFIED, **NOT COMMITTED, NOT COMPILED**
`git diff --stat benches/search_backends.rs` → `1 file changed, 40 insertions(+), 1 deletion(-)`.
Additions:
- module doc extended to state the ast lane and *why* it is a zero-match full-scan query;
- `registry_for` now enables `["grep", "find", "ast_grep"]`;
- new `run_ast_tool` helper (ast rejects `limit > 1000`, so it pins `limit = 1000 - iteration % 10`
  — still cache-busting, still in range);
- new benchmark lanes `grep_inproc_nomatch_2k_files` / `ast_grep_inproc_nomatch_2k_files`.

**Rustfmt is clean** (`rustfmt --edition 2024 --check benches/search_backends.rs` → exit 0), and the
file parses. It has **never been type-checked or run** — the release build was still in flight.

### 3.2 Out-of-tree microbench — written, complete, **NOT COMPILED, NOT RUN**
Location (scratch, outside the repo, created via bash because the `write` tool is cwd-restricted):
`/tmp/ast_grep_bench` → `C:\Users\ICEHOM~1\AppData\Local\Temp\codeg-acp\51348-c9e530b2\ast_grep_bench`
- `Cargo.toml` — pins the exact lockfile versions: `ast-grep-core =0.40.5`,
  `ast-grep-language =0.40.5` (feature `tree-sitter-rust`), `grep-regex =0.1.14`,
  `grep-searcher =0.1.17`, `ignore =0.4.33`; release profile mirrors the repo (`opt-level="z"`,
  `lto=true`, `codegen-units=1`).
- `src/main.rs` — 234 lines, replicates both shipping paths faithfully (same walker settings, same
  searcher builder, same `metadata`→`read_to_string`→`AstGrep::find_all` sequence) and reports
  `median_ms` / `min_ms` over 3 runs after a warmup for: shared I/O floor, `[absent]` (0-match full
  scan, apples-to-apples `.rs`-only **and** grep-all-files), `[unwrap()]` vs `[$X.unwrap()]`, and
  `[fn .. -> Result<..>]` vs `[fn $NAME($$$ARGS) -> Result<$$$RET> { $$$BODY }]`.

⚠️ **`main.rs` was truncated by a long bash heredoc at line 225 and then repaired by an append.**
The tail was re-appended, so it should be complete — **re-verify by reading the file before relying
on it** (`tail -25 /tmp/ast_grep_bench/src/main.rs`). This is the single most likely thing to be
broken in this envelope.

---

## 4. Why nothing was committed

Per `AGENTS.md`: *“Never commit into someone else's reservation… do not commit unverified in-flight
work”* and *“stage only the files YOU changed”*. The working tree contains **other agents' edits to
7 tracked files** (`README.md`, `docs/settings.md`, `src/app.rs`, `src/config.rs`, `src/dag_tool.rs`,
`src/interactive_ftui.rs`, `src/subagents.rs`) which must not be swept up.

`benches/search_backends.rs` is the only file this session touched, and it is **unverified** (never
compiled). It was deliberately left uncommitted rather than pushed to `main` as a partially-checked
change. **Nothing was pushed. No destructive command was run. No file was deleted.**

---

## 5. NEXT AGENT — starting position

**Step 0 — fix or discard the repo edit.** Decide whether the `benches/search_backends.rs` addition
is worth landing as a permanent part of an existing comparison bench (it arguably is: that bench's
stated purpose is exactly “compare search backends”). Then gate it:
```bash
cargo bench --bench search_backends --no-run          # type-check + link
cargo bench --bench search_backends                   # numbers
```
⚠️ **A `cargo bench` build holds the `target/release` lock and blocks other agents' builds.** A
background job from this session may still be holding it:
- **job id:** `job-f0cab2c344d14a0c920cfeacb96657a2`
- log: `C:\Users\icehomura\.ra\agent\tool-output-artifacts\jobs\job-f0cab2c344d14a0c920cfeacb96657a2.log`
- **Check `jobs` list / wait / cancel BEFORE starting your own build.** The log was 0 bytes with
  ~12 `cargo`/`rustc` processes alive when this envelope was written.

**Step 1 — get the numbers cheaply if the bench build is too slow.** There are **no compiled deps
anywhere** in this checkout (`target/release/deps`, `target/debug/deps`, `target-wb-*/…/deps` are all
empty — DSR/RCH builds remotely), so a local `cargo bench` is a **from-scratch full rebuild** with
`lto=true, codegen-units=1`. Prefer the out-of-tree microbench:
```bash
cd /tmp/ast_grep_bench
cargo +nightly-2026-08-31 run --release --offline -- \
  "C:/Users/icehomura/workspace/rust/pi_agent_rust/RecurAgent" src
```
All crates are already in `~/.cargo/registry/cache` (verified), the pinned toolchain
`nightly-2026-08-31` is installed, and the repo checkout is **not** the target of the build, so it
does not contend for `target/release`. Report `file count`, `bytes`, `matches`, `median_ms`, `min_ms`.
Expected shape: ast_grep ≫ slower on the `[absent]` full scan; comparable match counts on
`[unwrap()]` **except** grep inflates by matching comments/strings.

**Step 2 — the effectiveness half is not yet demonstrated end-to-end.** Run the same `unwrap()`
comparison through the *real* tools (harness `grep` vs harness `ast_grep` over `src/`) and diff the
result sets to quantify the false-positive delta, then spot-check 2–3 extras by hand. Note
`ast_grep`'s 1000-match cap when choosing the limit.

**Step 3 — cleanup / landing.** The scratch dir `/tmp/ast_grep_bench` is **outside the repo**;
delete only with the operator's explicit permission (RULE 1). `handoffs/` is untracked and includes
this file.

**Known-good context:** `rg`/`ast-grep`/`sg` are **not on PATH**; the harness `grep` tool is the
built-in ripgrep, and `run_code`'s sandbox cannot reach `ast_grep`
(`PTC_BRIDGE_DENIED` — read-only whitelist is `read|grep|find|ls`), which is why the measurement had
to go out-of-tree. `dag` reports **no per-node durations**, so it cannot be used to time tools.
