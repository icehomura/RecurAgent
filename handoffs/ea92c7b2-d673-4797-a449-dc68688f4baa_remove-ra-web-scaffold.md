# Handoff — remove the `ra web` scaffold (incomplete)

**Envelope id:** `ea92c7b2-d673-4797-a449-dc68688f4baa`
**Slug:** `remove-ra-web-scaffold`
**Date:** 2026-10-01 (local +0800)
**Agent:** RecurAgent session, iteration-budget handoff at 40/50 tool calls.

---

## 1. What the task was

Operator reported: `ra web` prints

```
Recur Agent Web Remote server listening on loopback:8080 (view_only=false)
Web client interface: http://127.0.0.1:8080
Pairing token: <redacted>
```

and then `127.0.0.1:8080` returns `ERR_CONNECTION_REFUSED`.

**Diagnosis (confirmed):** the command never bound a socket. `src/main.rs`
constructed an in-memory `WebRemoteManager`, issued a token, printed three
hardcoded `println!` lines, and returned `Ok(())` — the process exited. There
was no `TcpListener`, no WebSocket upgrade, no HTTP server anywhere:
`src/web_remote.rs` was a pure in-memory state model (token FSM, client
capacity, audit log) plus an **unreferenced** `EMBEDDED_WEB_CLIENT_HTML`
string. The "WASM client" was aspirational — no `wasm-bindgen`/`web-sys`, no
`.wasm` artifact, no `ftui-web` crate; only the module doc comment and a CSP
`'wasm-unsafe-eval'` token mentioned it.

**Provenance:** upstream code, not fork-introduced.
`src/web_remote.rs` exists in `upstream/main`; commits `4278426b2`
(bd-cv653.10.1) and `8e39577d0` (bd-cv653.10.2) are ancestors of
`upstream/main`, and upstream's `src/main.rs` Web arm has the identical
no-bind shape. The fork's only prior change was an 18-line mechanical
`pi.web.frame.v1` → `ra.web.frame.v1` rename.

**Operator decision:** option **A — 彻底移除** (remove the whole surface),
selected in-session. Per AGENTS.md Rule 1 the five file deletions were
restated verbatim and explicitly confirmed (`确认执行 A+B`).

---

## 2. What IS done

All removals are **present at HEAD and pushed** (`main...origin/main` clean).

### 2a. Deletions — LANDED, but in the *wrong* commit (see §4)

| File | Lines |
|---|---|
| `src/web_remote.rs` | 657 |
| `tests/web_remote.rs` | 195 |
| `tests/web_security.rs` | 111 |
| `docs/tools/web_remote.md` | 32 |
| `docs/security/web-access-threat-model.md` | 70 |

Verified absent at HEAD (`git cat-file -e HEAD:<path>` → absent) and absent
on disk. Pre-deletion blob hashes (for recovery, `git cat-file blob <hash>`):

```
src/web_remote.rs                        70d1eeeb18eba973f528aa689dd7cecc413d2171
tests/web_remote.rs                      769c2a3bab29c71537c482333cb6e3ac193649c7
tests/web_security.rs                    0b6aad36816876d8a1573b78c9b7be5b88ebb534
docs/tools/web_remote.md                 302706cd90624e0801e0f956e8d54c685f8dcac8
docs/security/web-access-threat-model.md 9404cd36498a9ac76d357fe90ca440d3c87565a9
```

### 2b. Wiring removal — LANDED in commit `be68d3c2d` (mine, correct)

```
chore(web): remove the unimplemented `ra web` scaffold
docs/TEST_COVERAGE_MATRIX.md        |  1 -
docs/settings.md                    |  7 -------
docs/traceability_matrix.json       |  8 --------
src/cli.rs                          | 17 -----------------
src/lib.rs                          |  1 -
src/main.rs                         | 28 ----------------------------
tests/conformance/fixture_runner.rs | 12 ------------
tests/suite_classification.toml     |  2 --
```

Specifically:
- `src/cli.rs` — dropped `"web"` from `ROOT_SUBCOMMANDS`, dropped the
  `Web { port, bind, view_only, max_viewers }` clap variant.
- `src/main.rs` — dropped the `cli::Commands::Web` match arm.
- `src/lib.rs` — dropped `pub mod web_remote;`.
- `tests/conformance/fixture_runner.rs` — dropped the `Commands::Web`
  parse-fixture arm (found late; `git grep` for `Commands::Web`, not
  `web_remote`).
- `tests/suite_classification.toml` — dropped `"web_remote"` + `"web_security"`.
- `docs/TEST_COVERAGE_MATRIX.md`, `docs/settings.md` (the "Web Remote & Collab"
  section incl. `web.port`/`web.bind_mode`/`web.view_only`/`web.max_viewers`),
  `docs/traceability_matrix.json` (2 entries).

### 2c. Gates run and passing

- `python3 scripts/check_module_reachability.py` →
  `157 declared, 154 reachable, 3 allowlisted, 0 unreachable` (exit 0).
- `python3 scripts/check_traceability_matrix.py` →
  `TRACEABILITY CHECK PASSED ... 100.00%` (exit 0). `traceability_matrix.json`
  re-validated as JSON.
- Suite classification: no test file on disk unlisted; deleted entries removed.
- `cargo check --locked --all-targets --keep-going --message-format short`:
  **lib + bin compile**; `tests/conformance/fixture_runner.rs` not in the error
  set. See §3 for the three *pre-existing, unrelated* failures.

**This is NOT a DSR-attributed result.** `dsr` is absent in this environment
(Windows/git-bash; `dsr`, `br`, `bv`, `am`, `ubs` all absent from PATH). The
cargo run was diagnostic only, per AGENTS.md "Enumerate whole-tree breakage in
one pass".

---

## 3. Pre-existing failures NOT caused by this change

`cargo check --all-targets` reports three errors, none in a touched file and
none mentioning the web surface — they are other agents' in-flight work:

```
tests/ext_proptest.rs:120:15   E0004 non-exhaustive patterns: HostcallKind::Fs { .. }
tests/handoff_generator.rs:269 E0369 `==` cannot be applied to Result<usize, io::Error>
examples/dag_bench.rs:209      E0716 temporary value dropped while borrowed
```

**Do not fix these as part of this work** — they belong to the agents editing
`src/extensions_js.rs` / handoff tooling / benches. If they persist, file
beads; do not sweep.

---

## 4. THE OPEN DEFECT — commit attribution is split (needs the next agent)

The five deletions did **not** land in my commit. A concurrent automated
committer swept my staged `git rm` into an unrelated commit:

```
commit cdb977cce9e8dd2eb910d9de76c06af714c1510a
Author: ci <example@example.com>
Date:   Thu Oct 1 22:49:37 2026 +0800
Subject: feat(i18n): localize the /model selector overlay (slice 2)

 docs/security/web-access-threat-model.md |  70 ----
 docs/tools/web_remote.md                 |  32 --
 locales/messages.yml                     |  82 ++++
 scripts/check_i18n_keys.py               |  73 +++-
 src/interactive/model_selector_ui.rs     |  97 +++--
 src/web_remote.rs                        | 657 -------------------------------
 tests/web_remote.rs                      | 195 ---------
 tests/web_security.rs                    | 111 -----
```

Timeline: my `git rm` executed `2026-10-01T14:48:26Z`; that commit is stamped
`22:49:37 +0800` (= `14:49:37Z`), one minute later. This is precisely the
failure mode AGENTS.md documents under "Pre-Commit Guard": an agent/sweeper
committed another agent's unverified in-flight work (cf. `5d3eb35a`,
2026-09-02).

**Consequences:**
- The *tree* is correct — every removal is present, compiles, and is pushed.
- The *history* is misleading: `cdb977cce`'s message says nothing about
  removing `ra web`, so anyone bisecting or auditing the removal will not find
  it, and the i18n slice appears to have deleted 1065 lines of web code.
- The agent-mail guard either was not consulted or did not flag my staged
  deletions (I ran `git rm` with no `AGENT_NAME` env var set, and `am`/`br`
  are absent from PATH here, so reservations could not be checked at all).

**Options for the next agent (do NOT rewrite pushed history without the
operator's explicit written approval — Rule 1 / irreversible-actions):**
1. Leave history as-is and record the split in a bead so a future audit is not
   misled. *(Lowest risk; recommended unless the operator says otherwise.)*
2. Operator-approved `git commit --amend`/rebase is **not** viable here
   because `cdb977cce` is already pushed and interleaved with other agents'
   commits. A corrective, forward-only note is safer than a rewrite.

**Next action:** ask the operator whether to (a) file a bead recording the
sweep + attribution split, and (b) whether the agent-mail guard's failure to
protect these files (`am` absent from PATH → guard cannot see reservations, or
fails open) should itself be a bead.

---

## 5. What remains

- [ ] **Decide the attribution-split disposition** (§4). Everything else is
      landed.
- [ ] **Operator bead decision (already asked, deferred):** the operator chose
      "不动 bead，写进 commit" for upstream `bd-cv653.10.1` / `.10.2` — i.e.
      leave upstream's closed records alone and record the removal in the commit
      message. That is done in `be68d3c2d`'s body. `br`/`bv` are absent here, so
      no bead was created; if a fork bead is later wanted it must be created
      when `br` is available.
- [ ] **Docs consistency spot-checks** (not yet done, low risk): grep `docs/`
      for any remaining prose describing `ra web`, and check
      `docs/evidence/module-reachability-sweep-runpack.json` — deliberately
      **left untouched** as a historical run record (it still lists
      `"web_remote"`). If the freshness/closeout gates
      (`scripts/check_closeout_gate_freshness.py`,
      `scripts/check_swarm_runpack_freshness.py`) complain, regenerate rather
      than hand-edit.
- [ ] **Full DSR quality run** when `dsr` is available — this removal has no
      DSR-attributed result. `.beads/` still references the removed module in
      `bd-33df9`'s prose ("Contrast src/web_remote.rs, which src/main.rs does
      reference — that one is genuinely wired"), which is now stale; that is
      upstream's closed bead text and was intentionally not edited.
- [ ] **`tests/ext_conformance/`** contains vendored third-party artifacts with
      unrelated `"web"` strings — ignore.

---

## 6. Next agent's starting position

Repo: `C:\Users\icehomura\workspace\rust\pi_agent_rust\RecurAgent`
Branch `main`, in sync with `origin/main`. Working tree has other agents'
uncommitted edits (`README.md`, `src/tools.rs`, `src/agent.rs`,
`src/ptc_bridge.rs`, `locales/messages.yml`, `scripts/check_i18n_keys.py`, …)
plus untracked `handoffs/*.md`. **Do not stash, revert, or sweep any of it.**

Quick re-verification (should all pass):

```bash
git log --oneline -3 --diff-filter=D -- src/web_remote.rs   # -> cdb977cce
git cat-file -e HEAD:src/web_remote.rs                      # -> absent
git grep -nE 'Commands::Web|web_remote|WebRemote' -- src tests benches examples  # -> clean
python3 scripts/check_module_reachability.py                # -> 0 unreachable
python3 scripts/check_traceability_matrix.py                # -> PASS
```

The one open question is §4 (attribution split) plus the deferred bead decision
in §5. Nothing in the code is half-done: this was a complete, verified removal
whose *commit attribution* was damaged by a concurrent sweeper.

---

## 7. Follow-on (same session): `br` installed; bead triage — DECISION PENDING

Operator decided to **KEEP** `.beads/` (not delete) but wants `ra doctor`'s bead
findings to stay PASS. `br` was absent, so it was installed from the verified
upstream release:

- Source: `https://github.com/Dicklesworthstone/beads_rust/releases/download/v0.7.4/br-0.7.4-windows_amd64.zip`
- Published sha256 `ea22cafa5c284f4840eaa13affbdabf4b1ec03e8f708114d2ce4ff6a8f85ba86`
  verified against the download: **MATCH**.
- Installed to `~/.cargo/bin/br.exe` (md5 `7944396ef4f88762681732cafa35a641`,
  `br 0.7.4`). A parallel `cargo install --git …` background job did NOT
  overwrite it (md5 unchanged) and produced no log.
- Staging leftovers, outside the repo: `C:\tmp\brstage\` (br.exe + LICENSE +
  README.md) and the zip in the git-bash temp dir. Safe to leave; remove only
  with explicit permission (AGENTS.md Rule 1).

Verified `br` reads this repo **read-only** without disturbing it: `br info`
left `.beads/issues.jsonl` md5 `5aeed2abff148ac4d0d4bf86db44abfe` unchanged and
`.beads/` git-clean (only gitignored `*.lock` files appeared, and no new
tracked file). `br show bd-soi5l` parses the hand-appended record correctly, and
`br list --status=in_progress` returns exactly the 8 stale beads.

### The 8 stale `in_progress` beads (all upstream-authored, 227–515h old)

| id | P | type | title |
|---|---|---|---|
| `bd-kgkrq` | P0 | bug | read tool returns empty content on Windows (GH #182) |
| `bd-tool-call-throughput-canonical-o3ubk` | P0 | task | produce pijs_workload data |
| `bd-gate1-fmt-chronically-red-aqxxe` | P1 | bug | Gate 1 `cargo fmt --check` chronically red |
| `bd-o6hte` | P1 | bug | quality gate builds only host target; Windows regression shipped undetected |
| `bd-m7im4` | P1 | task | add a Windows lane to the DSR quality recipe |
| `bd-2vmu6` | P1 | bug | close auto-retry event lifecycles across failover |
| `bd-3iodk` | P2 | bug | worktree_iso snapshot test fails on macOS (APFS EILSEQ) |
| `bd-0ngnq` | P2 | bug | rch workers have no usable .git |

### WHY THIS IS STILL OPEN

Mutating these changes UPSTREAM data. Explicit sign-off was requested and the
question **timed out**, so **NOTHING WAS CHANGED**.

Recommended action, if approved (non-destructive: releases the claim, loses no
work — the tool's own remediation for this WARN is `--status=open`):

```bash
br update bd-kgkrq bd-tool-call-throughput-canonical-o3ubk \
  bd-gate1-fmt-chronically-red-aqxxe bd-o6hte bd-m7im4 \
  bd-2vmu6 bd-3iodk bd-0ngnq --status open --actor RecurAgent
br sync --flush-only
git add .beads/issues.jsonl && git commit
ra doctor --only swarm        # confirm the WARNs clear
```

Do **not** `close` them: at least `bd-kgkrq` (P0), `bd-o6hte` and `bd-m7im4` are
real problems for this **Windows** fork and should be claimed, not erased.

Why releasing is a TRUE fix rather than gate-weakening: the current
`[FAIL] Live swarm admission decision: deny` is caused by
"ActiveAgents: 8 active vs 1 planned (8.00x)" — those 8 "active agents" are just
these stale claims, which doctor itself flags as *"8 stale in_progress bead(s)
may overstate live agent load"*. Releasing them corrects a false pressure signal.

Disclosed cost: the JSONL diff is a permanent small divergence from upstream
(8 lines will conflict on a future `git merge upstream/main`).

Do **not** hand-edit statuses in the JSONL to shortcut this: that forges state
transitions without br's FSM/audit trail. `br update` supports `--actor`,
`--transition-comment`, and an optimistic-concurrency guard (pass the
`updated_at` from `br show <id> --json`; a moved record writes nothing and exits
6) precisely to keep this honest. Note also that `br update --help` warns a
policy may REQUIRE `--transition-comment` for the transition.
