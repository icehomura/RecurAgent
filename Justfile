# ra — build and run recipes.
#
# `build` is a RELEASE build; `build-debug` is the debug one. Both use
# `--no-default-features --features api,impersonate` because `ra-cli`'s default
# features include `embed-llama`, which compiles llama.cpp and therefore needs
# LLVM's libclang (`LIBCLANG_PATH`) plus cmake; the `*-full` recipes use that
# default set. Install the prerequisites with:
#     scoop install llvm cmake
#     setx LIBCLANG_PATH "%USERPROFILE%\scoop\apps\llvm\current\bin"

# `just` cannot find a shell on Windows when Git Bash is not on PATH (its own
# default is `sh`). Git Bash is the shell this repo is developed and tested with
# here; unix hosts keep the plain `bash` default.
set shell := ["bash", "-cu"]
set windows-shell := ["C:/Program Files/Git/bin/bash.exe", "-cu"]

kernel   := "ra-cli"
bin      := "ra"
features := "api,impersonate"
tui_dir  := "tui"
tui_bin  := "ra-tui"

# list the recipes
default:
    @just --list

# build the ra binary (release)
build:
    cargo build --release -p {{kernel}} --bin {{bin}} --no-default-features --features {{features}}

# build the ra binary (debug)
build-debug:
    cargo build -p {{kernel}} --bin {{bin}} --no-default-features --features {{features}}

# build the ra binary (release, default features — needs libclang)
build-full:
    cargo build --release -p {{kernel}} --bin {{bin}}

# build the ra binary (debug, default features — needs libclang)
build-debug-full:
    cargo build -p {{kernel}} --bin {{bin}}

# build the terminal client (release)
tui-build:
    cargo build --release --manifest-path {{tui_dir}}/Cargo.toml --bin {{tui_bin}}

# build the terminal client (debug)
tui-build-debug:
    cargo build --manifest-path {{tui_dir}}/Cargo.toml --bin {{tui_bin}}

# build both binaries (release)
release: build tui-build

# run ra with arguments: `just run --help`
run *args:
    cargo run --release -p {{kernel}} --bin {{bin}} --no-default-features --features {{features}} -- {{args}}

# run ra (debug)
run-debug *args:
    cargo run -p {{kernel}} --bin {{bin}} --no-default-features --features {{features}} -- {{args}}

# start the local server in solo mode: `just serve --port 50080`
serve *args:
    cargo run --release -p {{kernel}} --bin {{bin}} --no-default-features --features {{features}} -- serve --solo {{args}}

# start the local server (debug)
serve-debug *args:
    cargo run -p {{kernel}} --bin {{bin}} --no-default-features --features {{features}} -- serve --solo {{args}}

# environment / backend / protocol diagnosis
doctor:
    cargo run --release -p {{kernel}} --bin {{bin}} --no-default-features --features {{features}} -- doctor

# environment / backend / protocol diagnosis (debug)
doctor-debug:
    cargo run -p {{kernel}} --bin {{bin}} --no-default-features --features {{features}} -- doctor

# print the version
version:
    cargo run --release -p {{kernel}} --bin {{bin}} --no-default-features --features {{features}} -- --version

# print the version (debug)
version-debug:
    cargo run -p {{kernel}} --bin {{bin}} --no-default-features --features {{features}} -- --version

# run the terminal client (spawns `ra serve --stdio` by default)
tui-run *args:
    cargo run --release --manifest-path {{tui_dir}}/Cargo.toml --bin {{tui_bin}} -- {{args}}

# run the terminal client (debug)
tui-run-debug *args:
    cargo run --manifest-path {{tui_dir}}/Cargo.toml --bin {{tui_bin}} -- {{args}}

# type-check the kernel closure, tests included
check:
    cargo check -p {{kernel}} --all-targets --no-default-features --features {{features}}

# unit tests for the runtime crates
test:
    cargo test -p ra-core -p ra-agent -p ra-llm -p ra-memory -p ra-pipeline -p ra-store -p ra-services --no-default-features

# unit tests for the CLI
test-cli:
    cargo test -p {{kernel}} --lib --no-default-features --features {{features}}

# the terminal client's test suite
test-tui:
    cargo test --manifest-path {{tui_dir}}/Cargo.toml --no-fail-fast

# format both workspaces
fmt:
    cargo fmt --all
    cargo fmt --manifest-path {{tui_dir}}/Cargo.toml

# lint the kernel closure
clippy:
    cargo clippy -p {{kernel}} --all-targets --no-default-features --features {{features}}

# remove build output for both workspaces
clean:
    cargo clean
    cargo clean --manifest-path {{tui_dir}}/Cargo.toml
