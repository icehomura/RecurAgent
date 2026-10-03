//! M6 PTC (Programmatic Tool Calling): the `run_code` tool.
//!
//! The model writes one JS program; it runs on an **in-process QuickJS realm**
//! (`rquickjs`) with a Rust-injected `sdk` object, and orchestrates multiple
//! tool calls in a single round-trip. Intermediate data stays in the realm;
//! only the program's final value returns to the model.
//!
//! There is no Node.js/Bun dependency and no embedded JS asset to keep in
//! sync: the engine is already linked in for extensions
//! ([`crate::extensions_js`]) and `eval` ([`crate::eval`]), and every `sdk.*`
//! binding below is a Rust closure. A run spawns no child process and writes
//! nothing to disk.
//!
//! # SDK surface (what program code may call)
//!
//! The program body receives `sdk` and a `console`. Every helper forwards to a
//! host tool through the bridge; the host reply is the tool's rendered text, or
//! a thrown error carrying the tool's error text.
//!
//! Each helper accepts the positional shorthand **or** an options object, and
//! the object form forwards every key to the host tool (so `offset`, `limit`,
//! `hashline`, `encoding`, `glob`, `context`, ... all take effect). Every
//! helper returns an already-settled `Promise`, so `await` it:
//!
//! | Call | Host tool |
//! |------|-----------|
//! | `sdk.read(path)` / `sdk.read({ path, offset, limit, hashline, encoding })` | `read` |
//! | `sdk.grep(pattern, path?)` / `sdk.grep({ pattern, path, glob, ignoreCase, literal, context, limit, hashline })` | `grep` |
//! | `sdk.find(glob, path?)` / `sdk.find({ pattern, path, limit })` | `find` |
//! | `sdk.ls(path?)` / `sdk.ls({ path, limit })` | `ls` |
//! | `sdk.astGrep(pattern, path?)` | `ast_grep` (structural search) |
//! | `sdk.jsonQuery(filter, path?)` / `sdk.jsonQuery({ json, filter })` | `json_query` |
//! | `sdk.currentTime()` | `current_time` |
//! | `sdk.call(tool, args)` | any tool the session has (the live registry) |
//!
//! # Authorization model
//!
//! The bridge does **no** authorization of its own. The single gate is whether
//! `run_code` itself was allowed to run — the agent-level approval / `yolo`
//! decision. Once it runs, **every** tool the session has is reachable through
//! `sdk.call(name, args)`, read *and* write, with no nested per-tool prompt,
//! whether the session is the main agent or a delegated subagent. The bridge
//! dispatches through the LIVE tool registry, so it reaches the exact tool
//! instances the session has (`lsp`, `debug`, `sessions`, memory, `jobs`,
//! `hub`, `browser`, …). `await sdk.tools()` lists what is reachable.
//!
//! # Error locations
//!
//! The program is evaluated with the file name `ptc-program`, and the wrapper
//! prefix stays on line 1, so a frame like `ptc-program:12:5` names line 12 of
//! the exact `code` string the model wrote — leading blank lines included,
//! because `code` is never trimmed.
//!
//! # Security boundary
//!
//! - **Capability layer.** The realm has no ambient I/O and no network API of
//!   its own: no `fs`, no `child_process`, no `require`, no `process`. *Only*
//!   the injected `sdk` object can reach outside, and every call is dispatched
//!   through the *same* [`Tool`] implementations a direct call uses (see
//!   [`RunCodeTool::bridge_call`]), so path confinement, workspace roots, and
//!   read settings are identical — no policy bypass. Network *reads* remain
//!   reachable only through [`sdk.read`](RunCodeTool), which forwards to the
//!   `read` tool and therefore fetches `http(s)` URLs.
//! - **Resource layer.** The realm is created with a heap ceiling
//!   ([`PTC_MEMORY_LIMIT_BYTES`]), a QuickJS stack ceiling, and an interrupt
//!   handler wired to the run deadline and a cancellation flag, so a runaway or
//!   wedged program is interrupted instead of taking the host down.
//! - A bridge call the host never answers fails at the run budget, so one stuck
//!   tool cannot pin the whole run past its deadline.

use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};
use crate::tools::{
    BashTool, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, EditTool, FindTool, GrepTool, LsTool, ReadTool,
    SearchBackend, Tool, ToolEffects, ToolOutput, ToolUpdate, WriteTool,
    search_backend_from_config, truncate_head,
};
use crate::workspace::WorkspaceHandle;
use async_trait::async_trait;
use rquickjs::function::{Func, Opt, Rest};
use rquickjs::{Coerced, Ctx, FromJs, Object, Promise, Value as JsValue};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Default wall-clock budget for one `run_code` invocation (seconds).
pub const DEFAULT_RUN_CODE_TIMEOUT_SECS: u64 = 120;

/// Schema tag for run_code outputs.
pub const PTC_RUN_CODE_SCHEMA: &str = "ra.ptc.run_code.v1";

/// Tools a bare `RunCodeTool` (no live registry bound — unit tests) can build
/// locally, used only as the `sdk.tools()` fallback list.
///
/// With a registry bound, the reachable set is the whole registry: `run_code`
/// does **no** nested authorization. The only gate is whether `run_code`
/// itself was allowed to run (agent-level approval / `yolo`); once it runs, any
/// tool the session has is callable and may read *and* write.
const BRIDGE_FALLBACK_TOOLS: [&str; 13] = [
    "read",
    "grep",
    "find",
    "ls",
    "ast_grep",
    "json_query",
    "current_time",
    "bash",
    "write",
    "edit",
    "ast_edit",
    "sessions",
    "web_search",
];

/// Script file name QuickJS reports in stack frames, so an error points at the
/// model's own `code` rather than at an anonymous eval.
const PROGRAM_FILENAME: &str = "ptc-program";

/// Maximum stack frames carried into the model-facing error text.
const MAX_ERROR_FRAMES: usize = 8;

/// QuickJS heap ceiling for one program. A program that allocates past this
/// aborts with an out-of-memory error instead of growing the host process.
const PTC_MEMORY_LIMIT_BYTES: usize = 256 * 1024 * 1024;

/// QuickJS stack ceiling for one program. Deep recursion trips this and raises
/// a catchable QuickJS error instead of overflowing the host thread stack.
const PTC_MAX_STACK_BYTES: usize = 2 * 1024 * 1024;

/// Stack reserve for the realm thread. The interpreter's own recursion is
/// bounded by [`PTC_MAX_STACK_BYTES`] (2 MiB), so 16 MiB still leaves 8x that
/// ceiling for the Rust frames around it — and the measured whole-turn floor
/// (708 KiB, `agent::tests::probe_full_turn_stack_scaling`, 905433d68) already
/// covered the path this thread serves: a `run_code` node inside a `dag`, with
/// `dag_tool` + `ptc_bridge` + the QuickJS realm all live. Bead bd-qtffv
/// tracks verifying the transport-heavy chains and going lower.
const REALM_THREAD_STACK_BYTES: usize = 16 * 1024 * 1024;

/// How often the host polls the realm for bridge calls and its terminal value.
const HOST_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How often a blocked bridge call re-checks the deadline and cancel flag.
const BRIDGE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Grace the host waits past the realm's own deadline before it gives up on a
/// worker wedged inside a native call (where the interrupt handler cannot run).
const HOST_DEADLINE_GRACE: Duration = Duration::from_secs(5);

/// Console text kept per run (surfaced in `details.console`).
const PTC_MAX_CONSOLE_BYTES: usize = 16 * 1024;

/// Actionable message for a run that outlived its budget.
const PTC_DEADLINE_MESSAGE: &str =
    "PTC_DEADLINE: run_code exceeded its wall-clock budget; the QuickJS realm was interrupted.";

/// Input parameters for the `run_code` tool.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunCodeInput {
    /// Async function body executed on the QuickJS realm; may `await`/`return`.
    pub code: String,
    /// 5–10 word description shown in the UI.
    pub description: Option<String>,
    /// Optional wall-clock budget in milliseconds (default 120_000).
    pub timeout_ms: Option<u64>,
}

/// Executes a JS program on an in-process QuickJS realm.
pub struct RunCodeTool {
    /// Working directory for the bridge tools (and thus path confinement).
    cwd: PathBuf,
    /// Wall-clock budget per invocation, in seconds.
    timeout_secs: u64,
    /// Session workspace roots, shared with every path-confining tool. The
    /// bridge constructs its tools with this handle so `run_code` sees the
    /// exact same root set (`--add-dir`) as a direct tool call.
    workspace: WorkspaceHandle,
    /// Search backend used by `grep`/`find`, matching the live registry.
    search_backend: SearchBackend,
    /// `read` image auto-resize flag, matching the live registry.
    image_auto_resize: bool,
    /// `read` image-blocking flag, matching the live registry.
    block_images: bool,
    /// Live registry, bound when this tool's registry is wrapped in a
    /// `SharedToolRegistry`. When present the bridge dispatches to the real
    /// tool instances (including `lsp`, `debug`, memory, `jobs`, `hub`, …)
    /// instead of rebuilding a subset.
    shared_registry: std::sync::OnceLock<std::sync::Weak<crate::tools::SharedToolRegistryInner>>,
}

impl RunCodeTool {
    /// Create a tool rooted at `cwd` with the default budget.
    ///
    /// The bridge's inner tools default to the identity workspace handle (cwd
    /// only) and the default search backend. [`RunCodeTool::with_workspace`]
    /// upgrades them to the session's live configuration.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            timeout_secs: DEFAULT_RUN_CODE_TIMEOUT_SECS,
            workspace: WorkspaceHandle::default(),
            search_backend: search_backend_from_config(None),
            image_auto_resize: true,
            block_images: false,
            shared_registry: std::sync::OnceLock::new(),
        }
    }

    /// Override the default budget (used by config wiring).
    #[must_use]
    pub const fn with_timeout_secs(mut self, timeout_secs: u64) -> Self {
        self.timeout_secs = timeout_secs;
        self
    }

    /// Share the session workspace root set with the bridge's inner tools so
    /// `run_code` honors `--add-dir` roots exactly like a direct call.
    #[must_use]
    pub fn with_workspace(mut self, workspace: WorkspaceHandle) -> Self {
        self.workspace = workspace;
        self
    }

    /// Adopt the registry's search backend and `read` image settings.
    ///
    /// `pub(crate)`: `SearchBackend` is crate-private (`tools.rs`), and the
    /// only caller is the in-crate tool registry, so exposing this at `pub`
    /// would leak a private type through a public interface.
    #[must_use]
    pub(crate) const fn with_read_config(
        mut self,
        search_backend: SearchBackend,
        image_auto_resize: bool,
        block_images: bool,
    ) -> Self {
        self.search_backend = search_backend;
        self.image_auto_resize = image_auto_resize;
        self.block_images = block_images;
        self
    }

    /// Build the whitelisted tool exactly the way the live registry does:
    /// same workspace handle, same search backend, same read settings.
    ///
    /// Constructing the same concrete [`Tool`] type (rather than a bespoke
    /// path resolver) is what guarantees the bridge cannot drift from the
    /// direct-call policy: path confinement, root set, and read behavior all
    /// live inside those tools.
    fn bridge_tool(&self, tool_name: &str) -> Option<Box<dyn Tool>> {
        match tool_name {
            "read" => Some(Box::new(
                ReadTool::with_settings(&self.cwd, self.image_auto_resize, self.block_images)
                    .with_workspace(self.workspace.clone()),
            )),
            "grep" => Some(Box::new(
                GrepTool::with_backend(&self.cwd, self.search_backend)
                    .with_workspace(self.workspace.clone()),
            )),
            "find" => Some(Box::new(
                FindTool::with_backend(&self.cwd, self.search_backend)
                    .with_workspace(self.workspace.clone()),
            )),
            "ls" => Some(Box::new(
                LsTool::new(&self.cwd).with_workspace(self.workspace.clone()),
            )),
            // Read-only extras, kept buildable so a bare `RunCodeTool` (unit
            // tests) can reach them without a bound registry. Constructed the
            // same way the registry builds them.
            "ast_grep" => Some(Box::new(crate::ast_tools::AstGrepTool::new(&self.cwd))),
            "json_query" => Some(Box::new(crate::json_query::JsonQueryTool::new())),
            "current_time" => Some(Box::new(crate::current_time::CurrentTimeTool::new())),
            // Write/process/network tools, constructed exactly the way the
            // registry builds them. A bare bridge reaches them with no
            // bridge-level authorization of its own.
            "bash" => Some(Box::new(BashTool::new(&self.cwd))),
            "write" => Some(Box::new(
                WriteTool::new(&self.cwd).with_workspace(self.workspace.clone()),
            )),
            "edit" => Some(Box::new(
                EditTool::new(&self.cwd).with_workspace(self.workspace.clone()),
            )),
            "ast_edit" => Some(Box::new(crate::ast_tools::AstEditTool::new(&self.cwd))),
            "sessions" => Some(Box::new(crate::sessions::SessionsTool::new())),
            "web_search" => Some(Box::new(crate::web_search::WebSearchTool::new())),
            _ => None,
        }
    }

    /// The tool names this run can reach, for `sdk.tools()`: every tool the live
    /// registry exposes (the whole session toolset), or the locally buildable
    /// fallback set when no registry is bound. There is no bridge-level
    /// authorization to filter by — reaching run_code's execution was the gate.
    fn reachable_tools(&self) -> Vec<String> {
        let live: Option<Vec<String>> = self
            .shared_registry
            .get()
            .and_then(crate::tools::SharedToolRegistry::upgrade)
            .map(|shared| {
                shared
                    .snapshot()
                    .tools()
                    .iter()
                    .map(|tool| tool.name().to_string())
                    .collect()
            });
        let mut reachable: Vec<String> = live.unwrap_or_else(|| {
            BRIDGE_FALLBACK_TOOLS
                .iter()
                .map(|name| (*name).to_string())
                .collect()
        });
        reachable.retain(|name| name != "run_code");
        reachable
    }

    /// Run one tool and keep its text. Non-text blocks are not silently
    /// dropped: an image/media block leaves a marker with its MIME type and
    /// approximate size, so a program can tell the model something was there
    /// (the realm channel is text-only; full base64 pass-through is a separate
    /// design task).
    async fn dispatch_bridge_tool(
        tool: &dyn Tool,
        input: Value,
    ) -> std::result::Result<String, String> {
        match tool.execute("run-code-bridge", input, None).await {
            Ok(output) => {
                let mut text = String::new();
                for block in &output.content {
                    match block {
                        ContentBlock::Text(t) => text.push_str(&t.text),
                        ContentBlock::Image(image) => text.push_str(&format!(
                            "\n[image omitted: {}, {} bytes]",
                            image.mime_type,
                            approx_decoded_bytes(&image.data)
                        )),
                        ContentBlock::Media(media) => text.push_str(&format!(
                            "\n[media omitted: {}, {}, {} bytes]",
                            media.name.as_deref().unwrap_or("media"),
                            media.mime_type,
                            approx_decoded_bytes(&media.data)
                        )),
                        _ => {}
                    }
                }
                if output.is_error { Err(text) } else { Ok(text) }
            }
            Err(err) => Err(err.to_string()),
        }
    }

    /// Dispatch one bridge call through the LIVE registry when available, so
    /// every tool the session has is reachable (not just the ones the bridge
    /// can rebuild standalone), and fall back to a locally-built tool for a
    /// bare `RunCodeTool` (unit tests). Errors name the reachable set.
    async fn bridge_call(
        &self,
        tool_name: &str,
        input: Value,
    ) -> std::result::Result<String, String> {
        if tool_name == "run_code" {
            return Err("PTC_BRIDGE_DENIED: `run_code` cannot call itself".to_string());
        }
        if let Some(weak) = self.shared_registry.get()
            && let Some(shared) = crate::tools::SharedToolRegistry::upgrade(weak)
        {
            let snapshot = shared.snapshot();
            if let Some(tool) = snapshot.get(tool_name) {
                return Self::dispatch_bridge_tool(tool, input).await;
            }
        }
        let Some(tool) = self.bridge_tool(tool_name) else {
            return Err(format!(
                "PTC_BRIDGE_DENIED: `{tool_name}` is not a registered tool"
            ));
        };
        Self::dispatch_bridge_tool(tool.as_ref(), input).await
    }
}

/* ------------------------------------------------------------------ */
/* Host <-> realm protocol                                             */
/* ------------------------------------------------------------------ */

/// One `sdk.*` call the program made, awaiting the host's reply.
struct BridgeCall {
    tool: String,
    args: Value,
    /// `Ok(text)` resolves the JS call; `Err(text)` throws it.
    reply: Sender<std::result::Result<String, String>>,
}

/// What the realm worker sends the host.
enum RealmMessage {
    /// A tool call to service; the host answers on `reply`.
    Call(BridgeCall),
    /// Terminal outcome, shaped like the old node protocol so error rendering
    /// is unchanged: `{ ok, result | error, console }`.
    Done(Value),
}

/// Run budget plus cancellation, shared with the QuickJS interrupt handler and
/// the bridge closures so both notice promptly.
#[derive(Clone)]
struct RealmFlags {
    origin: Instant,
    /// Budget in milliseconds; `u64::MAX` means "no deadline yet".
    budget_ms: Arc<AtomicU64>,
    cancelled: Arc<AtomicBool>,
}

impl RealmFlags {
    fn new(budget: Duration, cancelled: Arc<AtomicBool>) -> Self {
        Self {
            origin: Instant::now(),
            budget_ms: Arc::new(AtomicU64::new(
                u64::try_from(budget.as_millis()).unwrap_or(u64::MAX),
            )),
            cancelled,
        }
    }

    fn expired(&self) -> bool {
        let budget = self.budget_ms.load(Ordering::SeqCst);
        budget != u64::MAX
            && u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX) > budget
    }

    /// Cancel or deadline: either one aborts the interpreter.
    fn tripped(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst) || self.expired()
    }
}

/// A running QuickJS realm on its own OS thread.
///
/// Fresh per invocation: no state leaks from one `run_code` to the next, which
/// is what the old process-per-run design gave us for free.
struct JsRealm {
    inbox: Mutex<Receiver<RealmMessage>>,
    cancelled: Arc<AtomicBool>,
}

impl JsRealm {
    fn spawn(code: &str, budget: Duration, reachable: Vec<String>) -> Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flags = RealmFlags::new(budget, Arc::clone(&cancelled));
        let code = code.to_string();
        std::thread::Builder::new()
            .name("ptc-quickjs".into())
            .stack_size(REALM_THREAD_STACK_BYTES)
            .spawn(move || realm_thread(&code, &flags, &tx, budget, &reachable))
            .map_err(|err| Error::tool("run_code", format!("PTC_SPAWN: {err}")))?;
        Ok(Self {
            inbox: Mutex::new(rx),
            cancelled,
        })
    }

    /// Await the next realm message, or fail the run at `deadline`.
    ///
    /// Polling (rather than a blocking receive) keeps the async host runtime
    /// free while the program runs on its own thread.
    async fn next_message(&self, deadline: Instant) -> Result<RealmMessage> {
        loop {
            let received = self
                .inbox
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .try_recv();
            match received {
                Ok(message) => return Ok(message),
                Err(TryRecvError::Empty) => {
                    if Instant::now() > deadline {
                        return Err(Error::tool("run_code", PTC_DEADLINE_MESSAGE));
                    }
                    asupersync::time::sleep(asupersync::time::wall_now(), HOST_POLL_INTERVAL).await;
                }
                Err(TryRecvError::Disconnected) => {
                    return Err(Error::tool(
                        "run_code",
                        "PTC_EOF: the JavaScript realm exited before returning a result",
                    ));
                }
            }
        }
    }
}

impl Drop for JsRealm {
    fn drop(&mut self) {
        // Trips the interrupt handler on the realm thread so a program still
        // running after the host stopped listening winds down instead of
        // leaking a thread for the rest of the budget.
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

/* ------------------------------------------------------------------ */
/* The realm worker                                                    */
/* ------------------------------------------------------------------ */

/// Outcome of one program run, before it is shaped into a terminal message.
enum ProgramOutcome {
    Ok(Value),
    Failed(Value),
}

fn realm_thread(
    code: &str,
    flags: &RealmFlags,
    tx: &Sender<RealmMessage>,
    budget: Duration,
    reachable: &[String],
) {
    let console: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
    let outcome = match rquickjs::Runtime::new() {
        Ok(runtime) => match rquickjs::Context::full(&runtime) {
            Ok(context) => {
                runtime.set_memory_limit(PTC_MEMORY_LIMIT_BYTES);
                runtime.set_max_stack_size(PTC_MAX_STACK_BYTES);
                {
                    let flags = flags.clone();
                    runtime.set_interrupt_handler(Some(Box::new(move || flags.tripped())));
                }
                match context
                    .with(|ctx| install_globals(&ctx, &console, tx, flags, budget, reachable))
                {
                    Ok(()) => run_program(&context, &runtime, code, flags),
                    Err(err) => ProgramOutcome::Failed(protocol_error(&err.to_string())),
                }
            }
            Err(err) => ProgramOutcome::Failed(protocol_error(&format!("context: {err}"))),
        },
        Err(err) => ProgramOutcome::Failed(protocol_error(&format!("runtime: {err}"))),
    };
    let console_text = std::mem::take(&mut *console.borrow_mut());
    let terminal = terminal_message(outcome, &console_text);
    let _ = tx.send(RealmMessage::Done(terminal));
}

/// Wrap the model's body in an async IIFE.
///
/// The prefix stays on line 1, so a frame `ptc-program:N` names line N of the
/// `code` the model wrote; nothing is shifted. Only columns on line 1 are
/// offset, by the length of the prefix.
fn program_source(code: &str) -> String {
    format!("(async () => {{{code}\n}})()")
}

/// Install `console` and the Rust-built `sdk` bridge into the realm.
fn install_globals<'js>(
    ctx: &Ctx<'js>,
    console: &Rc<RefCell<String>>,
    tx: &Sender<RealmMessage>,
    flags: &RealmFlags,
    budget: Duration,
    reachable: &[String],
) -> rquickjs::Result<()> {
    let globals = ctx.globals();

    // console.* appends to the per-run buffer; the host surfaces it in
    // `details.console` (and, when a run fails, in the error content). It never
    // reaches the realm protocol as a value.
    let console_obj = Object::new(ctx.clone())?;
    for level in ["log", "info", "debug", "warn", "error"] {
        let sink = Rc::clone(console);
        let func = Func::from(move |ctx: Ctx<'js>, parts: Rest<JsValue<'js>>| {
            let text: Vec<String> = parts
                .0
                .iter()
                .map(|part| render_console_arg(&ctx, part))
                .collect();
            let mut buffer = sink.borrow_mut();
            buffer.push_str("[ptc:");
            buffer.push_str(level);
            buffer.push_str("] ");
            buffer.push_str(&text.join(" "));
            buffer.push('\n');
        });
        console_obj.set(level, func)?;
    }
    globals.set("console", console_obj)?;

    let sdk = Object::new(ctx.clone())?;
    set_sdk_helper(
        &sdk,
        SdkSpec {
            name: "read",
            tool: "read",
            key: "path",
            optional: false,
            scope: false,
        },
        tx,
        flags,
        budget,
    )?;
    set_sdk_helper(
        &sdk,
        SdkSpec {
            name: "grep",
            tool: "grep",
            key: "pattern",
            optional: false,
            scope: true,
        },
        tx,
        flags,
        budget,
    )?;
    set_sdk_helper(
        &sdk,
        SdkSpec {
            name: "find",
            tool: "find",
            key: "pattern",
            optional: false,
            scope: true,
        },
        tx,
        flags,
        budget,
    )?;
    set_sdk_helper(
        &sdk,
        SdkSpec {
            name: "ls",
            tool: "ls",
            key: "path",
            optional: true,
            scope: false,
        },
        tx,
        flags,
        budget,
    )?;
    // Read-only extras, exposed as named helpers like read/grep/find/ls.
    set_sdk_helper(
        &sdk,
        SdkSpec {
            name: "astGrep",
            tool: "ast_grep",
            key: "pattern",
            optional: false,
            scope: true,
        },
        tx,
        flags,
        budget,
    )?;
    set_sdk_helper(
        &sdk,
        SdkSpec {
            name: "jsonQuery",
            tool: "json_query",
            key: "filter",
            optional: false,
            scope: true,
        },
        tx,
        flags,
        budget,
    )?;
    set_sdk_helper(
        &sdk,
        SdkSpec {
            name: "currentTime",
            tool: "current_time",
            key: "path",
            optional: true,
            scope: false,
        },
        tx,
        flags,
        budget,
    )?;

    // Escape hatch: full argument set for any whitelisted tool. The host is
    // still the thing that decides whether the tool exists.
    let call_tx = tx.clone();
    let call_flags = flags.clone();
    let call = Func::from(
        move |ctx: Ctx<'js>,
              tool: String,
              args: Opt<JsValue<'js>>|
              -> rquickjs::Result<Promise<'js>> {
            let (promise, resolve, reject) = Promise::new(&ctx)?;
            let payload = args
                .0
                .as_ref()
                .and_then(|value| json_arg(&ctx, value))
                .unwrap_or_else(|| json!({}));
            let outcome = bridge_call(&ctx, &call_tx, &call_flags, budget, &tool, payload);
            settle_bridge_promise(&ctx, resolve, reject, outcome)?;
            Ok(promise)
        },
    );
    sdk.set("call", call)?;

    // Runtime introspection: the model can ask which tools this session can
    // actually reach. Returns a JSON array, as a Promise like every other
    // helper.
    let names: Vec<String> = reachable.to_vec();
    let tools_fn = Func::from(move |ctx: Ctx<'js>| -> rquickjs::Result<Promise<'js>> {
        let (promise, resolve, _reject) = Promise::new(&ctx)?;
        let json = serde_json::to_string(&names).unwrap_or_else(|_| "[]".to_string());
        resolve.call::<_, ()>((json,))?;
        Ok(promise)
    });
    sdk.set("tools", tools_fn)?;

    globals.set("sdk", sdk)?;
    Ok(())
}

/// Which host tool one `sdk` helper maps onto, and how it reads its arguments.
#[derive(Clone, Copy)]
struct SdkSpec {
    /// The binding's name on `sdk` (also the name used in error messages).
    name: &'static str,
    /// The host tool it dispatches to.
    tool: &'static str,
    /// The required key for the positional shorthand (`path`/`pattern`).
    key: &'static str,
    /// Whether the key may be omitted entirely (`ls`).
    optional: bool,
    /// Whether a second scope argument is merged in (`grep`/`find`).
    scope: bool,
}

fn set_sdk_helper<'js>(
    sdk: &Object<'js>,
    spec: SdkSpec,
    tx: &Sender<RealmMessage>,
    flags: &RealmFlags,
    budget: Duration,
) -> rquickjs::Result<()> {
    let SdkSpec {
        name,
        tool,
        key,
        optional,
        scope,
    } = spec;
    let tx = tx.clone();
    let flags = flags.clone();
    let func = Func::from(
        move |ctx: Ctx<'js>,
              first: Opt<JsValue<'js>>,
              second: Opt<JsValue<'js>>|
              -> rquickjs::Result<Promise<'js>> {
            let (promise, resolve, reject) = Promise::new(&ctx)?;
            let outcome = normalize_args(&ctx, first.0.as_ref(), key, name, optional)
                .and_then(|args| {
                    if scope {
                        merge_scope(&ctx, args, second.0.as_ref(), name)
                    } else {
                        Ok(args)
                    }
                })
                .and_then(|args| bridge_call(&ctx, &tx, &flags, budget, tool, args));
            settle_bridge_promise(&ctx, resolve, reject, outcome)?;
            Ok(promise)
        },
    );
    sdk.set(name, func)
}

/// Send one bridge call and block until the host answers, the run is
/// cancelled, or the budget runs out.
///
/// Returns the host text on success, or the host's error text: the caller
/// settles the helper's promise with the outcome. The `Ctx` parameter is kept
/// so the call shape matches the promise-building closures that invoke it.
fn bridge_call(
    _ctx: &Ctx<'_>,
    tx: &Sender<RealmMessage>,
    flags: &RealmFlags,
    budget: Duration,
    tool: &str,
    args: Value,
) -> std::result::Result<String, String> {
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    if tx
        .send(RealmMessage::Call(BridgeCall {
            tool: tool.to_string(),
            args,
            reply: reply_tx,
        }))
        .is_err()
    {
        return Err("PTC_CANCELLED: the host stopped listening".to_string());
    }
    let deadline = Instant::now() + budget;
    loop {
        match reply_rx.recv_timeout(BRIDGE_POLL_INTERVAL) {
            Ok(Ok(text)) => return Ok(text),
            Ok(Err(err)) => return Err(err),
            Err(RecvTimeoutError::Disconnected) => {
                return Err("PTC_CANCELLED: the host dropped the bridge".to_string());
            }
            Err(RecvTimeoutError::Timeout) => {
                if flags.cancelled.load(Ordering::SeqCst) {
                    return Err("PTC_CANCELLED: the run was cancelled".to_string());
                }
                if Instant::now() > deadline {
                    return Err(format!(
                        "PTC_BRIDGE_TIMEOUT: tool `{tool}` did not answer within the run budget"
                    ));
                }
            }
        }
    }
}

/// Settle a helper's promise with the host reply, or reject it with a JS
/// `Error` so `await sdk.x(...)` throws an object whose `.message` is the
/// host's error text.
fn settle_bridge_promise<'js>(
    ctx: &Ctx<'js>,
    resolve: rquickjs::Function<'js>,
    reject: rquickjs::Function<'js>,
    outcome: std::result::Result<String, String>,
) -> rquickjs::Result<()> {
    match outcome {
        Ok(text) => resolve.call::<_, ()>((text,))?,
        Err(message) => {
            let ctor: rquickjs::Function<'js> = ctx.globals().get("Error")?;
            let error: Object<'js> = ctor.call((message,))?;
            reject.call::<_, ()>((error,))?;
        }
    }
    Ok(())
}

/* ------------------------------------------------------------------ */
/* Argument normalization (built in Rust, no JS asset)                 */
/* ------------------------------------------------------------------ */

/// Convert a JS value to the JSON an host tool argument needs, if it has one.
fn json_arg<'js>(ctx: &Ctx<'js>, value: &JsValue<'js>) -> Option<Value> {
    if value.is_undefined() {
        return None;
    }
    let text = ctx.json_stringify(value.clone()).ok().flatten()?;
    serde_json::from_str(&text.to_string().ok()?).ok()
}

/// Human-readable kind for error messages.
fn js_kind(value: &JsValue<'_>) -> &'static str {
    if value.is_null() {
        "null"
    } else if value.is_undefined() {
        "undefined"
    } else if value.is_array() {
        "an array"
    } else if value.is_function() {
        "a function"
    } else if value.is_object() {
        "an object"
    } else if value.is_string() {
        "a string"
    } else if value.is_number() {
        "a number"
    } else if value.is_bool() {
        "a boolean"
    } else {
        "an unsupported value"
    }
}

fn js_string<'js>(ctx: &Ctx<'js>, value: &JsValue<'js>) -> String {
    Coerced::<String>::from_js(ctx, value.clone())
        .map(|coerced| coerced.0)
        .unwrap_or_default()
}

/// Render one console argument without ever throwing: a string prints raw,
/// JSON-able values print as JSON, and anything JSON cannot express (symbols,
/// functions, circular structures) falls back to text instead of aborting the
/// program.
fn render_console_arg<'js>(ctx: &Ctx<'js>, value: &JsValue<'js>) -> String {
    if value.is_string() {
        return js_string(ctx, value);
    }
    if value.is_undefined() {
        return "undefined".to_string();
    }
    if value.is_null() {
        return "null".to_string();
    }
    // A symbol has no successful `ToString` coercion, so describe it instead
    // of letting that exception escape the console call.
    if value.is_symbol() {
        return js_kind(value).to_string();
    }
    match ctx.json_stringify(value.clone()) {
        Ok(Some(text)) => text.to_string().unwrap_or_default(),
        Ok(None) => render_console_text(ctx, value),
        Err(_) => {
            // `json_stringify` raises a pending exception for BigInt/circular
            // input; clear it so it cannot surface later.
            let _ = ctx.catch();
            render_console_text(ctx, value)
        }
    }
}

/// Fallback text for a console argument JSON cannot express. Clears any
/// exception a failed coercion left pending so the console call cannot throw.
fn render_console_text<'js>(ctx: &Ctx<'js>, value: &JsValue<'js>) -> String {
    let text = js_string(ctx, value);
    let _ = ctx.catch();
    if text.is_empty() {
        js_kind(value).to_string()
    } else {
        text
    }
}

/// Approximate decoded byte length of a base64 payload, for omission markers.
fn approx_decoded_bytes(base64: &str) -> usize {
    base64.len() / 4 * 3
}

/// Normalize the first helper argument: positional shorthand or options object.
///
/// Mirrors the documented contract — a non-empty string becomes `{ key: value }`,
/// an options object is forwarded verbatim but must carry `key`, and (for `ls`)
/// both may be omitted.
fn normalize_args<'js>(
    ctx: &Ctx<'js>,
    value: Option<&JsValue<'js>>,
    key: &str,
    form: &str,
    optional: bool,
) -> std::result::Result<Value, String> {
    let Some(value) = value else {
        return if optional {
            Ok(json!({}))
        } else {
            Err(format!(
                "sdk.{form}: expected a `{key}` string or an options object, got undefined"
            ))
        };
    };
    if value.is_string() {
        let text = js_string(ctx, value);
        if text.is_empty() {
            return Err(format!("sdk.{form}: `{key}` must be a non-empty string"));
        }
        return Ok(Value::Object(Map::from_iter([(
            key.to_string(),
            Value::String(text),
        )])));
    }
    let Some(Value::Object(map)) = json_arg(ctx, value) else {
        if optional && (value.is_undefined() || value.is_null()) {
            return Ok(json!({}));
        }
        return Err(format!(
            "sdk.{form}: expected a `{key}` string or an options object, got {}",
            js_kind(value)
        ));
    };
    match map.get(key) {
        Some(Value::String(text)) if !text.is_empty() => {}
        Some(other) => {
            return Err(format!(
                "sdk.{form}: an options object needs a non-empty `{key}` string, got {other}"
            ));
        }
        None if optional => {}
        None => {
            return Err(format!(
                "sdk.{form}: an options object needs a non-empty `{key}` string"
            ));
        }
    }
    Ok(Value::Object(map))
}

/// Merge the optional second scope argument (path string or options object).
fn merge_scope<'js>(
    ctx: &Ctx<'js>,
    mut args: Value,
    scope: Option<&JsValue<'js>>,
    form: &str,
) -> std::result::Result<Value, String> {
    let Some(scope) = scope else {
        return Ok(args);
    };
    if scope.is_undefined() || scope.is_null() {
        return Ok(args);
    }
    if scope.is_string() {
        let text = js_string(ctx, scope);
        if text.is_empty() {
            return Err(format!(
                "sdk.{form}: the second argument must be a non-empty path string"
            ));
        }
        if let Value::Object(map) = &mut args {
            map.insert("path".to_string(), Value::String(text));
        }
        return Ok(args);
    }
    let Some(Value::Object(extra)) = json_arg(ctx, scope) else {
        return Err(format!(
            "sdk.{form}: the second argument must be a path string or an options object, got {}",
            js_kind(scope)
        ));
    };
    if let Value::Object(map) = &mut args {
        map.extend(extra);
    }
    Ok(args)
}

/* ------------------------------------------------------------------ */
/* Program execution                                                   */
/* ------------------------------------------------------------------ */

fn run_program(
    context: &rquickjs::Context,
    runtime: &rquickjs::Runtime,
    code: &str,
    flags: &RealmFlags,
) -> ProgramOutcome {
    enum Phase1 {
        Done(ProgramOutcome),
        Pending(rquickjs::Persistent<Promise<'static>>),
    }
    let source = program_source(code);
    let phase1 = context.with(|ctx| {
        let mut options = rquickjs::context::EvalOptions::default();
        options.global = true;
        options.strict = true;
        options.filename = Some(PROGRAM_FILENAME.to_string());
        let evaluated: rquickjs::Result<JsValue<'_>> =
            ctx.eval_with_options(source.as_bytes(), options);
        match evaluated {
            Err(err) => Phase1::Done(ProgramOutcome::Failed(error_or_stop(&ctx, &err, flags))),
            // A non-promise completion is a syntax-level surprise; take the
            // completion value as the result.
            Ok(value) => Promise::from_value(value.clone()).map_or_else(
                |_| Phase1::Done(ProgramOutcome::Ok(serialize_value(&ctx, &value))),
                |promise| Phase1::Pending(rquickjs::Persistent::save(&ctx, promise)),
            ),
        }
    });

    let saved = match phase1 {
        Phase1::Done(outcome) => {
            while matches!(runtime.execute_pending_job(), Ok(true)) {}
            return outcome;
        }
        Phase1::Pending(saved) => saved,
    };

    // Pump jobs until the program's promise settles or the run is stopped.
    // A pending promise with no runnable jobs means the program is waiting on
    // something that will never arrive (there is no timer or I/O API), so the
    // loop simply waits for the deadline to trip.
    loop {
        let state = context.with(|ctx| {
            saved
                .clone()
                .restore(&ctx)
                .ok()
                .map(|promise| promise.state())
        });
        match state {
            Some(rquickjs::promise::PromiseState::Pending) => {
                if flags.cancelled.load(Ordering::SeqCst) {
                    return ProgramOutcome::Failed(cancelled_error());
                }
                if flags.expired() {
                    return ProgramOutcome::Failed(deadline_error());
                }
                match runtime.execute_pending_job() {
                    Ok(true) => {}
                    Ok(false) => std::thread::sleep(Duration::from_millis(2)),
                    Err(_) => break,
                }
            }
            _ => break,
        }
    }
    while matches!(runtime.execute_pending_job(), Ok(true)) {}

    context.with(|ctx| {
        let Some(promise) = saved.clone().restore(&ctx).ok() else {
            return ProgramOutcome::Failed(protocol_error("promise restore failed"));
        };
        match promise.result::<JsValue<'_>>() {
            Some(Ok(value)) => ProgramOutcome::Ok(serialize_value(&ctx, &value)),
            Some(Err(err)) => ProgramOutcome::Failed(error_or_stop(&ctx, &err, flags)),
            None if flags.expired() => ProgramOutcome::Failed(deadline_error()),
            None if flags.cancelled.load(Ordering::SeqCst) => {
                ProgramOutcome::Failed(cancelled_error())
            }
            None => ProgramOutcome::Failed(protocol_error("promise never settled")),
        }
    })
}

/// Render the program's return value as JSON the host can display.
fn serialize_value<'js>(ctx: &Ctx<'js>, value: &JsValue<'js>) -> Value {
    if value.is_undefined() {
        return Value::Null;
    }
    if let Some(parsed) = json_arg(ctx, value) {
        return parsed;
    }
    // Non-JSON returns (bigint, function, circular, symbol) fall back to their
    // text form, mirroring the eval kernel's extraction.
    Value::String(js_string(ctx, value))
}

/// Shape a thrown value into `{ name, message, stack?, code? }`, the payload
/// [`render_program_error`] renders.
fn error_payload(ctx: &Ctx<'_>, err: &rquickjs::Error) -> Value {
    if !err.is_exception() {
        return json!({ "name": "Error", "message": err.to_string() });
    }
    let thrown = ctx.catch();
    let Some(exception) = thrown.as_exception() else {
        // `throw "x"` / `throw { code: 1 }`: no Error shape to read.
        let message = json_arg(ctx, &thrown)
            .map_or_else(|| js_string(ctx, &thrown), |value| value.to_string());
        return json!({ "name": "Error", "message": message });
    };
    let name = exception
        .get::<_, String>("name")
        .unwrap_or_else(|_| "Error".to_string());
    let mut payload = json!({
        "name": name,
        "message": exception.message().unwrap_or_default(),
    });
    if let Some(stack) = exception.stack() {
        payload["stack"] = Value::String(stack);
    }
    if let Ok(Some(code)) = exception.get::<_, Option<String>>("code") {
        payload["code"] = Value::String(code);
    }
    payload
}

/// Prefer the run's own stop reason over a raw QuickJS interrupt: an interrupt
/// raised because the deadline or cancellation tripped must read as
/// PTC_DEADLINE / PTC_CANCELLED, not "InternalError: interrupted".
fn error_or_stop(ctx: &Ctx<'_>, err: &rquickjs::Error, flags: &RealmFlags) -> Value {
    if flags.cancelled.load(Ordering::SeqCst) {
        return cancelled_error();
    }
    if flags.expired() {
        return deadline_error();
    }
    error_payload(ctx, err)
}

fn deadline_error() -> Value {
    json!({
        "name": "Error",
        "message": PTC_DEADLINE_MESSAGE,
        "code": "PTC_DEADLINE",
    })
}

fn cancelled_error() -> Value {
    json!({
        "name": "Error",
        "message": "PTC_CANCELLED: the run_code realm was cancelled before the program returned.",
        "code": "PTC_CANCELLED",
    })
}

fn protocol_error(detail: &str) -> Value {
    json!({
        "name": "Error",
        "message": format!("PTC_PROTOCOL: {detail}"),
        "code": "PTC_PROTOCOL",
    })
}

fn terminal_message(outcome: ProgramOutcome, console: &str) -> Value {
    let console = bound_console(console);
    match outcome {
        ProgramOutcome::Ok(result) => json!({ "ok": true, "result": result, "console": console }),
        ProgramOutcome::Failed(error) => json!({ "ok": false, "error": error, "console": console }),
    }
}

fn bound_console(console: &str) -> String {
    if console.len() <= PTC_MAX_CONSOLE_BYTES {
        return console.to_string();
    }
    let mut end = PTC_MAX_CONSOLE_BYTES;
    while end > 0 && !console.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &console[..end])
}

/* ------------------------------------------------------------------ */
/* Error rendering                                                     */
/* ------------------------------------------------------------------ */

/// Render the realm's terminal `error` field for the model.
///
/// Surface the `message` (prefixed by the name and error code when they add
/// signal) plus a short trace pointing at the program, instead of dumping the
/// whole JSON object.
fn render_program_error(error: &Value) -> String {
    let Value::Object(_) = error else {
        return match error {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
    };
    let name = error.get("name").and_then(Value::as_str).unwrap_or("Error");
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .map_or_else(|| error.to_string(), str::to_string);
    let mut head = if name == "Error" || name.is_empty() {
        message
    } else {
        format!("{name}: {message}")
    };
    let code_suffix = error
        .get("code")
        .and_then(Value::as_str)
        .filter(|code| !code.is_empty() && !head.contains(*code))
        .map_or_else(String::new, |code| format!(" [{code}]"));
    head.push_str(&code_suffix);
    match error.get("stack").and_then(Value::as_str) {
        Some(stack) => {
            let frames = render_error_frames(stack);
            if frames.is_empty() {
                head
            } else {
                format!("{head}\n{}", frames.join("\n"))
            }
        }
        None => head,
    }
}

/// Extract `<marker><line>:<col>` from one stack frame, if present.
///
/// `marker` includes its trailing colon (`"ptc-program:"`).
fn extract_frame_site(frame: &str, marker: &str) -> Option<String> {
    let start = frame.find(marker)?;
    let rest = &frame[start + marker.len()..];
    let line_end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    if line_end == 0 || !rest[line_end..].starts_with(':') {
        return None;
    }
    let col_rest = &rest[line_end + 1..];
    let col_end = col_rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(col_rest.len());
    if col_end == 0 {
        return None;
    }
    Some(format!(
        "{marker}{}:{}",
        &rest[..line_end],
        &col_rest[..col_end]
    ))
}

/// Pick the actionable frames out of a QuickJS stack.
///
/// Only frames that name the program survive: they point at the exact line of
/// the `code` the model wrote. QuickJS's own frames (`<eval>`, native helpers)
/// are dropped as noise. Unlike node, QuickJS stacks carry no `Error: msg`
/// header line, so every line is a candidate frame.
fn render_error_frames(stack: &str) -> Vec<String> {
    stack
        .lines()
        .filter_map(|frame| extract_frame_site(frame, "ptc-program:"))
        .take(MAX_ERROR_FRAMES)
        .map(|site| format!("  at {site}"))
        .collect()
}

/* ------------------------------------------------------------------ */
/* Tool implementation                                                 */
/* ------------------------------------------------------------------ */

#[async_trait]
impl Tool for RunCodeTool {
    fn name(&self) -> &'static str {
        "run_code"
    }

    fn label(&self) -> &'static str {
        "run code"
    }

    fn description(&self) -> &'static str {
        "Execute a JavaScript program against the available tools. `code` is the \
         BODY of an async function (top-level `await` and `return` work). Call \
         tools as `await sdk.read('path')`, `await sdk.grep('needle', 'dir')`, \
         `await sdk.find('*.rs', 'dir')`, or `await sdk.ls('dir')`; every helper \
         also accepts an options object (`await sdk.read({ path: 'src/main.rs', \
         offset: 1, limit: 40 })`, `await sdk.ls({ limit: 20 })`), and `await \
         sdk.call(tool, args)` reaches the full argument set. Every helper \
         returns a Promise, so `await` it. `await sdk.tools()` lists the tools \
         this session can currently reach. The program runs on the built-in \
         QuickJS engine with no filesystem, process, or network API of its own: \
         `sdk.*` is the only way out, and it reaches the read-only tools \
         `read`, `grep`, `find`, `ls`, `ast_grep` (`sdk.astGrep`), `json_query` \
         (`sdk.jsonQuery`) and `current_time` (`sdk.currentTime`). Note that \
         `read` can fetch http(s) URLs, so network reads are reachable through \
         it. Errors carry \
         `ptc-program:<line>` frames pointing at your own code; `console.log` \
         is captured separately and never throws. Only what you return is \
         program output — curate it. One run_code \
         replaces many model round-trips. Every other internal tool (`bash`, \
         `edit`, `lsp`, `debug`, `sessions`, `web_search`, memory, `jobs`, …) is \
         reachable via `await sdk.call(name, args)`; there is no nested \
         authorization — the only gate is whether run_code was allowed to run. \
         `await sdk.tools()` lists what is reachable. This holds for the main \
         agent and for delegated subagents alike. A missing tool is refused \
         with PTC_BRIDGE_DENIED."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "The program: the body of an async JavaScript function \
                                    (top-level await/return allowed)."
                },
                "description": {
                    "type": "string",
                    "description": "Clear, concise description of what this program does in \
                                    active voice, 5-10 words (shown in the UI). Examples: \
                                    \"Count TODO markers across packages\"; \"Read failing \
                                    test and its fixture\"."
                },
                "timeoutMs": {
                    "type": "integer",
                    "description": "Positive elapsed-time budget in milliseconds (default 120000)."
                }
            },
            "required": ["code"]
        })
    }

    fn bind_shared_registry(
        &self,
        shared: &std::sync::Weak<crate::tools::SharedToolRegistryInner>,
    ) {
        let _ = self.shared_registry.set(shared.clone());
    }

    fn effects(&self) -> ToolEffects {
        // Arbitrary code execution, serialized fail-closed like bash/eval. The
        // bridge adds no authorization of its own: it can reach every tool the
        // session has, so declare the registry's union when one is bound. A
        // bare `RunCodeTool` (unit tests) stays at `process()`.
        let mut effects = ToolEffects::process();
        if let Some(weak) = self.shared_registry.get()
            && let Some(shared) = crate::tools::SharedToolRegistry::upgrade(weak)
        {
            for tool in shared.snapshot().tools() {
                // `run_code` itself sits in that snapshot, and its `effects()`
                // re-enters this method through the same bound registry, so
                // unioning it recurses until the stack overflows. It is already
                // the `process()` baseline above.
                if tool.name() == "run_code" {
                    continue;
                }
                effects = effects.union(tool.effects());
            }
        }
        effects
    }

    #[allow(clippy::too_many_lines)]
    async fn execute(
        &self,
        _tool_call_id: &str,
        input: Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: RunCodeInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;
        if input.code.trim().is_empty() {
            return Err(Error::validation(
                "run_code requires a non-empty `code` body".to_string(),
            ));
        }
        // Deliberately NOT trimmed: reported `ptc-program:<line>` frames must
        // line up with the code string the model actually wrote.
        let code = input.code.as_str();
        let timeout = input.timeout_ms.map_or_else(
            || Duration::from_secs(self.timeout_secs),
            |ms| Duration::from_millis(ms.max(1)),
        );
        let deadline = Instant::now() + timeout;

        let realm = JsRealm::spawn(code, timeout, self.reachable_tools())?;
        // The realm enforces the budget itself via its interrupt handler; this
        // is only the backstop for a worker wedged in a native call.
        let host_deadline = deadline + HOST_DEADLINE_GRACE;

        let terminal = loop {
            match realm.next_message(host_deadline).await? {
                RealmMessage::Call(call) => {
                    let reply = self.bridge_call(&call.tool, call.args).await;
                    let _ = call.reply.send(reply);
                }
                RealmMessage::Done(terminal) => break terminal,
            }
        };

        let console = terminal
            .get("console")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let ok = terminal.get("ok").and_then(Value::as_bool).unwrap_or(false);
        if !ok {
            let error_value = terminal.get("error");
            let error =
                error_value.map_or_else(|| "run_code failed".to_string(), render_program_error);
            let error_code = error_value
                .and_then(|value| value.get("code"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let mut details = json!({
                "schema": PTC_RUN_CODE_SCHEMA,
                "ok": false,
                "error": error,
                "errorCode": error_code,
                "runtime": "quickjs",
            });
            attach_console(&mut details, &console);
            let mut message = format!("run_code failed: {error}");
            if !console.is_empty() {
                message = format!("[console output]\n{console}[end console output]\n{message}");
            }
            return Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(message))],
                details: Some(details),
                is_error: true,
            });
        }
        let value = terminal.get("result").cloned().unwrap_or(Value::Null);
        let rendered = match &value {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let truncation = truncate_head(rendered, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        // Details are not sent to the model, so a truncated result must say so
        // in the content itself; otherwise the model silently sees a prefix.
        let mut content = truncation.content.clone();
        if truncation.truncated {
            content.push_str(&format!(
                "\n\n[truncated: kept first {} of {} lines, {} of {} bytes]",
                truncation.output_lines,
                truncation.total_lines,
                truncation.output_bytes,
                truncation.total_bytes
            ));
        }
        let mut details = json!({
            "schema": PTC_RUN_CODE_SCHEMA,
            "ok": true,
            "truncated": truncation.truncated,
            "description": input.description,
            "runtime": "quickjs",
        });
        attach_console(&mut details, &console);
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(content))],
            details: Some(details),
            is_error: false,
        })
    }
}

/// Add captured console output to `details` when the program produced any.
fn attach_console(details: &mut Value, console: &str) {
    if console.is_empty() {
        return;
    }
    if let Value::Object(map) = details {
        map.insert("console".to_string(), Value::String(console.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Block on `run_code` with a fresh single-threaded runtime.
    fn run(tool: &RunCodeTool, input: Value) -> std::result::Result<ToolOutput, Error> {
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .build()
            .expect("runtime build");
        runtime.block_on(tool.execute("t1", input, None))
    }

    /// Concatenated text of a tool result.
    fn output_text(output: &ToolOutput) -> String {
        output
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect()
    }

    fn details_str(output: &ToolOutput, key: &str) -> Option<String> {
        output
            .details
            .as_ref()
            .and_then(|details| details.get(key))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    /// Run a program and return either its rendered output or the error text.
    fn run_text(tool: &RunCodeTool, code: &str) -> String {
        match run(tool, json!({ "code": code, "timeoutMs": 30_000 })) {
            Ok(output) => output_text(&output),
            Err(err) => err.to_string(),
        }
    }

    /// A scratch directory holding the given files.
    fn scratch_dir(tag: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pi-ptc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        for (name, content) in files {
            std::fs::write(dir.join(name), content).expect("write fixture");
        }
        dir
    }

    #[test]
    fn run_code_exposes_process_effects() {
        let tool = RunCodeTool::new(".");
        assert_eq!(tool.name(), "run_code");
        assert_eq!(tool.effects(), ToolEffects::process());
    }

    #[test]
    fn run_code_schema_requires_code() {
        let tool = RunCodeTool::new(".");
        let params = tool.parameters();
        assert_eq!(params["required"], json!(["code"]));
        assert!(params["properties"]["timeoutMs"].is_object());
    }

    #[test]
    fn bridge_reaches_any_registry_tool() {
        // No nested authorization: once run_code runs, any tool the session has
        // is reachable — `bash` included, read *and* write.
        let registry =
            crate::tools::ToolRegistry::new(&["run_code", "bash"], std::path::Path::new("."), None);
        let shared = crate::tools::SharedToolRegistry::new(registry);
        let snapshot = shared.snapshot();
        let tool = snapshot.get("run_code").expect("run_code registered");
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .build()
            .expect("runtime build");
        let out = runtime
            .block_on(tool.execute(
                "t1",
                json!({
                    "code": "return String(await sdk.call('bash', { command: 'echo hi' })).includes('hi');",
                    "timeoutMs": 30_000,
                }),
                None,
            ))
            .expect("run");
        assert_eq!(output_text(&out), "true");
    }

    #[test]
    fn read_only_extras_are_reachable() {
        // ast_grep / json_query / current_time join the always-allowed set and
        // are exposed as named helpers.
        let dir = scratch_dir("extras", &[("a.rs", "fn alpha() {}\n")]);
        let code = r#"
            const t = String(await sdk.currentTime());
            const q = String(await sdk.jsonQuery({ json: '{"n": 41}', filter: '.n + 1' }));
            const g = String(await sdk.astGrep('fn $NAME', '.'));
            return { time: t.length > 0, json: q.includes('42'), ast: g.includes('alpha') };
        "#;
        let out = run(
            &RunCodeTool::new(&dir),
            json!({ "code": code, "timeoutMs": 60_000 }),
        )
        .expect("run");
        assert!(!out.is_error, "{}", output_text(&out));
        let text = output_text(&out);
        assert!(text.contains(r#""time":true"#), "{text}");
        assert!(text.contains(r#""json":true"#), "{text}");
        assert!(text.contains(r#""ast":true"#), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bridge_reaches_non_whitelisted_tool() {
        // No nested authorization: the bridge reaches a real `bash` tool.
        let tool = RunCodeTool::new(".");
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .build()
            .expect("runtime build");
        let out = runtime
            .block_on(tool.bridge_call("bash", json!({ "command": "echo hi" })))
            .expect("bash must be reachable");
        assert!(out.contains("hi"), "{out}");
    }

    #[test]
    fn bridge_allows_read_through_the_real_tool() {
        // The whitelisted path must actually reach the shared ReadTool and
        // return its text rendering.
        let dir = scratch_dir("bridge", &[("probe.txt", "hello-bridge")]);
        let tool = RunCodeTool::new(&dir);
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .build()
            .expect("runtime build");
        let out = runtime
            .block_on(tool.bridge_call("read", json!({ "path": "probe.txt" })))
            .expect("read via bridge should succeed");
        assert!(out.contains("hello-bridge"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_empty_code() {
        let tool = RunCodeTool::new(".");
        let err = run(&tool, json!({ "code": "   " })).expect_err("empty code must be rejected");
        assert!(err.to_string().contains("non-empty"));
    }

    #[test]
    fn return_value_is_the_only_output() {
        let out = run(
            &RunCodeTool::new("."),
            json!({ "code": "console.log('noise'); return { answer: 42 };", "timeoutMs": 30_000 }),
        )
        .expect("run");
        assert!(!out.is_error, "{}", output_text(&out));
        assert_eq!(output_text(&out), r#"{"answer":42}"#);
        assert!(!output_text(&out).contains("noise"));
    }

    #[test]
    fn positional_and_options_forms_both_work() {
        // Regression for the bug that motivated the QuickJS port: the running
        // node SDK rejected the documented options-object form with a
        // misleading "pattern must be a non-empty string". Both forms must work
        // in the same realm.
        let dir = scratch_dir("forms", &[("needle.txt", "alpha needle\n")]);
        let code = r"
            const positional = String(await sdk.grep('needle', '.'));
            const object = String(await sdk.grep({ pattern: 'needle', path: '.' }));
            const readPositional = String(await sdk.read('needle.txt'));
            const readObject = String(await sdk.read({ path: 'needle.txt' }));
            return {
                positional: positional.includes('needle'),
                object: object.includes('needle'),
                readPositional: readPositional.includes('needle'),
                readObject: readObject.includes('needle'),
            };
        ";
        let out = run(
            &RunCodeTool::new(&dir),
            json!({ "code": code, "timeoutMs": 30_000 }),
        )
        .expect("run");
        assert!(!out.is_error, "{}", output_text(&out));
        let text = output_text(&out);
        for key in ["positional", "object", "readPositional", "readObject"] {
            assert!(text.contains(&format!(r#""{key}":true"#)), "{text}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn options_object_forwards_every_key() {
        // The object form must forward every key, not just `path`: `limit: 1`
        // has to reach ReadTool and actually truncate.
        let dir = scratch_dir("opts", &[("multi.txt", "one\ntwo\nthree\n")]);
        let code = r"
            const limited = String(await sdk.read({ path: 'multi.txt', limit: 1 }));
            const full = String(await sdk.read('multi.txt'));
            return {
                limitedHasThree: limited.includes('three'),
                fullHasThree: full.includes('three'),
                limitedHasOne: limited.includes('one'),
            };
        ";
        let out = run(
            &RunCodeTool::new(&dir),
            json!({ "code": code, "timeoutMs": 30_000 }),
        )
        .expect("options-object read should succeed");
        assert!(!out.is_error, "{}", output_text(&out));
        let text = output_text(&out);
        assert!(text.contains(r#""limitedHasThree":false"#), "{text}");
        assert!(text.contains(r#""fullHasThree":true"#), "{text}");
        assert!(text.contains(r#""limitedHasOne":true"#), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn call_escape_hatch_reaches_the_bridge() {
        let dir = scratch_dir("hatch", &[("needle.txt", "alpha needle\n")]);
        let code = r"
            const viaCall = String(await sdk.call('grep', { pattern: 'needle', path: '.' }));
            return { reached: viaCall.includes('needle') };
        ";
        let out = run(
            &RunCodeTool::new(&dir),
            json!({ "code": code, "timeoutMs": 30_000 }),
        )
        .expect("run");
        assert!(!out.is_error, "{}", output_text(&out));
        assert!(output_text(&out).contains(r#""reached":true"#));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn five_step_orchestration_in_one_round_trip() {
        // Acceptance: one run_code performs 5 bridge calls (multi-return,
        // grep, ls, find, read) with no extra model round-trips, and only the
        // curated return value is surfaced.
        let dir = scratch_dir("orch", &[("a.txt", "alpha needle\n"), ("b.txt", "beta\n")]);
        let code = r"
            const a = await sdk.read('a.txt');      // 1
            const hits = await sdk.grep('needle');   // 2
            const listing = await sdk.ls('.');       // 3
            const found = await sdk.find('*.txt');   // 4
            const b = await sdk.read('b.txt');       // 5
            return { calls: 5, hasAlpha: a.includes('alpha'), hasBeta: b.includes('beta'),
                     grep: String(hits).length > 0, listing: String(listing).length > 0,
                     find: String(found).length > 0 };
        ";
        let out = run(
            &RunCodeTool::new(&dir),
            json!({ "code": code, "timeoutMs": 30_000 }),
        )
        .expect("orchestration should succeed");
        assert!(!out.is_error, "{}", output_text(&out));
        let text = output_text(&out);
        assert!(text.contains(r#""calls":5"#), "{text}");
        assert!(text.contains(r#""hasAlpha":true"#), "{text}");
        assert!(text.contains(r#""hasBeta":true"#), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn async_program_settles() {
        let text = run_text(
            &RunCodeTool::new("."),
            "const value = await Promise.resolve(7);\nawait new Promise((resolve) => resolve());\nreturn value * 2;",
        );
        assert_eq!(text, "14");
    }

    #[test]
    fn thrown_program_reports_program_line() {
        // The error must name the line of the model's own `code`; the wrapper
        // prefix stays on line 1 so nothing is shifted.
        let out = run(
            &RunCodeTool::new("."),
            json!({
                "code": "const a = 1;\nconst b = 2;\nthrow new Error('boom');",
                "timeoutMs": 30_000,
            }),
        )
        .expect("a thrown program still yields a ToolOutput");
        assert!(out.is_error, "thrown program must be an error result");
        let text = output_text(&out);
        assert!(text.contains("boom"), "{text}");
        assert!(text.contains("ptc-program:3"), "{text}");
        assert_eq!(details_str(&out, "runtime").as_deref(), Some("quickjs"));
    }

    #[test]
    fn program_timeout_is_enforced() {
        // A program that neither returns nor settles must be interrupted at the
        // deadline, not hang the caller.
        let tool = RunCodeTool::new(".");
        let started = Instant::now();
        let text = match run(
            &tool,
            json!({ "code": "await new Promise(() => {});", "timeoutMs": 800 }),
        ) {
            Ok(output) => {
                assert!(output.is_error, "a never-settling program must error");
                output_text(&output)
            }
            Err(err) => err.to_string(),
        };
        assert!(text.contains("PTC_DEADLINE"), "{text}");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "timeout must fire promptly, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn console_output_is_captured() {
        let out = run(
            &RunCodeTool::new("."),
            json!({ "code": "console.log('hello-console'); return 'ok';", "timeoutMs": 30_000 }),
        )
        .expect("run");
        assert!(!out.is_error);
        assert_eq!(output_text(&out), "ok");
        let console = details_str(&out, "console").expect("console captured");
        assert!(console.contains("hello-console"), "{console}");
    }

    #[test]
    fn realm_has_no_ambient_node_globals() {
        let text = run_text(
            &RunCodeTool::new("."),
            "return [typeof process, typeof require, typeof module, typeof Buffer].join(',');",
        );
        assert_eq!(text, "undefined,undefined,undefined,undefined");
    }

    #[test]
    fn sdk_exposes_no_write_bindings() {
        let text = run_text(
            &RunCodeTool::new("."),
            "return { bash: typeof sdk.bash, write: typeof sdk.write, edit: typeof sdk.edit };",
        );
        assert_eq!(
            text,
            r#"{"bash":"undefined","edit":"undefined","write":"undefined"}"#
        );
    }

    #[test]
    fn runtime_is_reported_in_details() {
        let out = run(
            &RunCodeTool::new("."),
            json!({ "code": "return 1;", "timeoutMs": 30_000 }),
        )
        .expect("run");
        assert_eq!(details_str(&out, "runtime").as_deref(), Some("quickjs"));
        assert_eq!(output_text(&out), "1");
    }

    #[test]
    fn bridge_reaches_bash_and_write_from_program() {
        // No nested authorization: a program may read *and* write.
        let dir = scratch_dir("bridge-write", &[]);
        let code = r"
            await sdk.call('bash', { command: 'echo hi' });
            await sdk.call('write', { path: 'x.txt', content: 'y' });
            return 'ok';
        ";
        let text = run_text(&RunCodeTool::new(&dir), code);
        assert_eq!(text, "ok");
        assert_eq!(
            std::fs::read_to_string(dir.join("x.txt")).expect("written"),
            "y"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn host_tool_errors_reject_the_await() {
        // A host tool failure becomes a JS rejection carrying the host's error
        // text: it never hangs, and it never silently returns a value.
        let missing = "pi-ptc-definitely-missing.txt";
        let out = run(
            &RunCodeTool::new("."),
            json!({ "code": format!("await sdk.read('{missing}');"), "timeoutMs": 30_000 }),
        )
        .expect("a failed bridge call still yields a ToolOutput");
        assert!(out.is_error, "{}", output_text(&out));
        // The failure is attributed to the tool that actually ran. (The `read`
        // tool's not-found text is platform-localized here, so it is asserted
        // by attribution rather than by filename.)
        assert!(output_text(&out).contains("read"), "{}", output_text(&out));

        // Caught: the program keeps running and sees an ordinary Error whose
        // `message` is the host's text, not an empty husk.
        let code = format!(
            "try {{\n  await sdk.read('{missing}');\n  return 'no-error';\n}} catch (err) {{\n  return 'caught:' + typeof (err && err.message) + ':' + (String(err && err.message).length > 0);\n}}"
        );
        assert_eq!(
            run_text(&RunCodeTool::new("."), &code),
            "caught:string:true"
        );
    }

    #[test]
    fn bridge_ls_cannot_escape_the_workspace_roots() {
        // The tool layer is the security boundary the module docs rely on: a
        // bridge call goes through the SAME LsTool a direct call uses, so an
        // absolute directory outside the cwd is refused rather than listed.
        let outside = if cfg!(windows) { "C:/Windows" } else { "/etc" };
        if !std::path::Path::new(outside).is_dir() {
            return;
        }
        let code = format!("await sdk.ls({outside:?});");
        let out = run(
            &RunCodeTool::new("."),
            json!({ "code": code, "timeoutMs": 30_000 }),
        )
        .expect("run");
        assert!(
            out.is_error,
            "listing outside the roots must fail: {}",
            output_text(&out)
        );
        assert!(
            output_text(&out).contains("outside"),
            "{}",
            output_text(&out)
        );
    }

    #[test]
    fn unrepresentable_returns_fall_back_to_text() {
        // Plain strings come back verbatim...
        assert_eq!(run_text(&RunCodeTool::new("."), "return 'hello';"), "hello");
        // ...`undefined` renders as JSON null (the old node contract)...
        assert_eq!(
            run_text(&RunCodeTool::new("."), "return undefined;"),
            "null"
        );
        // ...and values JSON cannot express fall back to their text form
        // instead of failing the whole run.
        assert_eq!(run_text(&RunCodeTool::new("."), "return 5n;"), "5");
        let circular = "const a = {}; a.self = a; return a;";
        assert_eq!(
            run_text(&RunCodeTool::new("."), circular),
            "[object Object]"
        );
    }

    #[test]
    fn effects_union_with_registry() {
        // A bare tool stays at `process()`; with a registry bound, the effects
        // are the union of the tools the session has.
        assert_eq!(RunCodeTool::new(".").effects(), ToolEffects::process());
        let registry =
            crate::tools::ToolRegistry::new(&["run_code", "bash"], std::path::Path::new("."), None);
        let shared = crate::tools::SharedToolRegistry::new(registry);
        let snapshot = shared.snapshot();
        let effects = snapshot.get("run_code").expect("run_code").effects();
        assert!(effects.processes(), "run_code stays a process barrier");
    }

    #[test]
    fn bridge_runs_multiple_bash_commands_in_one_round_trip() {
        let tool = RunCodeTool::new(".");
        let code = r"
            const a = String(await sdk.call('bash', { command: 'echo alpha' }));
            const b = String(await sdk.call('bash', { command: 'echo beta' }));
            return { both: a.includes('alpha') && b.includes('beta'),
                     separate: !a.includes('beta') && !b.includes('alpha') };
        ";
        let out = run(&tool, json!({ "code": code, "timeoutMs": 60_000 })).expect("run");
        assert!(!out.is_error, "{}", output_text(&out));
        let text = output_text(&out);
        assert!(text.contains(r#""both":true"#), "{text}");
        assert!(text.contains(r#""separate":true"#), "{text}");
    }

    #[test]
    fn bridge_reaches_the_real_write_tool() {
        let dir = scratch_dir("grant-write", &[]);
        let tool = RunCodeTool::new(&dir);
        let code = "await sdk.call('write', { path: 'out.txt', content: 'written' }); return 'ok';";
        let out = run(&tool, json!({ "code": code, "timeoutMs": 30_000 })).expect("run");
        assert!(!out.is_error, "{}", output_text(&out));
        assert_eq!(output_text(&out), "ok");
        assert_eq!(
            std::fs::read_to_string(dir.join("out.txt")).expect("file must exist"),
            "written"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn program_source_keeps_line_numbers() {
        let source = program_source("first;\nsecond;");
        assert!(source.starts_with("(async () => {first;"), "{source}");
        assert_eq!(source.lines().nth(1), Some("second;"));
    }

    #[test]
    fn render_program_error_prefers_message() {
        let rendered = render_program_error(&json!({
            "name": "TypeError",
            "message": "x is not a function",
            "stack": "    at <anonymous> (ptc-program:1:1)",
        }));
        assert_eq!(
            rendered,
            "TypeError: x is not a function\n  at ptc-program:1:1"
        );
        assert_eq!(render_program_error(&json!("boom")), "boom");
        // A bare Error name collapses to the message alone.
        assert_eq!(
            render_program_error(&json!({ "name": "Error", "message": "nope" })),
            "nope"
        );
        // Program frames and an error code are carried through.
        assert_eq!(
            render_program_error(&json!({
                "name": "TypeError",
                "message": "x is not a function",
                "code": "ERR_TEST",
                "stack": "    at <anonymous> (ptc-program:4:9)",
            })),
            "TypeError: x is not a function [ERR_TEST]\n  at ptc-program:4:9"
        );
        // A code already spelled out in the message is not duplicated.
        assert_eq!(
            render_program_error(&json!({
                "name": "Error",
                "message": "PTC_EXIT: nope",
                "code": "PTC_EXIT",
            })),
            "PTC_EXIT: nope"
        );
    }

    #[test]
    fn error_frames_keep_only_program_lines() {
        // QuickJS stacks carry no `Error: msg` header, and only frames naming
        // the program survive; eval/native frames are noise.
        let quickjs =
            "    at <anonymous> (ptc-program:3:7)\n    at <eval> (ptc-program:9:1)\n    at native";
        assert_eq!(
            render_error_frames(quickjs),
            vec![
                "  at ptc-program:3:7".to_string(),
                "  at ptc-program:9:1".to_string()
            ]
        );
        // A header line (node-style, kept for robustness) is not a frame.
        assert_eq!(
            render_error_frames("Error: boom\n    at <anonymous> (ptc-program:5:2)"),
            vec!["  at ptc-program:5:2".to_string()]
        );
        // A stack with nothing recognizable yields no frames.
        assert!(render_error_frames("Error: boom\n    at <anonymous>:1:1").is_empty());
    }

    #[test]
    fn frame_site_requires_line_and_column() {
        assert_eq!(
            extract_frame_site("  at ptc-program:12:5)", "ptc-program:").as_deref(),
            Some("ptc-program:12:5")
        );
        // Trailing frame with no closing punctuation still parses.
        assert_eq!(
            extract_frame_site("  at ptc-program:9:4", "ptc-program:").as_deref(),
            Some("ptc-program:9:4")
        );
        // A marker with no digits is not a site.
        assert_eq!(
            extract_frame_site("  at ptc-program:)", "ptc-program:"),
            None
        );
        assert_eq!(
            extract_frame_site("  at other.js:1:2", "ptc-program:"),
            None
        );
    }

    #[test]
    fn console_is_bounded() {
        let long = "x".repeat(PTC_MAX_CONSOLE_BYTES + 100);
        let bounded = bound_console(&long);
        assert!(bounded.ends_with('…'));
        assert!(bounded.len() <= PTC_MAX_CONSOLE_BYTES + '…'.len_utf8());
        assert_eq!(bound_console("short"), "short");
    }

    #[test]
    fn helpers_return_promises() {
        let code = r"
            const r = sdk.ls(undefined);
            return { isPromise: r instanceof Promise, then: typeof r.then, len: String(await r).length > 0 };
        ";
        let out = run(
            &RunCodeTool::new("."),
            json!({ "code": code, "timeoutMs": 30_000 }),
        )
        .expect("run");
        assert!(!out.is_error, "{}", output_text(&out));
        let text = output_text(&out);
        assert!(text.contains(r#""isPromise":true"#), "{text}");
        assert!(text.contains(r#""then":"function""#), "{text}");
        assert!(text.contains(r#""len":true"#), "{text}");
    }

    #[test]
    fn ls_accepts_zero_arguments() {
        assert_eq!(
            run_text(
                &RunCodeTool::new("."),
                "return String(await sdk.ls()).length > 0;"
            ),
            "true"
        );
    }

    #[test]
    fn then_chaining_works() {
        assert_eq!(
            run_text(
                &RunCodeTool::new("."),
                "return await sdk.ls(undefined).then((s) => String(s).length > 0);",
            ),
            "true"
        );
    }

    #[test]
    fn console_never_throws_on_exotic_values() {
        let text = run_text(
            &RunCodeTool::new("."),
            "console.log(Symbol('s'), () => {}, 10n); return 'ok';",
        );
        assert_eq!(text, "ok");
    }

    #[test]
    fn cpu_spin_timeout_reports_deadline() {
        let out = run(
            &RunCodeTool::new("."),
            json!({ "code": "while (true) {}", "timeoutMs": 800 }),
        )
        .expect("a spinning program still yields a ToolOutput");
        assert!(out.is_error, "a spinning program must error");
        assert!(
            output_text(&out).contains("PTC_DEADLINE"),
            "{}",
            output_text(&out)
        );
    }

    #[test]
    fn truncation_is_visible() {
        let text = run_text(
            &RunCodeTool::new("."),
            r"return Array(2100).fill('x').join('\n');",
        );
        assert!(
            text.contains("[truncated: kept first 2000 of 2100 lines"),
            "{text}"
        );
    }

    #[test]
    fn console_surfaces_on_failure() {
        let out = run(
            &RunCodeTool::new("."),
            json!({
                "code": "console.log('debug-line-xyz'); throw new Error('boom');",
                "timeoutMs": 30_000,
            }),
        )
        .expect("a failed program still yields a ToolOutput");
        assert!(out.is_error);
        let text = output_text(&out);
        assert!(text.contains("debug-line-xyz"), "{text}");
        assert!(text.contains("boom"), "{text}");
    }

    #[test]
    fn sdk_tools_lists_the_session_tools() {
        // With a registry bound there is no bridge-level filter: every session
        // tool (bash included) is listed, so the model can see its reach.
        let registry =
            crate::tools::ToolRegistry::new(&["run_code", "bash"], std::path::Path::new("."), None);
        let shared = crate::tools::SharedToolRegistry::new(registry);
        let snapshot = shared.snapshot();
        let tool = snapshot.get("run_code").expect("run_code registered");
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .build()
            .expect("runtime build");
        let out = runtime
            .block_on(tool.execute(
                "t1",
                json!({ "code": "return String(await sdk.tools());", "timeoutMs": 30_000 }),
                None,
            ))
            .expect("run");
        let text = output_text(&out);
        // The bound registry is the reachable set, so `bash` is listed and
        // `run_code` itself is filtered out (it cannot call itself); a tool the
        // registry does not carry (`ast_grep` here) is absent.
        assert!(text.contains("bash"), "{text}");
        assert!(!text.contains("run_code"), "{text}");
        assert!(!text.contains("ast_grep"), "{text}");
    }

    #[test]
    fn live_registry_reaches_tools_the_bridge_cannot_rebuild() {
        // With the registry bound, a call dispatches through the live registry,
        // so a tool the bridge does NOT rebuild standalone (`hashline_edit`) is
        // reachable: the failure must be that tool's own validation error, never
        // `PTC_BRIDGE_DENIED`.
        let registry = crate::tools::ToolRegistry::new(
            &["run_code", "hashline_edit"],
            std::path::Path::new("."),
            None,
        );
        let shared = crate::tools::SharedToolRegistry::new(registry);
        let snapshot = shared.snapshot();
        let tool = snapshot.get("run_code").expect("run_code registered");
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .build()
            .expect("runtime build");
        let out = runtime
            .block_on(tool.execute(
                "t1",
                json!({
                    "code": "try { await sdk.call('hashline_edit', {}); return 'no-error'; } catch (e) { return 'err:' + String(e && e.message); }",
                    "timeoutMs": 30_000,
                }),
                None,
            ))
            .expect("run");
        let text = output_text(&out);
        assert!(
            !text.contains("PTC_BRIDGE_DENIED"),
            "live registry did not dispatch: {text}"
        );
    }

    /// Emits a text block plus an image block, to exercise the marker path.
    struct ImageProbeTool;

    #[async_trait::async_trait]
    impl Tool for ImageProbeTool {
        fn name(&self) -> &str {
            "image_probe"
        }
        fn label(&self) -> &str {
            "image probe"
        }
        fn description(&self) -> &str {
            ""
        }
        fn parameters(&self) -> Value {
            json!({})
        }
        fn effects(&self) -> ToolEffects {
            ToolEffects::read()
        }
        async fn execute(
            &self,
            _tool_call_id: &str,
            _input: Value,
            _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
        ) -> Result<ToolOutput> {
            Ok(ToolOutput {
                content: vec![
                    ContentBlock::Text(TextContent::new("probe-text")),
                    ContentBlock::Image(crate::model::ImageContent {
                        data: "AAAA".to_string(),
                        mime_type: "image/png".to_string(),
                    }),
                ],
                details: None,
                is_error: false,
            })
        }
    }

    #[test]
    fn image_blocks_are_marked_not_dropped() {
        let registry = crate::tools::ToolRegistry::from_tools(vec![
            Box::new(ImageProbeTool),
            Box::new(RunCodeTool::new(".")),
        ]);
        let shared = crate::tools::SharedToolRegistry::new(registry);
        let snapshot = shared.snapshot();
        let tool = snapshot.get("run_code").expect("run_code registered");
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .build()
            .expect("runtime build");
        let out = runtime
            .block_on(tool.execute(
                "t1",
                json!({
                    "code": "return await sdk.call('image_probe', {});",
                    "timeoutMs": 30_000,
                }),
                None,
            ))
            .expect("run");
        let text = output_text(&out);
        assert!(text.contains("probe-text"), "{text}");
        assert!(
            text.contains("[image omitted: image/png"),
            "image block was not surfaced: {text}"
        );
    }

    #[test]
    fn child_bridge_reaches_its_registry_tools() {
        // A delegated child is an ordinary session as far as the bridge is
        // concerned: no nested authorization, so every tool its registry
        // carries is reachable.
        let registry = crate::tools::ToolRegistry::from_tools(vec![
            Box::new(RunCodeTool::new(".")),
            Box::new(BashTool::new(std::path::Path::new("."))),
            Box::new(WriteTool::new(std::path::Path::new("."))),
        ]);
        let shared = crate::tools::SharedToolRegistry::new(registry);
        let snapshot = shared.snapshot();
        let tool = snapshot.get("run_code").expect("run_code registered");
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .build()
            .expect("runtime build");
        let out = runtime
            .block_on(tool.execute(
                "t1",
                json!({
                    "code": "return String(await sdk.call('bash', { command: 'echo hi' })).includes('hi');",
                    "timeoutMs": 30_000,
                }),
                None,
            ))
            .expect("run");
        assert_eq!(output_text(&out), "true");
    }
}
