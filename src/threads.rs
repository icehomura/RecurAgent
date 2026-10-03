//! Thread spawning with an explicit stack reserve.
//!
//! `std::thread::spawn` uses the platform default stack: the PE `/STACK`
//! reserve on Windows, `ulimit -s` elsewhere, and only `RUST_MIN_STACK` can
//! raise it. A shipped binary does not get this crate's `.cargo/config.toml`
//! `[env]` entry, and `#![forbid(unsafe_code)]` forbids setting the variable
//! in-process, so the default is what a released thread actually gets.
//!
//! That default is too small for this tree. The agent drives a deeply nested
//! asupersync future whose poll stack is large and varies per run; a thread
//! that polls it on a default stack can abort the whole process with
//! `STATUS_STACK_OVERFLOW` (`0xC00000FD`) or `SIGSEGV`. A stack overflow is
//! fail-fast — no unwinding, no `Drop`, no session flush — so the cost is not
//! an error result but the entire conversation.
//!
//! Route every thread through here so the reserve is explicit and uniform
//! rather than a property of the linker configuration. The reservation is
//! virtual and committed lazily, so a thread that never needs the depth pays
//! address space, not resident memory.

/// Stack reserve given to every thread spawned through this module.
///
/// Threads here can poll the same agent/provider/tool future chain as
/// `main::MAIN_STACK_BYTES` and `interactive_ftui::DRIVER_STACK_BYTES`, so it
/// takes the same 16 MiB, on the same measured basis: a complete offline agent
/// turn — including a `dag` whose node runs `run_code` (dag_tool + ptc_bridge +
/// QuickJS) — holds at 724992 B (708 KiB) and aborts at 720896 B (704 KiB),
/// flat to DAG N=256 (probe `agent::tests::probe_full_turn_stack_scaling`,
/// commit `905433d68`). That probe uses a mock provider, so real transport
/// (TLS/HTTP) and session persistence (sqlite/JSONL) are unexercised
/// (`-Zprint-type-sizes` puts those frames at <= ~30 KiB each); 16 MiB keeps
/// ~23x headroom over the floor. Bead `bd-qtffv` tracks verifying the
/// transport-heavy chains and going lower.
pub const AGENT_STACK_BYTES: usize = 16 * 1024 * 1024;

/// [`std::thread::spawn`] with [`AGENT_STACK_BYTES`].
///
/// A drop-in replacement: same argument, same return type, and it panics on a
/// spawn failure exactly as `std::thread::spawn` does. Use it instead of
/// `std::thread::spawn` everywhere in this crate.
///
/// Not `#[must_use]`, matching [`std::thread::spawn`]: dropping the handle
/// detaches the thread, which several call sites rely on.
///
/// # Panics
///
/// Panics if the operating system cannot create the thread, matching
/// [`std::thread::spawn`].
pub fn spawn<F, T>(f: F) -> std::thread::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    std::thread::Builder::new()
        .stack_size(AGENT_STACK_BYTES)
        .spawn(f)
        .expect("the OS must be able to create a thread with the reserved stack")
}

/// [`spawn`] with a name attached, for `thread::Builder::name` diagnostics.
///
/// # Panics
///
/// Panics if the operating system cannot create the thread, matching
/// [`std::thread::Builder::spawn`]'s expectation at call sites that unwrap.
pub fn spawn_named<F, T>(name: &str, f: F) -> std::thread::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    std::thread::Builder::new()
        .name(name.to_string())
        .stack_size(AGENT_STACK_BYTES)
        .spawn(f)
        .expect("the OS must be able to create a thread with the reserved stack")
}
