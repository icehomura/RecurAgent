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
paths:
    @printf '%s\n' "D:/cargo-target/pi_agent/release/ra" "D:/cargo-target/pi_agent/release/ra-headless"
