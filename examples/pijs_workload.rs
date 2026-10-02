//! `PiJS` workload harness for deterministic perf baselines.
//!
//! Entry point only. The harness body lives in `pijs_workload/harness.rs`, and
//! `benches/pijs_workload.rs` is a second entry point over that same file:
//! the perf lane builds this example (`target/<profile>/examples/pijs_workload`)
//! while the criterion lane runs `cargo bench --bench pijs_workload`, and both
//! must measure one implementation. Cargo warns when a single path serves two
//! targets, so the two entry points have to be distinct files, and `include!`
//! cannot supply the shared body because crate-level inner attributes are not
//! allowed in an included file.
#![recursion_limit = "256"]
#![forbid(unsafe_code)]

#[path = "pijs_workload/harness.rs"]
mod harness;

fn main() {
    harness::main();
}
