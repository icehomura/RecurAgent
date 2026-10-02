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
# This recipe builds the same shape with thin LTO: no OOM, links in a fraction
# of the time, but the binary is not the one CI ships, nor the one the 48 MiB
# size budget is measured on.
build-tui-local:
    cargo build --locked --profile release-local --bin ra

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
