# ra — build and run recipes.
#
# Feature note: `ra-cli`'s default features include `embed-llama`, which compiles
# llama.cpp from source and therefore needs LLVM's libclang (`LIBCLANG_PATH`).
# The default recipes here pass `--no-default-features --features api,impersonate`
# so they build on a machine without LLVM; the `*-full` recipes use the real
# default set once LLVM is installed.

kernel   := "ra-cli"
bin      := "ra"
features := "api,impersonate"
tui_dir  := "tui"
tui_bin  := "ra-tui"

# list the recipes
default:
    @just --list

# build the ra binary (no llama.cpp embedder — see the header note)
build:
    cargo build -p {{kernel}} --bin {{bin}} --no-default-features --features {{features}}

# build the ra binary with the real default features (needs libclang)
build-full:
    cargo build -p {{kernel}} --bin {{bin}}

# build release binaries for the kernel and the terminal client
release:
    cargo build --release -p {{kernel}} --bin {{bin}} --no-default-features --features {{features}}
    cargo build --release --manifest-path {{tui_dir}}/Cargo.toml --bin {{tui_bin}}

# run ra with arguments: `just run --help`
run *args:
    cargo run -p {{kernel}} --bin {{bin}} --no-default-features --features {{features}} -- {{args}}

# start the local server in solo mode: `just serve --port 50080`
serve *args:
    cargo run -p {{kernel}} --bin {{bin}} --no-default-features --features {{features}} -- serve --solo {{args}}

# environment / backend / protocol diagnosis
doctor:
    cargo run -p {{kernel}} --bin {{bin}} --no-default-features --features {{features}} -- doctor

# print the version
version:
    cargo run -p {{kernel}} --bin {{bin}} --no-default-features --features {{features}} -- --version

# build the terminal client
tui-build:
    cargo build --manifest-path {{tui_dir}}/Cargo.toml --bin {{tui_bin}}

# run the terminal client (spawns `ra serve --stdio` by default)
tui-run *args:
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
