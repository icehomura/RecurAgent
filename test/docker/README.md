# test/docker — RecurAgent quality-gate container

A reproducible Linux box that **clones the `icehomura/RecurAgent` fork** and runs
the project's quality gates, together with the author's full **Flywheel** tool
chain. State is persisted in Docker named volumes, so cargo caches, the clone,
and DSR config survive between runs.

```
test/docker/
├── Dockerfile               # Ubuntu 24.04 + Rust pin + Go + Bun + Flywheel tools
├── docker-compose.yml       # gate / shell / dev / dev-shell services + volumes
├── .dockerignore
├── .env.example             # copy to .env to override
├── scripts/
│   ├── install-tools.sh     # ACFS `stack-only` (default) or direct installers
│   ├── entrypoint.sh        # clone/refresh the fork, then exec the command
│   ├── run-quality.sh       # runs the checks listed in .dsr/repos.yaml
│   └── bin/{curl,wget}      # shims: github.com release assets via gh-proxy.com
└── README.md
```

## Host requirements

- Docker Engine / Docker Desktop with BuildKit. On Windows this must be a
  working WSL2 backend — a wedged WSL VM (commands hang with
  `Hcs/E_CONNECTION_TIMEOUT`) also wedges the daemon;
  `wsl --shutdown` then restarting Docker Desktop clears it.
- **Disk**: the image plus a full `--all-targets` build of this crate needs tens
  of GB. `docker builder prune -f` before a rebuild when the host volume is
  near capacity; build cache for this image alone has reached 21 GB.

## Quick start

```bash
cd test/docker
cp .env.example .env                    # optional
docker compose build                    # ~10-20 min (Rust + full toolchain)
docker compose run --rm gate            # clone fork, run all gates
```

Interactive shell against the persisted workspace:

```bash
docker compose run --rm shell
```

Test **local uncommitted code** (bind-mounts the repo root, no clone):

```bash
docker compose --profile dev run --rm dev        # run gates on the mount
docker compose --profile dev run --rm dev-shell  # shell on the mount
```

## Network (mainland China / GFW)

`raw.githubusercontent.com` and `registry-1.docker.io` are unreachable from
some networks. This image works around that by default:

| Channel | Default mirror | Override |
|---------|----------------|----------|
| GitHub raw entry files | `cdn.jsdelivr.net/gh/...@ref/...` | `GH_RAW_MODE=jsdelivr\|ghproxy\|direct` |
| GitHub release assets | `gh-proxy.com/https://github.com/...` | `GH_RELEASE_MODE=ghproxy\|direct` |
| Ubuntu apt archive | `mirrors.tuna.tsinghua.edu.cn/ubuntu` (http) | `APT_MIRROR` |
| rustup dist | `mirrors.ustc.edu.cn/rust-static` | `RUSTUP_DIST_SERVER`, `RUSTUP_UPDATE_ROOT` |
| crates.io | USTC sparse index | `CARGO_REGISTRY_MIRROR=ustc\|rsproxy\|none` |
| Go modules | `goproxy.cn` | `GOPROXY` |
| pip | Tsinghua PyPI | `PIP_INDEX_URL` |
| npm / bun | `registry.npmmirror.com` | `NPM_CONFIG_REGISTRY` |
| base image | local `ubuntu:24.04` / Docker Hub | configure a Docker registry mirror |

The `curl`/`wget` shims are installed at `/usr/local/bin`, which precedes
`/usr/bin` on `PATH`, so they shadow the system tools without any PATH
manipulation — and `~/.local/bin` stays `PATH`'s first entry, which is where
installers put their binary. They rewrite **only**
`github.com/<owner>/<repo>/releases/download/...` URLs — that is where the
throttling is. Everything else under `github.com` passes through untouched:
gh-proxy answers `403` for the `/releases/latest` HTML page that installers
follow to resolve a version, and rewriting that page made every tool conclude
"no release version found" and compile itself from source.

`api.github.com` is blocked here, so installers that resolve a version only
through the REST API fail (`slb`); the ones with a redirect-based fallback
(`br`, `cass`, `cm`, `brenner`, `apr`, `jfp`) resolve fine. Each installer entry
file is fetched from the first mirror that answers (jsdelivr, then gh-proxy,
then `raw.githubusercontent.com`), because jsdelivr intermittently 404s a file
it has not cached.

The upstream installers themselves download release artifacts and may fetch
more scripts from `raw.githubusercontent.com` internally; `GH_RAW_MODE` only
mirrors the entry-point files. If `raw.githubusercontent.com` is blocked on your
network, pass a proxy so those internal fetches succeed:

```bash
cat >> .env <<'EOF'
HTTP_PROXY=http://host.docker.internal:7890
HTTPS_PROXY=http://host.docker.internal:7890
NO_PROXY=localhost,127.0.0.1,::1
EOF
docker compose build
```

`TOOLS_STRATEGY=none` builds without third-party tools and needs no proxy.
Clone/pull of the fork uses `github.com`, which is reachable on the networks
observed here.

## What runs

`run-quality.sh` **reads the check list out of `.dsr/repos.yaml`** — the same
seven checks `dsr quality` would run — and executes each one verbatim, with only
the `rch exec --` transport prefix stripped so cargo compiles locally:

| # | Check (from `.dsr/repos.yaml`) |
|---|-------------------------------|
| 1 | `cargo fmt --check` |
| 2 | `cargo check --locked --all-targets` |
| 3 | `cargo clippy --locked --all-targets -- -D warnings` |
| 4 | `cargo test --locked --all-targets --no-fail-fast` |
| 5 | `bash tests/installer_regression.sh` |
| 6 | `python3 scripts/check_module_reachability.py` |
| 7 | `python3 scripts/check_fixture_read_patience.py` |
| + | optional: `ubs --only=rust .` when UBS is installed (not part of the recipe) |

Reading the recipe instead of copying it is deliberate: a hand-copied list had
already lost check 7. The recipe key is `pi_agent_rust`; override with
`DSR_TOOL=<key>`.

Cargo is bounded with `CARGO_INCREMENTAL=0`, `CARGO_PROFILE_TEST_DEBUG=0`,
`CARGO_BUILD_JOBS=2`, `TMPDIR=/tmp` — the same knobs the DSR recipe sets to keep
the target tree from filling a disk.

A green run here is a **fork-local** result: it is not a DSR run and must not be
cited as DSR-attributed evidence (see the fork note in `AGENTS.md`).

## RCH is deliberately local here

The registered recipe runs every heavy check through `rch` with
`RCH_REQUIRE_REMOTE=1`, which **fails closed** when no SSH worker can take the
job. A container has no fleet, so this image sets `RCH_DISABLED=1` and compiles
locally. To use a real fleet instead, mount the host's `~/.ssh` and
`~/.config/rch` and drop `RCH_DISABLED` / set `RCH_REQUIRE_REMOTE=1`.

## Tools installed

Default `TOOLS_STRATEGY=direct` installs each tool from its own `install.sh`,
streaming progress with a per-tool timeout (900 s). `TOOLS_STRATEGY=none` skips
third-party tools entirely.

`TOOLS_STRATEGY=acfs` runs the author's bootstrap (`--profile stack-only
--skip-ubuntu-upgrade`). It is **not** the default because, as of 2026-10-02,
ACFS v0.9.0 fails closed on a stale `uv` installer checksum
(`https://astral.sh/uv/install.sh` no longer matches the pinned hash) and then
does not fail fast — it proceeds into the coding-agents phase, which stalls on
`claude.ai`. `uv` is installed directly in the image instead; see the
`Dockerfile`. ACFS remains available for the day that pin is refreshed.

| Tool | Repo | Lang / Stars | Install |
|------|------|--------------|---------|
| **DSR** | `Dicklesworthstone/doodlestein_self_releaser` | Shell / 75 | `curl …/install.sh \| bash` (needs gh, docker, act) |
| **RCH** | `Dicklesworthstone/remote_compilation_helper` | Rust / 66 | `curl …/install.sh \| bash -s -- --easy-mode` |
| **BR** | `Dicklesworthstone/beads_rust` | Rust / 1112 | `curl …/install.sh \| bash` / `cargo install beads_rust` |
| **BV** | `Dicklesworthstone/beads_viewer` | Go / 1705 | `curl …/install.sh \| bash` / `go install ./cmd/bv` |
| **UBS** | `Dicklesworthstone/ultimate_bug_scanner` | Python / 304 | `curl …/install.sh \| bash` |
| **DCG** | `Dicklesworthstone/destructive_command_guard` | Rust / 6076 | `curl …/master/install.sh \| bash -s -- --easy-mode` |
| **AM** | `Dicklesworthstone/mcp_agent_mail` | Python / 2180 | `curl …/scripts/install.sh \| bash -s -- --yes` |
| **CASS** | `Dicklesworthstone/coding_agent_session_search` | Rust / 1158 | `curl …/install.sh \| bash -s -- --easy-mode --verify` |
| **CM** | `Dicklesworthstone/cass_memory_system` | TypeScript / 442 | `curl …/install.sh \| bash -s -- --easy-mode --verify` |
| **SLB** | `Dicklesworthstone/slb` | Go / 81 | `curl …/scripts/install.sh \| bash` |
| **NTM** | `Dicklesworthstone/ntm` | Go / 452 | `curl …/install.sh \| bash -s -- --easy-mode` |
| **CAAM** | `Dicklesworthstone/coding_agent_account_manager` | Go / 205 | `curl …/install.sh \| bash` |
| **RU** | `Dicklesworthstone/repo_updater` | Shell / 128 | `curl …/install.sh \| bash` |
| **PT** | `Dicklesworthstone/process_triage` | Rust / 28 | `curl …/install.sh \| bash` |
| **SRPS** | `Dicklesworthstone/system_resource_protection_script` | Shell / 31 | `curl …/install.sh \| bash` |
| **ACFS** | `Dicklesworthstone/agentic_coding_flywheel_setup` | Shell / 1656 | `curl …/install.sh \| bash -s -- --yes --profile stack-only` |

`install_direct` also installs `brenner`
(`Dicklesworthstone/brenner_bot`), `apr`
(`Dicklesworthstone/automated_plan_reviser_pro`), and `jfp`. `jfp` is fetched
from `https://jeffreysprompts.com/install-cli.sh`: its repository publishes no
installer (the in-repo `install-cli.sh` path 404s), so `install_url` takes the
full URL instead of an owner/repo/ref triple.

## Persisted state

| Volume | Mount | Contents |
|--------|-------|----------|
| `cargo-registry` | `~/.cargo/registry` | crate cache |
| `cargo-git` | `~/.cargo/git` | git dependency cache |
| `workspace` | `/data/projects` | cloned fork + `target/` |
| `dsr-config` | `~/.config/dsr` | `repos.yaml`, `repos.d/` |
| `dsr-state` | `~/.local/state/dsr` | DSR quality logs |
| `flywheel-bin` | `~/.local/bin` | tool binaries |

Reset everything: `docker compose down -v` (destructive — removes those volumes).

## Notes / caveats

- **Rust pin**: the image pre-installs `nightly-2026-08-31` to match
  `rust-toolchain.toml`. If the repo pin changes, rebuild with
  `--build-arg RUST_TOOLCHAIN=<new>`; otherwise rustup re-downloads at run time.
- **`curl | bash` installers track `main`**, so two builds on different days can
  differ. ACFS verifies its installers against `checksums.yaml`; the `direct`
  path does not.
- **Docker-in-Docker**: `docker` and `act` are installed so DSR's dependency
  check passes. `dsr quality` works without a daemon; `dsr build`/`dsr release`
  need `/var/run/docker.sock` mounted or a DinD sidecar.
- **`gh`** is installed but unauthenticated. Anything needing GitHub API auth
  (rate limits, pushes) must pass `GH_TOKEN` at run time.
