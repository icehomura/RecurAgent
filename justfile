set shell := ["bash", "-euo", "pipefail", "-c"]

# Complete interactive build. Default features include the TUI stack.
build-tui:
    cargo build --release --locked --bin ra

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
