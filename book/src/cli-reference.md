# CLI Reference

## `ra chat`

Interactive multi-turn conversation with readline history.

```
ra chat [OPTIONS]

Options:
  -c, --cwd <PATH>         Working directory
      --config <PATH>      Config file path
      --provider <NAME>    LLM provider
      --model <NAME>       Model name
      --base-url <URL>     Custom API endpoint
  -m, --message <MSG>      Single message (non-interactive)
      --max-iterations <N> Max tool iterations per message (default: 50)
      --profile <NAME>     Runtime tool profile (default: coding — lean
                           core-coding tools; coding-full = everything)
  -v, --verbose            Show tool outputs
      --no-retry           Disable retry
```

**Features:**

- Arrow keys and line editing (rustyline)
- Persistent history at `.ra/history/chat_history`
- Exit: `/exit`, `/quit`, `exit`, `quit`, `:q`, Ctrl+C, Ctrl+D
- Lean default tool surface (files, shell, search, memory, spawn); web,
  research, pipelines, and bundled skills via `--profile coding-full`
  (see [Configuration → Runtime Tool Profiles](./configuration.md#runtime-tool-profiles))

**Examples:**

```bash
ra chat                              # Interactive (default)
ra chat --provider deepseek          # Use DeepSeek
ra chat --model glm-4-plus           # Auto-detects Zhipu
ra chat --message "Fix auth bug"     # Single message, exit
ra chat --profile coding-full        # Unfiltered tool set (web, skills, pipelines)
```

---

## `ra gateway`

Run as a persistent multi-channel daemon.

```
ra gateway [OPTIONS]

Options:
  -c, --cwd <PATH>         Working directory
      --config <PATH>      Config file path
      --provider <NAME>    Override provider
      --model <NAME>       Override model
      --base-url <URL>     Override API endpoint
  -v, --verbose            Verbose logging
      --no-retry           Disable retry
```

Requires a `gateway` section in config with a `channels` array. Runs continuously until Ctrl+C.

---

## `ra init`

Initialize workspace with config and bootstrap files.

```
ra init [OPTIONS]

Options:
  -c, --cwd <PATH>    Working directory
      --defaults       Skip prompts, use defaults
```

**Creates:**

- `.ra/config.json` -- Provider/model config
- `.ra/.gitignore` -- Ignores state files
- `.ra/AGENTS.md` -- Agent instructions template
- `.ra/SOUL.md` -- Personality template
- `.ra/USER.md` -- User info template
- `.ra/memory/` -- Memory storage directory
- `.ra/sessions/` -- Session history directory
- `.ra/skills/` -- Custom skills directory

---

## `ra status`

Show system status.

```
ra status [OPTIONS]

Options:
  -c, --cwd <PATH>    Working directory
```

**Example output:**

```
ra Status
══════════════════════════════════════════════════

Config:    .ra/config.json (found)
Workspace: .ra/            (found)
Provider:  anthropic
Model:     claude-sonnet-4-20250514

API Keys
──────────────────────────────────────────────────
  Anthropic    ANTHROPIC_API_KEY         set
  OpenAI       OPENAI_API_KEY           not set
  ...

Bootstrap Files
──────────────────────────────────────────────────
  AGENTS.md        found
  SOUL.md          found
  USER.md          found
  TOOLS.md         missing
  IDENTITY.md      missing
```

---

## `ra serve`

Launch the web UI and REST API server. Requires the `api` feature flag.

```bash
cargo install --path crates/ra-cli --features api
ra serve                               # Binds to 127.0.0.1:50080
ra serve --host 0.0.0.0 --port 50080   # Accept external connections
ra serve --solo                        # Enable local no-password "solo" login
ra serve --stdio                       # AppUI JSON-RPC over stdin/stdout (no HTTP bind)
```

Key options:

| Flag | Description |
|------|-------------|
| `--port <N>` | Port to listen on (default `50080`, in IANA's dynamic range) |
| `--host <ADDR>` | Bind address (default `127.0.0.1`; use `0.0.0.0` for external) |
| `--stdio` | Run the AppUI JSON-RPC protocol over stdin/stdout instead of HTTP |
| `--solo` | Enable the loopback-only no-password solo login (`POST /api/auth/solo*`); also `OCTOS_SOLO_LOGIN=1`. Never enable behind a reverse proxy |
| `--data-dir <P>` | Data directory for episodes/memory/sessions (default `$OCTOS_HOME` or `~/.ra`) |
| `--auth-token <T>` | Admin bearer token for API access. Visible in the process list (`ps`) — prefer the `OCTOS_AUTH_TOKEN` env var or the config file |
| `--config <P>` | Config file path |
| `--swarm-backend <stdio\|http>` | Enable the `/api/swarm/*` contract-authoring endpoints (pairs with `--swarm-backend-cmd` / `--swarm-backend-url`) |

Serves the embedded SPAs at `/app/` (chat/studio) and `/admin/` (operator dashboard) plus the WS UI Protocol at `/api/ui-protocol/ws`. A `/metrics` endpoint provides Prometheus-format metrics (`ra_tool_calls_total`, `octos_tool_call_duration_seconds`, `ra_llm_tokens_total`). Multiple instances can run in parallel with distinct `--data-dir` + `--port`.

---

## `ra clean`

Clean database and state files.

```bash
ra clean [--all] [--dry-run]
```

| Flag | Description |
|------|-------------|
| `--all` | Remove all state files |
| `--dry-run` | Show what would be removed without deleting |

---

## `ra completions`

Generate shell completions.

```bash
ra completions <shell>
```

Supported shells: `bash`, `elvish`, `fish`, `powershell`, `zsh`.

**Install (static).** The printed script completes flags and subcommands:

```bash
ra completions bash > ~/.local/share/bash-completion/completions/ra
```

**Install (dynamic, recommended).** Source the registration script so your shell
calls back into `ra` for candidates on every tab — regenerating it at shell
startup, so it stays current with the installed binary:

```bash
# bash (4+; older bash such as the macOS system bash 3.2 may not source this
# form reliably — use the static script above there, or Homebrew's bash)
echo 'source <(OCTOS_COMPLETE=bash ra)' >> ~/.bashrc
# zsh
echo 'source <(OCTOS_COMPLETE=zsh ra)' >> ~/.zshrc
# fish
echo 'OCTOS_COMPLETE=fish ra | source' >> ~/.config/fish/config.fish
# elvish
echo 'eval (E:OCTOS_COMPLETE=elvish ra | slurp)' >> ~/.elvish/rc.elv
# powershell
echo '$env:OCTOS_COMPLETE = "powershell"; ra | Out-String | Invoke-Expression; Remove-Item Env:\OCTOS_COMPLETE' >> $PROFILE
```

`--dynamic` prints a candidate list for one category instead of a script —
useful for scripts and for checking what completions would offer:

```bash
ra completions bash --dynamic models     # model names from model_catalog.json
ra completions bash --dynamic providers  # provider families from the registry
ra completions bash --dynamic sessions   # session ids in ./.ra/sessions
ra completions bash --dynamic skills     # skills in ./.ra/skills
```

---

## `ra cron`

Manage scheduled jobs.

```bash
ra cron list [--all]                  # List active jobs (--all includes disabled)
ra cron add [OPTIONS]                 # Add a cron job
ra cron remove <job-id>               # Remove a cron job
ra cron enable <job-id>               # Enable a cron job
ra cron enable <job-id> --disable     # Disable a cron job
```

**Adding jobs:**

```bash
ra cron add --name "report" --message "Generate daily report" --cron "0 0 9 * * * *"
ra cron add --name "check" --message "Check status" --every 3600
ra cron add --name "once" --message "Run migration" --at "2025-03-01T09:00:00Z"
```

Cron expressions use standard syntax. Jobs support an optional `timezone` field with IANA timezone names (e.g., `"America/New_York"`, `"Asia/Shanghai"`). When omitted, UTC is used.

When Matrix is fronted by a BotFather-style management bot, the same cron runtime is also available through natural-language chat commands:

```text
/schedule 20秒之后提醒我看天气
/schedule 每天早上 9 点提醒我看天气
/schedules
/unschedule <job-id>
```

These commands create, list, and remove jobs scoped to the current Matrix room or DM instead of exposing raw `cron` syntax to end users.

---

## `ra channels`

Manage messaging channels.

```bash
ra channels status    # Show channel compile/config status
ra channels login     # WhatsApp QR code login
```

The status command shows a table with channel name, compile status (feature flags), and config summary (env vars set/missing).

---

## `ra office`

Office file manipulation (DOCX/PPTX/XLSX). Native Rust implementation with no external dependencies for the core operations; a few subcommands optionally shell out to LibreOffice (`soffice`) when installed.

```bash
# Core (pure Rust)
ra office extract <file>                     # Extract text as Markdown
ra office unpack <file> <output-dir>         # Unpack into pretty-printed XML
ra office pack <input-dir> <output>          # Pack directory into Office file
ra office clean <dir>                        # Remove orphaned files from unpacked PPTX
ra office validate <file>                    # Validate an Office file's structure
ra office make-slide <image> -o <pptx>       # Compose a slide (bg image + --texts JSON overlays) into a .pptx
ra office add-slide <unpacked-dir> <source>  # Add a slide to an unpacked PPTX (dup slideN.xml or apply slideLayoutN.xml)
ra office overlay-text <image> <text>        # Burn text onto a PNG/JPEG (--x/--y position)
ra office comment <unpacked-dir> <id> <text> # Add a comment to an unpacked DOCX

# LibreOffice-backed (require `soffice` on PATH)
ra office accept-changes <input> <output>    # Accept tracked changes (DOCX) → clean copy
ra office recalc <file>                      # Recalculate XLSX formulas
ra office thumbnail <file> [OPTIONS]         # Render slide/page thumbnails (also needs Poppler's pdftoppm)
ra office soffice <args...>                  # Passthrough to a sandboxed soffice
```

`make-slide` composes a rendered background image plus JSON text overlays into a `.pptx` slide (used by the slides pipeline). `comment` inserts its text into the DOCX XML verbatim, so pass **pre-escaped** XML (`&amp;`, `&lt;`, …). Office is **CLI-only** — it is not exposed as an agent tool. Run `ra office <subcommand> --help` for the exact arguments.

---

## `ra account`

Manage sub-accounts under profiles. Sub-accounts inherit LLM provider config but have their own data directory (memory, sessions, skills) and channels.

```bash
ra account list --profile <id>                         # List sub-accounts
ra account create --profile <id> <name> [OPTIONS]      # Create sub-account
ra account update <id> [OPTIONS]                       # Update sub-account
```

---

## `ra auth`

OAuth login and API key management.

```bash
ra auth login --provider openai           # PKCE browser OAuth
ra auth login --provider openai --device-code  # Device code flow
ra auth login --provider anthropic        # Paste-token (stdin)
ra auth logout --provider openai          # Remove stored credential
ra auth status                            # Show authenticated providers
```

Credentials are stored in `~/.ra/auth.json` (file mode 0600). The auth store is checked before environment variables when resolving API keys.

---

## `ra skills`

Manage skills.

```bash
ra skills list                            # List installed skills
ra skills install user/repo/skill-name    # Install from GitHub
ra skills remove skill-name               # Remove a skill
```

Fetches `SKILL.md` from the GitHub repo's main branch and installs to `.ra/skills/`.

---

## `ra doctor`

Run local environment diagnostics for the ra server and print a health report.

```bash
ra doctor [OPTIONS]

Options:
      --json          Emit a machine-readable JSON support bundle
      --verbose       Add resolved paths / versions to each line
      --strict        Promote warnings to failures (affects exit code)
      --data-dir <P>  Data dir override (defaults to ~/.ra)
```

Checks the installed binary's location (and PATH shadowing), the terminal (terminfo), config/data-dir writability, the UI-protocol version skew, and `api.github.com` reachability for update checks. (It does **not** validate provider API keys — use `ra status` for those.) Exit code is non-zero when a check fails (or, with `--strict`, when any check warns). Use `--json` to attach the bundle to a bug report.

---

## `ra docs`

Generate reference documentation for the built-in tools and providers.

```bash
ra docs [--output <DIR>]
```

With no `--output` the Markdown is written to stdout; otherwise it creates `<DIR>` and writes `<DIR>/TOOLS.md`. The output documents the built-in tools plus a provider list that is currently **hard-coded** in the command, so it can lag the actual provider registry.

---

## `ra memory`

Inspect and drive the memory-refresh pipeline (see [Memory & Skills](./memory-skills.md)).

```bash
ra memory refresh [--data-dir <P>]          # Run one extraction pass now
ra memory status  [--data-dir <P>]          # Sweep state: lock holder, backlog, budgets
ra memory remember "<text>" [--data-dir <P>] # Host-authored remember (no model in the loop)
ra memory forget  "<text>" [--sensitive]     # Free-text forget (starts a confirm flow)
ra memory forget  --id ^m4k2abq              # Hard-delete an exact MEMORY.md entry
```

`refresh` works even when the background sweep is disabled in config, but refuses when a running service holds the profile lock. `remember`/`forget` only write a **local staging note** (no LLM at write time); the note is applied on the next consolidation pass — the background sweep or `ra memory refresh` — which *does* send it to the consolidation model. `--sensitive` interim-archives candidates immediately and scrubs them everywhere on confirmation.

---

## `ra update`

Check for a newer ra release.

```bash
ra update --check         # Print the update plan; exit 10 if an update is available, 0 if up to date
ra update --check --json  # Same, machine-readable
```

This is the Stage-2 **check-only** command: it detects the installer lineage (Homebrew, cargo, cargo-dist receipt, …) and prints the exact per-installer upgrade command. Applying updates in-place is Stage 3 and is **not wired yet** — run the printed command to upgrade.

---

## `ra mcp-serve`

Expose ra itself as an MCP server so an outer orchestrator can invoke it as a sub-agent.

```bash
ra mcp-serve [OPTIONS]

Options:
      --transport <stdio|http>  Transport to bind (default: stdio)
      --bind <ADDR>             Bind address for the HTTP transport (default: 127.0.0.1:4033)
  -c, --cwd <PATH>              Working directory
```

Both transports are served by the [rmcp](https://github.com/modelcontextprotocol/rust-sdk) SDK. `stdio` uses parent-trust auth (MCP JSON-RPC over stdin/stdout). `http` is an MCP Streamable HTTP endpoint (SSE responses with a per-session `Mcp-Session-Id`) and **requires** a bearer token via the `OCTOS_MCP_SERVER_TOKEN` environment variable; it is only compiled into builds with the `api` feature (otherwise use `--transport stdio`). Binding `--bind` to a non-loopback address disables rmcp's DNS-rebinding host guard, leaving the bearer token as the sole authenticator.

The session it drives runs inside the configured sandbox (`SandboxMode::Auto` by default), so outer callers cannot use the exposed `run_ra_session` tool to read or write outside the working directory.

---

## `ra admin`

Tenant and tunnel management for the hosted/fleet deployment (frps reverse-tunnel onboarding). Most single-user installs never need this.

```bash
ra admin create-tenant --name <id> [OPTIONS]   # Assign subdomain, auth token, SSH/serve ports
ra admin list-tenants                          # List registered tunnel tenants
ra admin delete-tenant <id>                    # Remove a tenant
ra admin show-tenant-config <id>               # Print the frpc config for a tenant
ra admin reset-token                           # Reset the admin token (restores bootstrap auth)
ra admin set-smtp-password                     # Write smtp_secret.json (0600) for OTP email
ra admin operator-summary [--base-url <URL>] [--auth-token <TOK>]  # Condensed runtime observability view
```

`create-tenant` defaults the base domain to `ra-cloud.org` and the local serve port to `50080` (matching `ra serve`). `reset-token` and `set-smtp-password` operate on the local `--data-dir`; `operator-summary` queries a running API.
