//! Headless ACP entry point for RecurCode.
//!
//! This binary deliberately does not depend on the interactive CLI crate. The
//! `headless` feature leaves the TUI feature disabled, so Cargo does not build
//! the interactive modules or their optional terminal dependencies.

#![forbid(unsafe_code)]
#![recursion_limit = "256"]

use anyhow::Result;
use asupersync::runtime::reactor::create_reactor;
use asupersync::runtime::RuntimeBuilder;
use ra::acp::{AcpOptions, run_stdio};
use ra::auth::AuthStorage;
use ra::config::Config;
use ra::models::{ModelRegistry, default_models_path};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    let reactor = create_reactor()?;
    let runtime = RuntimeBuilder::multi_thread()
        .blocking_threads(1, 2)
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
