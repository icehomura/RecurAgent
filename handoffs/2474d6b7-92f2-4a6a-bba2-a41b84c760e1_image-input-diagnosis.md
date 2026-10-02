# Handoff — why image input (drag-drop + clipboard paste) still fails

Investigation only. **No repo files were modified or committed** (the `git status`
dirty set belongs to other agents; do not sweep it into a commit).

## What was asked
Research why image input is still broken on both paths:
- dragging an image file onto the terminal, and
- pasting an image from the clipboard (Ctrl+V).

## Environment established
- Harness shell: `MINGW64_NT-10.0-26200` (Git Bash), **native Windows**, not WSL.
- `WT_SESSION` is set → the user runs under **Windows Terminal**; `TERM=xterm-256color`,
  `TERM_PROGRAM` empty. `running_under_wsl()` is therefore **false**, so the
  `powershell.exe` image-paste path is not used; the native `arboard` path is.
- `target/release/ra.exe` mtime **Oct 1 12:58 local (+0800)**.

## Primary finding — the tested binary predates the fixes
HEAD is `cdb977cce`. The image work is in HEAD's ancestry:
- `2c690b2b0` (Oct 1 **17:31 +0800**) — `build(tui): enable clipboard + image-resize with the tui feature`
- `7c2e0a0a5` (Oct 1 **18:58 +0800**) — `feat(ftui): paste/drag file paths into @file refs, show an image attachment box`
- `a02588a2e` (Sep 24) — ftui `@file` attachments + image paste

`ra.exe` at 12:58 was built **before** the 17:31 and 18:58 commits. First action for
the next agent: **rebuild** (default features are enough; `tui` pulls
`clipboard` + `image-resize`, see `Cargo.toml:570-592`) and re-test before changing
anything. Feature presence in the current stripped binary could **not** be proven
(no `target/release/deps`; no `arboard` strings survive LTO/strip).

## Real gaps that remain even after a rebuild

### 1. Ctrl+V never reaches the app under Windows Terminal
`AppAction::PasteImage` defaults to `ctrl+v` only (`src/keybindings.rs:1595`; key
mapping `:718`). Windows Terminal's default keybinding claims `Ctrl+V` for its own
paste and does **not** forward it. With an image-only clipboard WT pastes no text,
so the app receives nothing and the handler at `src/interactive_ftui.rs:5083-5098`
never runs. Classic has the same limitation.
The retired ledger already records this: `docs/dropin-112-feature-inventory-matrix.md:609`
("Paste image | Ctrl+V | Classic only; inert on ftui"), stale relative to the ftui
wiring added in `a02588a2e`.
Fix direction: add a non-intercepted default chord (e.g. `ctrl+shift+v` or `alt+v`)
and/or a slash command; update `docs/keybindings.md`, `/hotkeys`, and the ledger.

### 2. Drag/drop of a file outside cwd is refused by read-scope
Confirmed in WT source (`TermControl.cpp`): a drop calls `_pasteTextWithBroadcast`
→ `Core.PasteText`, i.e. it arrives as a bracketed paste → ftui `Event::Paste`
handled at `src/interactive_ftui.rs:5171-5183` → `normalize_pasted_file_refs`
(`src/interactive/file_refs.rs:291-332`) → `@<path>`. But on submit,
`prepare_prompt` (`src/interactive_ftui.rs:7080-7117`) calls
`process_file_arguments` (`src/tools.rs:5025-5101`), which enforces read scope to
`[cwd, agent_dir]` via `enforce_read_scope` (`src/tools.rs:4188-4225`,
error at `:4205`) and `read_file_capped_within_roots_sync` (`src/tools.rs:1969`).
A screenshot in `C:\Users\...\Pictures\...` is **outside cwd** → rejected with
"Cannot read outside the working directory or agent dir". Pasted clipboard images
work only because `paste_image_from_clipboard` writes into `<agent dir>/pastes`
(`src/interactive/keybindings.rs:164`); dropped files are not copied there.
Fix direction: on attach, copy/allow dropped files outside cwd (e.g. stage them
under `<agent dir>/pastes`), or widen the scope for user-attached refs.

### 3. If a drop is delivered as typed chars (not a bracketed paste)
`normalize_pasted_file_refs` only runs on `Event::Paste`; typed text lands in the
editor as a literal path. `extract_file_references` only rewrites `@token`s, so no
attachment is produced and the model receives a bare path string. WT does send a
paste today, but other terminals/Windows-console paths may not.
Fix direction: normalize at submit time too (the helper already refuses prose).

## Next agent — starting position
1. Rebuild from `cdb977cce` with default features; re-run the user's two repros
   under Windows Terminal and capture whether `AppAction::PasteImage` fires at all.
2. Add a terminal-safe paste-image chord and cover it in `src/keybindings.rs`
   tests + `format_hotkeys_for_ftui`.
3. Decide and implement scope policy for out-of-cwd attachments (gap 2), with a test
   under `src/interactive_ftui.rs` / `src/interactive/file_refs.rs`.
4. Only then touch the stale ledger line at
   `docs/dropin-112-feature-inventory-matrix.md:609`.

## Useful entry points
- `src/interactive_flui` handler: `src/interactive_ftui.rs:5083`, paste `:5171`, `prepare_prompt` `:7080`
- classic: `src/interactive/keybindings.rs:136` / `:1019`, `src/interactive.rs:2158`
- shared normalization: `src/interactive/file_refs.rs:291`
- read pipeline: `src/tools.rs:5025`, `:4188`, `:1969`
- default bindings: `src/keybindings.rs:1595`
