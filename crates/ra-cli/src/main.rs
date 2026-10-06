//! RecurAgent CLI entry point.

use clap::{CommandFactory, FromArgMatches};
use color_eyre::eyre::Result;

#[cfg_attr(not(feature = "api"), allow(unused_imports))]
use ra_cli::commands::{self, Args, Executable};

/// Interactive = at least one of stdout/stderr is a TTY. When running as a
/// launchd daemon both are redirected to /dev/null, so this returns false and
/// log init drops the console layer in favour of the rolling file logger —
/// giving the service "one primary logging path" instead of duplicated sinks.
fn is_interactive_terminal() -> bool {
    use std::io::IsTerminal as _;

    std::io::stdout().is_terminal() || std::io::stderr().is_terminal()
}

/// Enable the console tracing layer only when the invocation is interactive
/// (dev/debug) OR when no rolling-file sink is configured (fallback so logs
/// don't vanish entirely).
fn should_enable_console_logs(has_rolling_file_logs: bool, interactive: bool) -> bool {
    !has_rolling_file_logs || interactive
}

/// Shell-completion request variable (new name) — see [`complete_env_var`].
const COMPLETE_VAR: &str = "RA_COMPLETE";
/// Pre-rename shell-completion request variable. Kept as a literal because
/// `clap_complete` is handed the *name*, not a value;
/// [`complete_var_names_follow_the_brand_prefixes`] pins both spellings to the
/// brand prefixes.
const LEGACY_COMPLETE_VAR: &str = "RA_COMPLETE";

/// Name of the shell-completion request variable: `RA_COMPLETE`, falling back to
/// the legacy `RA_COMPLETE` when only that one is set (the new name wins when
/// both are). `clap_complete` reads the variable itself and wants a
/// `&'static str`, so the two candidate names are consts.
fn complete_env_var() -> &'static str {
    if std::env::var_os(COMPLETE_VAR).is_some_and(|value| !value.is_empty()) {
        COMPLETE_VAR
    } else {
        LEGACY_COMPLETE_VAR
    }
}

/// Write a panic report to any `io::Write` without panicking on BrokenPipe.
///
/// Extracted as a standalone function so unit tests can inject writers that
/// return `ErrorKind::BrokenPipe` or other I/O errors.
pub fn write_panic_report(w: &mut impl std::io::Write, msg: &str) {
    match w
        .write_all(msg.as_bytes())
        .and_then(|()| w.write_all(b"\n"))
    {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
            // Observer left — nothing to report to, just continue.
        }
        Err(_) => {
            // Other I/O error — nothing more we can do.
        }
    }
}

/// Install the production error hooks: color-eyre EyreHook plus a panic
/// hook whose stderr write goes through [`write_panic_report`] (fallible,
/// BrokenPipe-swallowing). Shared by `main` and the `--test-panic` harness
/// entry so integration tests exercise the REAL production hook path.
pub fn install_error_hooks() -> color_eyre::Result<()> {
    let (panic_hook, eyre_hook) = color_eyre::config::HookBuilder::default().into_hooks();
    eyre_hook.install()?;
    std::panic::set_hook(Box::new(move |pi| {
        let report = panic_hook.panic_report(pi);
        let msg = format!("{report}");
        let mut err = std::io::stderr().lock();
        write_panic_report(&mut err, &msg);
    }));
    Ok(())
}

fn main() -> Result<()> {
    // Windows gives the process's main thread a 1 MB stack (the PE default) where
    // unix gives 8 MB, and the serve/agent call chains recurse past 1 MB: `serve
    // --stdio` died with "thread 'main' has overflowed its stack" before it could
    // answer its first JSON-RPC frame, which broke the terminal client's default
    // local launch (it spawns exactly that command). Windows therefore runs the
    // real entry point on a thread with a generous stack.
    //
    // unix must NOT: `ra acp --host-managed` has to call
    // `ra_sandbox::confine_host_managed` from the process's INITIAL thread (the
    // sandbox rejects a spawned caller), and unix already gives the main thread
    // 8 MB — the same budget the Windows fix exists to match.
    #[cfg(windows)]
    {
        const MAIN_STACK_BYTES: usize = 32 * 1024 * 1024;
        std::thread::Builder::new()
            .name("ra-main".to_owned())
            .stack_size(MAIN_STACK_BYTES)
            .spawn(run_cli)
            .map_err(|error| eyre::eyre!("failed to start the main worker thread: {error}"))?
            .join()
            .unwrap_or_else(|_panic| std::process::exit(101))
    }
    #[cfg(not(windows))]
    {
        run_cli()
    }
}

fn run_cli() -> Result<()> {
    // Captured once: when the invocation turns out to be the terminal UI (no
    // subcommand), the raw argv is forwarded to `ra_tui::run_with` — the TUI's
    // launch flags (`--mode`, `--endpoint`, `--stdio-command`, `--no-splash`,
    // …) belong to ITS parser, not to this one.
    let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();

    // Answer shell completion requests before argument parsing (#2413): with
    // `RA_COMPLETE=<shell>` set the shell sources the registration script and
    // calls back into this binary on every tab; without it this is a no-op. The
    // var is namespaced — a generic `COMPLETE` exported for some other tool
    // must not brick every RecurAgent invocation. Must precede parsing —
    // mid-edit arguments don't parse cleanly.
    clap_complete::CompleteEnv::with_factory(<Args as clap::CommandFactory>::command)
        .var(COMPLETE_VAR)
        .complete();

    install_error_hooks()?;

    // Hidden chaos-test switch (outer-loop blueprint step ②): when
    // RA_TEST_PANIC_AFTER_BOOT=1, panic immediately AFTER the production
    // hooks are installed. Integration tests use this to drive the REAL
    // production panic-hook path under a broken-pipe stderr and assert no
    // second panic / no SIGABRT. Never set in normal operation.
    //
    // It stays ahead of the TUI dispatch below: the integration test drives a
    // bare `ra` (no subcommand), i.e. exactly the invocation that becomes the
    // terminal UI.
    if ra_core::brand::env_compat_str("TEST_PANIC_AFTER_BOOT").as_deref() == Some("1") {
        panic!("__test_panic__: intentional panic for production hook verification");
    }

    // Parse into ArgMatches first (this preserves clap's --help/--version/error
    // handling exactly as `Args::parse()` did), materialize the typed Args, then
    // merge the layered `cli.<cmd>` startup defaults BEFORE any downstream reads
    // of the subcommand. Precedence: explicit CLI flag > env var > config.json
    // `cli.<cmd>` > built-in default (see `ra_cli::config_layer`).
    let matches = match Args::command().try_get_matches_from(argv.iter().cloned()) {
        Ok(matches) => matches,
        Err(error) => {
            // An invocation this parser rejects outright and that names no
            // subcommand is not a ra-cli command line — it is the TUI's own
            // flag set (`ra --endpoint …`, `ra --no-splash`), so the raw argv
            // goes to the TUI's parser. Everything else (help/version, an
            // invalid subcommand, a bad flag inside a real subcommand) keeps
            // clap's message and exit code.
            #[cfg(feature = "tui")]
            if tui_owns_invocation(&error, &argv) {
                return ra_tui::run_with(argv);
            }
            error.exit();
        }
    };
    let mut args = Args::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());

    // `ra` with NO subcommand starts the terminal UI, in-process: the TUI is
    // what the bare command does, never a subcommand of its own. In a build
    // without the `tui` feature there is no UI to start, so a bare `ra` keeps
    // clap's missing-subcommand contract (exit code 2).
    if args.command.is_none() {
        #[cfg(feature = "tui")]
        {
            return ra_tui::run_with(argv);
        }
        #[cfg(not(feature = "tui"))]
        {
            Args::command()
                .error(
                    clap::error::ErrorKind::MissingSubcommand,
                    "'ra' requires a subcommand but one was not provided",
                )
                .exit();
        }
    }

    // Protected children must enter confinement before config, credentials,
    // tracing workers or a Tokio runtime can acquire ambient authority.
    if matches!(&args.command, Some(commands::Command::Acp(command)) if command.host_managed) {
        return args
            .command
            .take()
            .expect("the match above proved a subcommand is present")
            .execute();
    }
    ra_cli::config_layer::apply(&mut args, &matches)?;

    // Determine log directory for serve command (enables rolling file logs)
    #[allow(unused_mut)]
    let mut log_dir: Option<std::path::PathBuf> = None;
    #[cfg(feature = "api")]
    if let Some(commands::Command::Serve(cmd)) = &args.command {
        let data_dir = commands::resolve_data_dir(cmd.data_dir.clone())?;
        let dir = data_dir.join("logs");
        std::fs::create_dir_all(&dir).ok();
        log_dir = Some(dir);
    }

    // Initialize tracing (with optional rolling file output for serve). Some
    // commands emit a machine-readable stream on STDOUT (ACP/MCP JSON-RPC,
    // `profile` payloads, `chat --json`) and must keep it pure — one stray log
    // line corrupts it — so their console logs are routed to stderr. See
    // [`commands::reserve_stdout`] for the exact set.
    let reserve_stdout = commands::reserve_stdout(args.command.as_ref());
    let _log_guard = init_tracing(log_dir.as_deref(), reserve_stdout)?;

    args.command
        .take()
        .expect("the no-subcommand case returned above")
        .execute()
}

/// Whether a clap parse failure means "this invocation belongs to the terminal
/// UI" rather than "this is a malformed `ra` command line".
///
/// `ra` with no subcommand IS the TUI, and its launch flags are the TUI's own,
/// so the outer parser cannot judge them — it rejects the whole line before the
/// TUI sees it. The subcommand slot is the first token after the program name,
/// which is exactly what this parser looks at, so:
///
/// * help/version stay with clap — `ra --help` must list the commands;
/// * a real `ra` subcommand keeps clap's error, however malformed the rest of
///   the line is (`ra chat --bogus` must not become a TUI flag error);
/// * a leading flag (`ra --endpoint …`, `ra --no-splash`) or a subcommand the
///   TUI's own dispatch routes (`ra olp-mcp-serve`, `ra outer-duty`) is handed
///   to the TUI, which parses it and reports any genuinely unknown flag itself;
/// * anything else — a mistyped subcommand — keeps clap's message, which
///   suggests the intended command.
#[cfg(feature = "tui")]
fn tui_owns_invocation(error: &clap::Error, argv: &[std::ffi::OsString]) -> bool {
    if matches!(
        error.kind(),
        clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
    ) {
        return false;
    }
    let Some(first) = argv.get(1).and_then(|arg| arg.to_str()) else {
        return false;
    };
    if Args::command()
        .get_subcommands()
        .any(|command| command.get_name() == first)
    {
        return false;
    }
    first.starts_with('-') || ra_tui::cmd::SUBCOMMANDS.contains(&first)
}

/// Initialize tracing with console output and optional rolling file output.
///
/// When `log_dir` is `Some`, logs are also written to daily-rotated files
/// under that directory (e.g. `~/.ra/logs/serve.2026-03-09.log`), keeping
/// the last 7 days.  The returned guard must be held for the program lifetime.
fn init_tracing(
    log_dir: Option<&std::path::Path>,
    reserve_stdout: bool,
) -> Result<Option<tracing_appender::non_blocking::WorkerGuard>> {
    use std::io::IsTerminal as _;
    use tracing_subscriber::fmt::writer::{BoxMakeWriter, MakeWriterExt};
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"))
        // Suppress noisy HTML5 parser warnings ("foster parenting not implemented")
        .add_directive("html5ever=error".parse().unwrap());

    // Check if JSON format is requested via environment
    let json_logs = ra_core::brand::env_compat_str("LOG_JSON").is_some();
    let has_rolling_file_logs = log_dir.is_some();
    let console_enabled =
        should_enable_console_logs(has_rolling_file_logs, is_interactive_terminal());

    // Console layer routing: when the operator has a rolling-file sink AND is
    // not running interactively (is_terminal() == false), suppress stderr to
    // avoid the launchd StandardErrorPath double-capturing what the rolling
    // appender is already persisting. When interactive, keep stderr alive so
    // `ra chat`/debugging still prints.
    let enable_console = console_enabled && (log_dir.is_none() || std::io::stderr().is_terminal());
    let _unused_has_rolling = has_rolling_file_logs; // retained for future use

    if let Some(dir) = log_dir {
        // Rolling daily log file, keep last 7 days
        let file_appender = tracing_appender::rolling::RollingFileAppender::builder()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix("serve")
            .filename_suffix("log")
            .max_log_files(7)
            .build(dir)
            .map_err(|e| eyre::eyre!("failed to create log file appender: {e}"))?;

        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
        let writer = if enable_console {
            BoxMakeWriter::new(std::io::stderr.and(non_blocking))
        } else {
            BoxMakeWriter::new(non_blocking)
        };

        if json_logs {
            tracing_subscriber::registry()
                .with(
                    fmt::layer()
                        .json()
                        .with_target(true)
                        .with_span_list(true)
                        .with_current_span(true)
                        .with_writer(writer),
                )
                .with(filter)
                .init();
        } else {
            tracing_subscriber::registry()
                .with(
                    fmt::layer()
                        .with_ansi(false)
                        .with_target(false)
                        .compact()
                        .with_writer(writer),
                )
                .with(filter)
                .init();
        }

        Ok(Some(guard))
    } else {
        // `ra acp` reserves stdout for the JSON-RPC protocol → its logs go to
        // stderr. Every other no-log-dir command keeps the historical stdout.
        let writer: BoxMakeWriter = if reserve_stdout {
            BoxMakeWriter::new(std::io::stderr)
        } else {
            BoxMakeWriter::new(std::io::stdout)
        };
        if json_logs {
            tracing_subscriber::registry()
                .with(
                    fmt::layer()
                        .json()
                        .with_target(true)
                        .with_span_list(true)
                        .with_current_span(true)
                        .with_writer(writer),
                )
                .with(filter)
                .init();
        } else {
            tracing_subscriber::registry()
                .with(
                    fmt::layer()
                        .with_target(false)
                        .with_thread_ids(false)
                        .compact()
                        .with_writer(writer),
                )
                .with(filter)
                .init();
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct BrokenPipeWriter;

    impl std::io::Write for BrokenPipeWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "pipe closed",
            ))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn write_panic_report_broken_pipe_no_panic() {
        let mut w = BrokenPipeWriter;
        write_panic_report(&mut w, "panic message");
        // If we reach here, no panic occurred — test passes.
    }

    #[test]
    fn write_panic_report_normal_writer_outputs() {
        let mut buf: Vec<u8> = Vec::new();
        write_panic_report(&mut buf, "panic message");
        assert!(!buf.is_empty());
        assert!(buf.ends_with(b"\n"));
    }

    #[test]
    fn interactive_tty_with_file_logs_still_gets_console() {
        assert!(should_enable_console_logs(true, true));
    }

    #[test]
    fn daemon_with_file_logs_drops_console() {
        assert!(!should_enable_console_logs(true, false));
    }

    #[test]
    fn daemon_without_file_logs_falls_back_to_console() {
        assert!(should_enable_console_logs(false, false));
    }

    #[test]
    fn interactive_without_file_logs_gets_console() {
        assert!(should_enable_console_logs(false, true));
    }

    /// The completion variable names must stay tied to the brand prefixes
    /// (they are literals only because `clap_complete` wants `&'static str`).
    #[test]
    fn complete_var_names_follow_the_brand_prefixes() {
        assert_eq!(
            COMPLETE_VAR,
            format!("{}COMPLETE", ra_core::brand::ENV_PREFIX)
        );
        assert_eq!(
            LEGACY_COMPLETE_VAR,
            format!("{}COMPLETE", ra_core::brand::ENV_PREFIX)
        );
    }
}
