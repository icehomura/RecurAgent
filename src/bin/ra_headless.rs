//! Headless ACP entry point for RecurCode.
//!
//! This binary deliberately does not depend on the interactive CLI crate. The
//! `headless` feature leaves the TUI feature disabled, so Cargo does not build
//! the interactive modules or their optional terminal dependencies.

#![forbid(unsafe_code)]
#![recursion_limit = "256"]

use anyhow::Result;
use asupersync::runtime::RuntimeBuilder;
use asupersync::runtime::reactor::create_reactor;
use ra::acp::{AcpOptions, run_stdio};
use ra::auth::AuthStorage;
use ra::config::Config;
use ra::models::{ModelRegistry, default_models_path};

/// Stack reserve for every worker thread the runtime spawns.
///
/// A stack overflow is a fail-fast abort on Windows (`STATUS_STACK_OVERFLOW`,
/// `0xC00000FD`): no unwinding, no `Drop`, no session flush. Sized from the
/// same measurement as `ra`'s `MAIN_STACK_BYTES`: a complete offline
/// agent turn — including a `dag` whose node runs `run_code` (dag_tool +
/// ptc_bridge + QuickJS) — holds at 724992 B (708 KiB) and aborts at 720896 B
/// (704 KiB), flat to DAG N=256 (probe
/// `agent::tests::probe_full_turn_stack_scaling`, commit `905433d68`). That
/// probe uses a mock provider, so real transport (TLS/HTTP) and session
/// persistence (sqlite/JSONL) are unexercised (`-Zprint-type-sizes` puts those
/// frames at <= ~30 KiB each); 16 MiB keeps ~23x headroom over the floor.
/// Bead `bd-qtffv` tracks verifying the transport-heavy chains and going lower.
const HEADLESS_WORKER_STACK_BYTES: usize = 16 * 1024 * 1024;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    let reactor = create_reactor()?;
    let runtime = RuntimeBuilder::multi_thread()
        .blocking_threads(1, 2)
        // Worker threads poll the same deeply nested agent/provider futures the
        // `ra` binary drives. Size them explicitly: unsized they inherit the PE
        // default, and a deep future then aborts with `STATUS_STACK_OVERFLOW`
        // (fail-fast, no unwinding, no session flush). Same 16 MiB as `ra`'s
        // `MAIN_STACK_BYTES`/`RUNTIME_WORKER_STACK_BYTES`; see
        // `HEADLESS_WORKER_STACK_BYTES` above and bead `bd-qtffv`.
        .thread_stack_size(HEADLESS_WORKER_STACK_BYTES)
        .with_reactor(reactor)
        .build()
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let handle = runtime.handle();

    let config = Config::load()?;
    let auth = AuthStorage::load(Config::auth_path())?;
    let models_path = default_models_path(&Config::global_dir());
    let model_registry = ModelRegistry::load(&auth, Some(models_path));
    let available_models = model_registry.get_available();

    let options = AcpOptions {
        config,
        available_models,
        model_registry,
        auth,
        runtime_handle: handle,
        session_dir: Some(Config::sessions_dir()),
    };

    let result = runtime.block_on(run_stdio(options));
    ra::jobs::kill_all();
    ra::hub::kill_session_services();
    result.map_err(anyhow::Error::from)
}
