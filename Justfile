# ra — build and run recipes.
#
# Three ways to build the product:
#
#   ra-headless : kernel only — no TUI, no web UI, no local llama.cpp
#                 (`--no-default-features --features api`)
#   ra          : kernel + TUI with local llama.cpp embeddings               ← `just build`
#                 (crate default features), no web UI
#   ra-full    : `ra` plus the embedded web UI (`/admin/`; the `/app/` client is
#                 not shipped — build your own into crates/ra-cli/static/web/)
#
# Every variant is copied into `dist/<variant>/` so the three can coexist.
#
# The `*-full` recipes build the dashboard before cargo (rust-embed bakes
# `crates/ra-cli/static/admin/` in at compile time) and therefore need node/npm.
# Local llama.cpp embeddings need LLVM's libclang and cmake:
#     scoop install llvm cmake
#     setx LIBCLANG_PATH "%USERPROFILE%\scoop\apps\llvm\current\bin"

# `just` cannot find a shell on Windows when Git Bash is not on PATH (its own
# default is `sh`). Git Bash is the shell this repo is developed and tested with
# here; unix hosts keep the plain `bash` default.
set shell := ["bash", "-cu"]
set windows-shell := ["C:/Program Files/Git/bin/bash.exe", "-cu"]

kernel  := "ra-cli"
bin     := "ra"
tui_dir := "tui"
tui_bin := "ra-tui"
dist    := "dist"
target  := env_var_or_default("CARGO_TARGET_DIR", "target")
exe     := if os() == "windows" { ".exe" } else { "" }

# list the recipes
default:
    @just --list

# kernel with the crate's default features (api + embed-llama + impersonate)
_kernel profile:
    cargo build --profile {{profile}} -p {{kernel}} --bin {{bin}}

# kernel without the local llama.cpp embedder or the impersonating research engines
_kernel_headless profile:
    cargo build --profile {{profile}} -p {{kernel}} --bin {{bin}} --no-default-features --features api

# terminal client
_tui profile:
    cargo build --profile {{profile}} --manifest-path {{tui_dir}}/Cargo.toml --bin {{tui_bin}}

# the embedded web UI (dashboard)
_dashboard:
    ./scripts/build-dashboard.sh

# ---- ra (default: kernel + TUI, local llama.cpp, no web UI) ----------------

# build `ra` (release)
build: (_kernel "release") (_tui "release")
    @mkdir -p {{dist}}/ra
    cp "{{target}}/release/{{bin}}{{exe}}" "{{dist}}/ra/"
    cp "{{target}}/release/{{tui_bin}}{{exe}}" "{{dist}}/ra/"

# build `ra` (debug)
build-debug: (_kernel "dev") (_tui "dev")
    @mkdir -p {{dist}}/ra
    cp "{{target}}/dev/{{bin}}{{exe}}" "{{dist}}/ra/"
    cp "{{target}}/dev/{{tui_bin}}{{exe}}" "{{dist}}/ra/"

# ---- ra-headless (kernel only, no TUI / web UI / llama.cpp) ----------------

# build `ra-headless` (release)
build-headless: (_kernel_headless "release")
    @mkdir -p {{dist}}/ra-headless
    cp "{{target}}/release/{{bin}}{{exe}}" "{{dist}}/ra-headless/"

# build `ra-headless` (debug)
build-headless-debug: (_kernel_headless "dev")
    @mkdir -p {{dist}}/ra-headless
    cp "{{target}}/dev/{{bin}}{{exe}}" "{{dist}}/ra-headless/"

# ---- ra-full (ra + embedded web UI) ---------------------------------------

# build `ra-full` (release)
build-full: _dashboard (_kernel "release") (_tui "release")
    @mkdir -p {{dist}}/ra-full
    cp "{{target}}/release/{{bin}}{{exe}}" "{{dist}}/ra-full/"
    cp "{{target}}/release/{{tui_bin}}{{exe}}" "{{dist}}/ra-full/"

# build `ra-full` (debug)
build-full-debug: _dashboard (_kernel "dev") (_tui "dev")
    @mkdir -p {{dist}}/ra-full
    cp "{{target}}/dev/{{bin}}{{exe}}" "{{dist}}/ra-full/"
    cp "{{target}}/dev/{{tui_bin}}{{exe}}" "{{dist}}/ra-full/"

# all three variants (release)
release: build build-headless build-full

# ---- the TUI on its own ---------------------------------------------------

# build the terminal client (release)
tui-build: (_tui "release")

# build the terminal client (debug)
tui-build-debug: (_tui "dev")

# run the terminal client (spawns `ra serve --stdio` by default)
tui-run *args:
    cargo run --release --manifest-path {{tui_dir}}/Cargo.toml --bin {{tui_bin}} -- {{args}}

# run the terminal client (debug)
tui-run-debug *args:
    cargo run --manifest-path {{tui_dir}}/Cargo.toml --bin {{tui_bin}} -- {{args}}

# ---- run / inspect --------------------------------------------------------

# run ra with arguments: `just run --help`
run *args:
    cargo run --release -p {{kernel}} --bin {{bin}} -- {{args}}

# run ra (debug)
run-debug *args:
    cargo run -p {{kernel}} --bin {{bin}} -- {{args}}

# start the local server in solo mode: `just serve --port 50080`
serve *args:
    cargo run --release -p {{kernel}} --bin {{bin}} -- serve --solo {{args}}

# start the local server (debug)
serve-debug *args:
    cargo run -p {{kernel}} --bin {{bin}} -- serve --solo {{args}}

# environment / backend / protocol diagnosis
doctor:
    cargo run --release -p {{kernel}} --bin {{bin}} -- doctor

# environment / backend / protocol diagnosis (debug)
doctor-debug:
    cargo run -p {{kernel}} --bin {{bin}} -- doctor

# print the version
version:
    cargo run --release -p {{kernel}} --bin {{bin}} -- --version

# print the version (debug)
version-debug:
    cargo run -p {{kernel}} --bin {{bin}} -- --version

# ---- housekeeping ---------------------------------------------------------

# type-check the kernel closure, tests included
check:
    cargo check -p {{kernel}} --all-targets --no-default-features --features api,impersonate

# unit tests for the runtime crates
test:
    cargo test -p ra-core -p ra-agent -p ra-llm -p ra-memory -p ra-pipeline -p ra-store -p ra-services --no-default-features

# unit tests for the CLI
test-cli:
    cargo test -p {{kernel}} --lib --no-default-features --features api,impersonate

# the terminal client's test suite
test-tui:
    cargo test --manifest-path {{tui_dir}}/Cargo.toml --no-fail-fast

# format both workspaces
fmt:
    cargo fmt --all
    cargo fmt --manifest-path {{tui_dir}}/Cargo.toml

# lint the kernel closure
clippy:
    cargo clippy -p {{kernel}} --all-targets --no-default-features --features api,impersonate

# remove build output for both workspaces plus the collected variants
clean:
    cargo clean
    cargo clean --manifest-path {{tui_dir}}/Cargo.toml
    rm -rf {{dist}}
