# Windows 上 recipe 用哪个 shell。
#
# 这里必须用 `set windows-shell` 而不是 `set shell`，也别写成
# `set shell := ["bash", ...]`：原生 just.exe 按 Windows 的 PATH 规则解析
# `bash`，System32 里的 WSL 启动器（C:\Windows\System32\bash.exe）排在
# Git Bash 之前被命中，于是整条 recipe 被丢进 WSL 执行 —— 用的是 WSL 里的
# cargo（host x86_64-unknown-linux-gnu），`just build-tui` 产出的是 Linux
# ELF 而非 .exe。绝对路径绕开 `bash` 的名字歧义。
#
# `set windows-shell` 只在 Windows 生效；`set shell` 会在所有平台覆盖，
# 把 Linux/macOS 也钉到这个不存在的 C:/ 路径上，所以不用它。
set windows-shell := ["C:/Program Files/Git/bin/bash.exe", "-euo", "pipefail", "-c"]

# Complete interactive build. Default features include the TUI stack.
build-tui:
    cargo build --release --locked --bin ra

# Local release build for a memory-constrained host.
#
# `build-tui` is the shipping configuration: `[profile.release]` is fat LTO
# with a single codegen unit, so it needs a quiet machine. Under load on a
# shared box it dies before it links (`rustc-LLVM ERROR: out of memory`, exit
# 0xc0000409) — see `[profile.release-local]` in Cargo.toml for the mechanism.
# Same shape, thin LTO, and its final LTO step stays small instead of holding
# the whole graph in one module. It lands in `release-local/`, beside
# `release/`, and it is neither what CI ships nor what the 48 MiB size budget
# measures.
#
# `--jobs 6` is not decoration. A profile switch invalidates every dependency
# artifact, so the first run is a full 714-crate rebuild, and that phase — not
# LTO — becomes the peak: measured on this box, six jobs took rustc to 15.6 GiB
# combined and left 0.2 GiB of free physical memory. The default is one job per
# core, 20 here.
build-tui-local:
    cargo build --locked --profile release-local --jobs 6 --bin ra

# RecurCode ACP build. `--no-default-features` is required because Cargo
# features can add dependencies but cannot turn `tui` off.
build-headless:
    cargo build --release --locked --no-default-features --features headless --bin ra-headless

check-headless:
    cargo check --locked --no-default-features --features headless --bin ra-headless

# Print the two release binary paths for packaging scripts.
#
# Ask cargo rather than hardcoding: the target directory is a local choice
# (`.cargo/config.toml` may move it, or drop the setting entirely), and a
# hardcoded path silently kept pointing at a stale copy once the config changed.
paths:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo metadata --format-version 1 --no-deps \
        | python3 -c "import json,os.path,sys; d=json.load(sys.stdin)['target_directory']; [print(os.path.join(d,'release',n)) for n in ('ra','ra-headless')]"
