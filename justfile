set shell := ["bash", "-euo", "pipefail", "-c"]

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
