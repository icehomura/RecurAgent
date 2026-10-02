# RecurAgent quality-gate container.
#
# Purpose: a reproducible Linux box that clones the icehomura/RecurAgent fork and
# runs the project's quality gates — the native equivalent of the DSR recipe in
# .dsr/repos.yaml — together with the author's full "Flywheel" tool chain
# (br, bv, ubs, dcg, am, cass, cm, slb, ntm, caam, ru, pt, srps, dsr, rch, ...).
#
# RCH runs in LOCAL compile mode (RCH_DISABLED=1). There is no SSH worker fleet
# inside the container, so the fail-closed `RCH_REQUIRE_REMOTE=1` recipe from
# .dsr/repos.yaml is deliberately NOT used here; see test/docker/README.md.
#
# NETWORK: raw.githubusercontent.com and Docker Hub are unreachable from some
# networks (notably mainland China). This image therefore routes Rust/Go/npm/pip
# and GitHub-raw fetches through mirrors by default. A proxy can be injected at
# build time with the standard build args:
#
#   docker build \
#     --build-arg HTTPS_PROXY=http://host.docker.internal:7890 \
#     --build-arg HTTP_PROXY=http://host.docker.internal:7890 \
#     -t recur-agent-gates test/docker
#
#   build: docker compose -f test/docker/docker-compose.yml build
#   run  : docker compose -f test/docker/docker-compose.yml run --rm gate

FROM ubuntu:24.04

ARG DEBIAN_FRONTEND=noninteractive
# Non-root user, matching the ACFS manifest (`user: ubuntu`).
ARG TARGET_USER=ubuntu
# Must match the channel in the repo's rust-toolchain.toml.
ARG RUST_TOOLCHAIN=nightly-2026-08-31
# acfs   = run the author's own full-toolchain bootstrap (stack-only profile)
# direct = install each tool from its own install.sh (default; ACFS is currently
#          broken by an upstream `uv` checksum-pin mismatch and does not fail fast)
# none   = environment only, no third-party tools
ARG TOOLS_STRATEGY=direct
# ACFS module profile: stack-only = the tool chain, no shell/agents/cloud.
ARG ACFS_PROFILE=stack-only
ARG ACFS_REF=main

# --- mirror configuration --------------------------------------------------
# How to fetch GitHub raw files: jsdelivr | ghproxy | direct
ARG GH_RAW_MODE=jsdelivr
# How to fetch GitHub release assets: ghproxy | direct
ARG GH_RELEASE_MODE=ghproxy
# Rustup dist mirror (USTC). Works globally; override for other preferences.
ARG RUSTUP_DIST_SERVER=https://mirrors.ustc.edu.cn/rust-static
ARG RUSTUP_UPDATE_ROOT=https://mirrors.ustc.edu.cn/rust-static/rustup
# Go module proxy.
ARG GOPROXY=https://goproxy.cn,direct
# Go SDK download base. go.dev is fastest here but occasionally flaky; the RUN
# step falls back to the mirrors in order and aborts a stalled transfer.
ARG GOLANG_MIRROR=https://go.dev/dl
# Python / npm mirrors.
ARG PIP_INDEX_URL=https://pypi.tuna.tsinghua.edu.cn/simple
ARG NPM_CONFIG_REGISTRY=https://registry.npmmirror.com
# Cargo source replacement: ustc | rsproxy | none
ARG CARGO_REGISTRY_MIRROR=ustc
# Ubuntu archive mirror used inside the image (tuna; USTC's apt mirror is NOT
# reachable from containers behind some TUN setups, archive.ubuntu.com is slow).
ARG APT_MIRROR=http://mirrors.tuna.tsinghua.edu.cn/ubuntu

ENV LANG=C.UTF-8 \
    LC_ALL=C.UTF-8 \
    TZ=UTC \
    DEBIAN_FRONTEND=noninteractive \
    RUSTUP_HOME=/home/${TARGET_USER}/.rustup \
    CARGO_HOME=/home/${TARGET_USER}/.cargo \
    GOBIN=/home/${TARGET_USER}/.local/bin \
    RUSTUP_DIST_SERVER=${RUSTUP_DIST_SERVER} \
    RUSTUP_UPDATE_ROOT=${RUSTUP_UPDATE_ROOT} \
    GOPROXY=${GOPROXY} \
    GOSUMDB=off \
    PIP_INDEX_URL=${PIP_INDEX_URL} \
    PIP_TRUSTED_HOST=pypi.tuna.tsinghua.edu.cn \
    NPM_CONFIG_REGISTRY=${NPM_CONFIG_REGISTRY} \
    PATH=/home/${TARGET_USER}/.local/bin:/home/${TARGET_USER}/.cargo/bin:/usr/local/go/bin:/home/${TARGET_USER}/.bun/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

# ---------------------------------------------------------------------------
# System packages
# ---------------------------------------------------------------------------
RUN set -eux; \
    if [ -f /etc/apt/sources.list.d/ubuntu.sources ]; then \
      sed -i -E "s#URIs:.*#URIs: ${APT_MIRROR}#" /etc/apt/sources.list.d/ubuntu.sources; \
    fi; \
    if [ -f /etc/apt/sources.list ]; then \
      sed -i -E "s#https?://(archive|security)\.ubuntu\.com/ubuntu#${APT_MIRROR}#g" /etc/apt/sources.list; \
    fi; \
    apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl wget git git-lfs openssh-client sshpass rsync \
      build-essential pkg-config libssl-dev libsqlite3-dev cmake clang libclang-dev \
      python3 python3-venv python3-pip pipx \
      jq bc file less vim-tiny nano \
      fd-find ripgrep fzf tmux procps psmisc lsof iproute2 util-linux \
      sqlite3 age gnupg unzip zip xz-utils tar \
      libxcb1-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev \
      sudo locales tzdata \
      gh docker.io \
 && ln -sf "$(command -v fdfind)" /usr/local/bin/fd \
 && rm -rf /var/lib/apt/lists/*

# ---------------------------------------------------------------------------
# Non-root user with passwordless sudo (ACFS expects the `ubuntu` user)
# ---------------------------------------------------------------------------
RUN set -eux; \
    if id -u "${TARGET_USER}" >/dev/null 2>&1; then \
      usermod --shell /bin/bash "${TARGET_USER}"; \
    else \
      useradd --create-home --shell /bin/bash --uid 1000 "${TARGET_USER}"; \
    fi; \
    echo "${TARGET_USER} ALL=(ALL) NOPASSWD:ALL" > "/etc/sudoers.d/90-${TARGET_USER}"; \
    chmod 0440 "/etc/sudoers.d/90-${TARGET_USER}"; \
    mkdir -p /data/projects \
             "/home/${TARGET_USER}/.config" \
             "/home/${TARGET_USER}/.config/dsr" \
             "/home/${TARGET_USER}/.local/state" \
             "/home/${TARGET_USER}/.local/state/dsr" \
             "/home/${TARGET_USER}/.local/share" \
             "/home/${TARGET_USER}/.local/bin" \
             "/home/${TARGET_USER}/.cargo/registry" \
             "/home/${TARGET_USER}/.cargo/git"; \
    chown -R "${TARGET_USER}:${TARGET_USER}" /data/projects "/home/${TARGET_USER}"

# ---------------------------------------------------------------------------
# Go SDK (ACFS bootstrap + go-installable tool fallback), installed system-wide
# ---------------------------------------------------------------------------
RUN set -eux; \
    go_ver="$(curl -fsSL --retry 3 --max-time 30 'https://go.dev/VERSION?m=text' 2>/dev/null | head -n1 || true)"; \
    if [ -z "$go_ver" ]; then \
      go_ver="$(curl -fsSL --retry 3 --max-time 30 'https://golang.google.cn/VERSION?m=text' | head -n1)"; \
    fi; \
    case "$(dpkg --print-architecture)" in \
      amd64) go_arch=amd64 ;; \
      arm64) go_arch=arm64 ;; \
      *)     go_arch=amd64 ;; \
    esac; \
    ok=0; \
    for base in "${GOLANG_MIRROR}" \
                https://mirrors.aliyun.com/golang \
                https://golang.google.cn/dl; do \
      if curl -fsSL --retry 3 --retry-delay 2 --connect-timeout 20 \
           --max-time 600 --speed-limit 200000 --speed-time 30 \
           "${base}/${go_ver}.linux-${go_arch}.tar.gz" -o /tmp/go.tgz; then \
        ok=1; break; \
      fi; \
    done; \
    [ "$ok" = 1 ] || { echo "Go SDK download failed from every mirror" >&2; exit 1; }; \
    tar -C /usr/local -xzf /tmp/go.tgz; \
    rm -f /tmp/go.tgz; \
    /usr/local/go/bin/go version

# ---------------------------------------------------------------------------
# Rust: pinned toolchain + stable fallback
# ---------------------------------------------------------------------------
USER ${TARGET_USER}
WORKDIR /home/${TARGET_USER}

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --profile minimal --default-toolchain "${RUST_TOOLCHAIN}" \
                 --component rustfmt,clippy

RUN rustup toolchain install stable --profile minimal --component rustfmt,clippy \
 && rustup component add rustfmt clippy --toolchain stable \
 && rustup show

# Cargo registry source replacement (mainland-China mirror by default).
RUN if [ "${CARGO_REGISTRY_MIRROR}" = "ustc" ]; then \
      printf '[source.crates-io]\nreplace-with = "ustc"\n\n[source.ustc]\nregistry = "sparse+https://mirrors.ustc.edu.cn/crates.io-index/"\n' \
        > "${CARGO_HOME}/config.toml"; \
    elif [ "${CARGO_REGISTRY_MIRROR}" = "rsproxy" ]; then \
      printf '[source.crates-io]\nreplace-with = "rsproxy-sparse"\n\n[source.rsproxy-sparse]\nregistry = "sparse+https://rsproxy.cn/index/"\n' \
        > "${CARGO_HOME}/config.toml"; \
    fi; \
    cat "${CARGO_HOME}/config.toml" 2>/dev/null || true

# yq + act: prebuilt release binaries, no `go install` compile step.
RUN set -eux; \
    case "$(dpkg --print-architecture)" in \
      amd64) yq_arch=amd64;  act_arch=x86_64 ;; \
      arm64) yq_arch=arm64;  act_arch=arm64 ;; \
      *)     yq_arch=amd64;  act_arch=x86_64 ;; \
    esac; \
    if [ "${GH_RELEASE_MODE}" = "ghproxy" ]; then GP="https://gh-proxy.com/"; else GP=""; fi; \
    mkdir -p "${GOBIN}"; \
    curl -fsSL --retry 3 --retry-delay 2 --connect-timeout 20 --max-time 600 \
      "${GP}https://github.com/mikefarah/yq/releases/latest/download/yq_linux_${yq_arch}" \
      -o "${GOBIN}/yq"; \
    chmod 0755 "${GOBIN}/yq"; \
    curl -fsSL --retry 3 --retry-delay 2 --connect-timeout 20 --max-time 600 \
      "${GP}https://github.com/nektos/act/releases/latest/download/act_Linux_${act_arch}.tar.gz" \
      -o /tmp/act.tgz; \
    tar -xzf /tmp/act.tgz -C /tmp act; \
    install -m 0755 /tmp/act "${GOBIN}/act"; \
    rm -f /tmp/act.tgz /tmp/act; \
    "${GOBIN}/yq" --version; \
    "${GOBIN}/act" --version

# minisign verifies release signatures: caam's installer resolves its release,
# downloads the signature, and *refuses* to install without a verifier
# (`ERROR: minisign is required to verify release signatures`) rather than
# silently downgrade to an unsigned source build. Its own apt layer rather than
# an entry in the package list above: that list precedes the Go SDK and rustup
# layers, whose downloads dominate a cold build, and editing it would
# invalidate both.
USER root
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends minisign; \
    rm -rf /var/lib/apt/lists/*; \
    minisign -v
USER ${TARGET_USER}

# ast-grep (prebuilt release). UBS' installer falls back to
# `cargo install ast-grep` when the binary is missing — 222 crates, measured at
# ~430 s of the tool layer — so shipping the release binary lets it skip that.
# This is also the binary the repo's ast-grep workflow expects.
RUN set -eux; \
    case "$(dpkg --print-architecture)" in \
      amd64) sg_arch=x86_64 ;; \
      arm64) sg_arch=aarch64 ;; \
      *)     sg_arch=x86_64 ;; \
    esac; \
    if [ "${GH_RELEASE_MODE}" = "ghproxy" ]; then GP="https://gh-proxy.com/"; else GP=""; fi; \
    curl -fsSL --retry 3 --retry-delay 2 --connect-timeout 20 --max-time 600 \
      "${GP}https://github.com/ast-grep/ast-grep/releases/latest/download/app-${sg_arch}-unknown-linux-gnu.zip" \
      -o /tmp/ast-grep.zip; \
    unzip -q -o /tmp/ast-grep.zip -d /tmp/ast-grep; \
    install -m 0755 /tmp/ast-grep/ast-grep "${GOBIN}/ast-grep"; \
    install -m 0755 /tmp/ast-grep/sg "${GOBIN}/sg"; \
    rm -rf /tmp/ast-grep.zip /tmp/ast-grep; \
    "${GOBIN}/ast-grep" --version; \
    "${GOBIN}/sg" --version

# ---------------------------------------------------------------------------
# Gate scripts. Copied before bun/uv because those two installers fetch their
# binaries from github.com release assets, which is the traffic the curl/wget
# shims in scripts/bin exist to mirror (GH_RELEASE_MODE=ghproxy).
#
# Only what the tool-chain layer needs is copied here. The gate scripts
# (entrypoint.sh, run-quality.sh) are copied *after* install-tools.sh runs, so
# editing them does not re-run the ~15-25 minute tool-chain layer.
# ---------------------------------------------------------------------------
COPY --chown=${TARGET_USER}:${TARGET_USER} scripts/install-tools.sh /usr/local/share/recur-agent-gates/install-tools.sh
COPY --chown=${TARGET_USER}:${TARGET_USER} scripts/bin/ /usr/local/share/recur-agent-gates/bin/

# uv fetches both its packages and its Python interpreters from the network,
# and it ignores PIP_INDEX_URL: without these it pulls ~160 packages from
# pypi.org and a 35 MB CPython tarball straight from GitHub (measured: 592 s
# for mcp_agent_mail alone). Both are overridable build args.
ARG UV_DEFAULT_INDEX=https://pypi.tuna.tsinghua.edu.cn/simple
ARG UV_PYTHON_INSTALL_MIRROR=https://gh-proxy.com/https://github.com/astral-sh/python-build-standalone/releases/download
ENV UV_DEFAULT_INDEX=${UV_DEFAULT_INDEX} \
    UV_INDEX_URL=${UV_DEFAULT_INDEX} \
    UV_PYTHON_INSTALL_MIRROR=${UV_PYTHON_INSTALL_MIRROR}

# Install the gh-proxy curl/wget shims over the system ones. `/usr/local/bin`
# precedes `/usr/bin` on PATH, so `curl`/`wget` resolve to the shims with no
# PATH manipulation — which also leaves `~/.local/bin` as PATH's first entry,
# the directory installers pick when they decide where to drop a binary.
USER root
RUN set -eux; \
    chmod 0755 /usr/local/share/recur-agent-gates/bin/curl \
               /usr/local/share/recur-agent-gates/bin/wget; \
    install -m 0755 /usr/local/share/recur-agent-gates/bin/curl /usr/local/bin/curl; \
    install -m 0755 /usr/local/share/recur-agent-gates/bin/wget /usr/local/bin/wget; \
    command -v curl; command -v wget; \
    curl --version | head -n1; \
    wget --version | head -n1
USER ${TARGET_USER}

# Bun (used by some Flywheel tools / ACFS)
RUN curl -fsSL https://bun.sh/install | bash || true

# uv (Python runner; UBS / MCP Agent Mail / CASS Memory need it). Installed
# directly rather than through ACFS, whose pinned uv-installer checksum is stale.
RUN curl -LsSf https://astral.sh/uv/install.sh | sh || true

# ---------------------------------------------------------------------------
# Flywheel tool chain
# ---------------------------------------------------------------------------
ENV TOOLS_STRATEGY=${TOOLS_STRATEGY} \
    ACFS_PROFILE=${ACFS_PROFILE} \
    ACFS_REF=${ACFS_REF} \
    GH_RAW_MODE=${GH_RAW_MODE} \
    GH_RELEASE_MODE=${GH_RELEASE_MODE}

# Optional: a GitHub token lifts the anonymous REST API limit (60 requests/hour
# per IP) for installers that resolve a release through api.github.com. slb and
# cass cannot install without one. It is mounted as a BuildKit secret, not an
# ARG, so it is never written into the image or the build cache.
#
# Secrets deliberately do not affect the cache key, which would make a
# token-less build shadow a later token build; this marker does affect it, so a
# token build re-runs the layer and a token-less build can reuse a token build's
# result. `required=false` keeps a token-less build working.
ARG GH_TOKEN_PRESENT=0
RUN --mount=type=secret,id=github_token,env=GITHUB_TOKEN,required=false bash /usr/local/share/recur-agent-gates/install-tools.sh

# Gate scripts last: they are what gets edited most often, and this position
# keeps an edit to them from invalidating the tool-chain layer above.
COPY --chown=${TARGET_USER}:${TARGET_USER} scripts/entrypoint.sh scripts/run-quality.sh /usr/local/share/recur-agent-gates/

USER root
RUN install -m 0755 /usr/local/share/recur-agent-gates/entrypoint.sh /usr/local/bin/entrypoint.sh \
 && install -m 0755 /usr/local/share/recur-agent-gates/run-quality.sh /usr/local/bin/run-quality.sh \
 && install -m 0755 /usr/local/share/recur-agent-gates/install-tools.sh /usr/local/bin/install-tools.sh

# ---------------------------------------------------------------------------
# Runtime
# ---------------------------------------------------------------------------
USER ${TARGET_USER}
WORKDIR /data/projects
ENV RCH_DISABLED=1

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["/usr/local/bin/run-quality.sh"]
