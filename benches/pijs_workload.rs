//! Criterion-facing entry point for the `pijs_workload` harness.
//!
//! Shares `examples/pijs_workload/harness.rs` with the example target, so
//! `cargo bench --bench pijs_workload` and
//! `cargo build --example pijs_workload` run identical logic through two
//! distinct files (Cargo warns when one path serves two targets). `harness =
//! false` in the manifest makes this file the benchmark's `main`.
#![recursion_limit = "256"]
#![forbid(unsafe_code)]

#[path = "../examples/pijs_workload/harness.rs"]
mod harness;

fn main() {
    harness::main();
}
