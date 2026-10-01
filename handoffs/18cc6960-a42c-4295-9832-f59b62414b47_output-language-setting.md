# Handoff — `output_language` setting (B) + TUI i18n workload assessment

Session: `18cc6960-a42c-4295-9832-f59b62414b47`
Slug: `output-language-setting`
Date: 2026-10-01
Status: **IMPLEMENTED, COMPILES (`cargo check --lib` green), TESTS NOT RUN.** Treat this as a starting position, not a finished change.

---

## 1. Why this work exists

User asked, in order:

1. Why are tool names / operation steps in English? → Answer: tool names and enum
   values are **protocol identifiers** (static tables, JSON Schema enums, ACP
   fields), and the repo has **no i18n layer at all**. Free-text values (dag node
   `name`, `reason`, todo text) *are* mine to choose.
2. Can the repo's output language be unified via settings/config? → **Yes**, but
   there was no such field. Chose option **B**: add a real `output_language`
   setting plumbed into the system prompt.
3. Also: assess the workload of doing full TUI i18n. → Assessed, see §5.

---

## 2. What is DONE (edits on disk, uncommitted at handoff time)

Three files touched, nothing else:

### `src/config.rs`
- **Field** added to `pub struct Config` (line ~187, between `ask_policy` and the
  `// Compaction` comment):
  `#[serde(alias = "outputLanguage")] pub output_language: Option<String>`
  with a doc comment stating the prose-only / identifiers-stay-English rule.
- **Merge arm** added in the global←project merge (line ~1146):
  `output_language: other.output_language.or(base.output_language),`
- **Accessor** added after `follow_up_queue_mode` (line ~1325):
  `pub fn output_language(&self) -> Option<&str>` — trims, and returns `None`
  for unset *or blank*, so no empty directive is ever emitted.
- **3 tests** added after `queue_mode_accessors_default_on_unknown`
  (before the `// ── thinking_budget accessor ──` banner):
  - `output_language_is_unset_by_default_and_blank_reads_as_unset`
  - `output_language_accepts_both_spellings_and_stays_visible_to_doctor`
  - `output_language_project_setting_overrides_global`

### `src/app.rs`
- **Two helpers** inserted immediately before `fn default_system_prompt`:
  - `fn output_language_directive(language: Option<&str>) -> Option<String>`
  - `fn language_display_name(language: &str) -> String`
- **Injection** in `pub fn build_system_prompt` just after the `skills_prompt`
  block and **before** the `Current date and time:` footer (line ~268):
  ```rust
  if let Some(directive) = output_language_directive(config.output_language()) {
      prompt.push_str("\n\n");
      prompt.push_str(&directive);
  }
  ```
  Placement is deliberate: **last block before the footer = highest recency**,
  so it outranks an `AGENTS.md` that implies another language.
- **4 tests** added before the `default_system_prompt_names_every_builtin_subagent_and_the_verify_loop`
  doc comment:
  - `output_language_directive_pins_prose_and_keeps_identifiers_english`
  - `output_language_directive_spells_out_common_tags_and_passes_the_rest_through`
  - `output_language_directive_is_absent_when_unset_or_blank`
  - `built_prompt_carries_the_language_directive_last` (end-to-end through
    `build_system_prompt`, using `cli::Cli::parse_from(["pi"])`, `test_mode=true`)

### `docs/settings.md`
- New `### Output language` section inserted before `### Message delivery (queue modes)`.

---

## 3. What remains — THE NEXT AGENT'S STARTING POSITION

1. **TESTS NOT YET RUN — that is the remaining gap.** `dsr` is **not installed** in
   this environment (`command -v dsr` → MISSING) and `br` is **not installed**
   either. So no DSR-attributed quality claim exists and no bead was created.
   **However `cargo check --locked --lib --message-format short` DID complete
   successfully** (`Finished dev profile [unoptimized + debuginfo] target(s) in
   1m 04s`), so the three touched files compile. The only warnings were
   pre-existing and unrelated: a `Cargo.toml` duplicate-target warning for
   `examples/pijs_workload.rs`, and a future-incompat note for `nix`/
   `proc-macro-error2`.
   Still to do: `cargo test --lib output_language` and
   `cargo test --lib language_display_name`, then a wider `--all-targets` pass.
2. **Lint.** Repo runs pedantic+nursery under `-D warnings`. Suspects in the new
   code: `match_like_matches_macro`, `if_then_some_else_none`, and the
   `&str → String` returns in `language_display_name`. Also confirm `clippy::doc_markdown`
   is happy with the backticked identifiers in the new doc comments.
3. **Then** run the authoritative gate: `dsr quality --tool recur_agent` (install
   dsr first, or record the hold and leave the bead open — do **not** cite a DSR
   result that was not produced by a DSR run).
4. **Bead:** `br` was unavailable, so **no bead exists**. Create one (e.g.
   `output_language` setting) and link this handoff, per the "outcome beads close
   on the outcome" rule.
5. **Commit + push.** At handoff the change was **left uncommitted on purpose**
   because it is unverified. Stage only these three paths —
   `src/config.rs`, `src/app.rs`, `docs/settings.md` — never `git add -A`.
   Other agents have unrelated modifications in flight in this checkout:
   `README.md`, `src/dag_tool.rs`, `src/interactive_ftui.rs`, `src/subagents.rs`.
   **Do not disturb those.**
6. **Agent Mail:** no file reservation was taken. If the guard is live, take an
   exclusive reservation on the three paths before editing, with a TTL longer
   than a gate run (`--ttl 14400`).
7. **Optional follow-up:** consider whether `output_language` should also reach
   the ACP system prompt (`src/acp.rs:1430 build_acp_system_prompt`) — it takes no
   `Config` today, so ACP sessions would not honor the setting yet. That is a
   deliberate scope cut, not an oversight.

---

## 4. Design decisions worth not re-litigating

- **Doctor needed no change.** `src/doctor.rs:570 is_known_config_key` delegates to
  `crate::config::recognises_setting_key`, which **asks serde** rather than using a
  hand-kept list (`src/config.rs:1976`). Adding the field + alias makes both
  spellings recognised automatically, and the existing test
  `every_key_config_accepts_is_known_to_doctor` (doctor.rs:11064) picks up the new
  key because the field has no `skip_serializing_if`. Verified by reading, not by
  running.
- **Aliases are load-bearing.** The repo's own history (doctor.rs:560-568) records
  that a hand-kept key list drifted to 67 of 108 spellings and made doctor accuse
  41 *correct* settings of being typos. Do not "simplify" this back to a list.
- **Identifiers stay English unconditionally.** Tool names, parameter names and
  enum values are matched by the dispatcher, the JSON Schemas, and the ACP /
  extension contract. The directive says so outright rather than leaving it to
  inference.
- **Unlisted languages pass through verbatim** (user's own casing preserved)
  instead of being rejected, keeping the setting open-ended.

---

## 5. TUI i18n workload assessment (the user's third question)

Measured, not estimated from vibes. **Method note: the first measurement was
garbage** — a `grep -E '[^\x00-\x7F]'` in this environment matched pure-ASCII
strings, reporting 8458 CJK literals. The real figure is ~43. Re-measured with
Python (`\p{Han}`) and with the repo's own Rust regex via `sdk.grep`.

| Metric | Value |
|--------|-------|
| UI-surface files | 30 (`interactive_ftui.rs` 16,539 lines; `interactive/agent.rs` 7,495; `interactive/commands.rs` 5,760; …) |
| UI-surface lines | ~61,800 |
| Lines containing an **English prose** string literal | ~2,500–3,400 (depending on scan depth) |
| **Unique** translatable strings | **~2,700** |
| …containing `{placeholder}` (need a format-aware catalog) | ~873 |
| …short chrome labels (≤24 chars: "Enter", "Esc", "Cancel") | ~827 |
| …plain sentences | ~1,037 |
| Lines containing a **CJK** string literal | **~43** |
| i18n crates in `Cargo.toml` | **none** |

**Verdict: the TUI is monolingual English, not partially localized.** The ~43 CJK
literals are incidental (`dag_view.rs` `START_LABEL`/`END_LABEL` = "开始"/"结束",
a few FTUI strings, 3 in `theme.rs`) — not a translation layer.

Workload shape, in ascending cost:

1. **Extracting ~2,700 strings into a catalog** — mechanical but large; ~873 need
   format-aware entries (`"… {n} more lines"`) so a naive `t!()` macro around the
   raw literal will not compile.
2. **Language negotiation** — none exists: no locale detection, no
   `Accept-Language`-equivalent, no setting. Would need a new mechanism (or reuse
   this session's `output_language`, which is *not* the same concern: prose
   language ≠ chrome language).
3. **The strings are inline at ~1,947 render sites** across ~20k lines, of which only
   ~380 live in named `const`s — and many of those 380 are protocol identifiers
   (`"allow-once"`, `"end_turn"`, `"ra.dag.scheduler.v1"`), i.e. **not translatable
   at all**. So centralizing first is a prerequisite, not optional.
4. **Width-aware layout.** `dag_view.rs` does integer column arithmetic with
   `unicode_width` and asserts centring; `interactive_ftui.rs` does box drawing.
   Translated strings change display width and will break layout/alignment tests.
   Chinese text is double-width — this is a real, non-trivial class of breakage.
5. **Test surface** — `src/interactive/tests.rs` alone holds ~102 English string
   literals used as assertions; every catalog change risks churn there.

**Recommendation:** do **not** attempt whole-TUI i18n. The honest decomposition is
(a) centralize first, (b) translate chrome only (menus, keybinding hints, errors),
(c) leave developer-facing diagnostics English. Note that per `AGENTS.md`, the
retired drop-in program explicitly does not gate on this, and tool *names* must
never be translated.

---

## 6. Session-limited caveats

- `dsr` MISSING, `br` MISSING → **no quality claim, no bead**.
- `cargo check --locked --lib` passed; **no test was run**, so the four new
  `app.rs` tests and three new `config.rs` tests are unproven.
- Clippy was **not** run at all. Nightly pedantic+nursery under `-D warnings` is
  the real gate here and it has not been consulted.
