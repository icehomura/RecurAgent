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
//! `hashline`, `encoding`, `glob`, `context`, ... all take effect):
//!
//! | Call | Host tool |
//! |------|-----------|
//! | `sdk.read(path)` / `sdk.read({ path, offset, limit, hashline, encoding })` | `read` |
//! | `sdk.grep(pattern, path?)` / `sdk.grep({ pattern, path, glob, ignoreCase, literal, context, limit, hashline })` | `grep` |
//! | `sdk.find(glob, path?)` / `sdk.find({ pattern, path, limit })` | `find` |
//! | `sdk.ls(path?)` / `sdk.ls({ path, limit })` | `ls` |
//! | `sdk.call(tool, args)` | any **whitelisted** tool (escape hatch, still whitelist-gated) |
//!
//! The SDK exposes only the four read-only whitelisted tools plus the `call`
//! escape hatch. `write`, `edit`, and `bash` are intentionally **absent** —
//! they are outside [`BRIDGE_WHITELIST`], so exposing them would advertise
//! bindings the host always rejects. Adding them requires routing through the
//! approval pipeline first (port plan §7).
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
//! - **Capability layer.** The realm has no ambient I/O: no `fs`, no
//!   `child_process`, no `require`, no network, no `process`. *Only* the
//!   injected `sdk` object can reach outside, and every call is dispatched
//!   through the *same* [`Tool`] implementations a direct call uses (see
//!   [`RunCodeTool::bridge_call`]), so path confinement, workspace roots, and
//!   read settings are identical — no policy bypass.
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

/// Bridge whitelist. Read-only tools only — they need no approval, so the
/// bridge cannot bypass pi's permission pipeline for these.
const BRIDGE_WHITELIST: [&str; 4] = ["read", "grep", "find", "ls"];

/// Approval-gated tools a run may reach only when the OPERATOR granted them at
/// construction ([`RunCodeTool::with_capabilities`]).
///
/// They must never become reachable because the *program* asked. `run_code`
/// executes model-authored code, so honouring a model-supplied grant would be
/// self-escalation past the per-call approval gate in the agent loop
/// (`ToolApprovalHandler`); the grant is therefore a property of the tool
/// instance, decided outside the model's control.
const GRANTABLE_TOOLS: [&str; 3] = ["bash", "write", "edit"];

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
/// bounded by [`PTC_MAX_STACK_BYTES`]; this covers the Rust frames around it.
const REALM_THREAD_STACK_BYTES: usize = 32 * 1024 * 1024;

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
    /// Operator-granted, approval-gated tools the bridge may reach (empty =
    /// read-only, the default).
    capabilities: Vec<&'static str>,
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
            capabilities: capabilities_from_env(),
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

    /// Grant approval-gated tools to this bridge instance.
    ///
    /// An operator decision, never a model one (see [`GRANTABLE_TOOLS`]). Names
    /// outside that list are dropped, so a typo cannot silently widen the
    /// surface. This is the seam the config/CLI wiring should call once
    /// `run_code` capabilities become a real setting; `PTC_CAPABILITIES` is the
    /// interim source.
    #[must_use]
    pub(crate) fn with_capabilities(mut self, capabilities: Vec<&'static str>) -> Self {
        self.capabilities = capabilities
            .into_iter()
            .filter(|name| GRANTABLE_TOOLS.contains(name))
            .collect();
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
            // Approval-gated tools. Constructed exactly the way the registry
            // builds them, but reachable only when the operator granted them:
            // `bridge_call` gates on `allowed` before it ever gets here.
            "bash" => Some(Box::new(BashTool::new(&self.cwd))),
            "write" => Some(Box::new(
                WriteTool::new(&self.cwd).with_workspace(self.workspace.clone()),
            )),
            "edit" => Some(Box::new(
                EditTool::new(&self.cwd).with_workspace(self.workspace.clone()),
            )),
            _ => None,
        }
    }

    /// Whether the bridge may reach `tool_name`: the read-only default plus
    /// whatever the operator granted.
    fn allowed(&self, tool_name: &str) -> bool {
        BRIDGE_WHITELIST.contains(&tool_name) || self.capabilities.contains(&tool_name)
    }

    /// Dispatch one whitelisted bridge call through the SAME tool
    /// implementations a direct call uses — identical path policy.
    async fn bridge_call(
        &self,
        tool_name: &str,
        input: Value,
    ) -> std::result::Result<String, String> {
        if !self.allowed(tool_name) {
            return Err(format!(
                "PTC_BRIDGE_DENIED: tool `{tool_name}` was not granted to this run \
                 (read-only default: {}; operator-grantable: {})",
                BRIDGE_WHITELIST.join("|"),
                GRANTABLE_TOOLS.join("|")
            ));
        }
        let Some(tool) = self.bridge_tool(tool_name) else {
            return Err(format!(
                "PTC_BRIDGE_DENIED: `{tool_name}` is not a bridge tool"
            ));
        };
        match tool.execute("run-code-bridge", input, None).await {
            Ok(output) => {
                let mut text = String::new();
                for block in &output.content {
                    if let ContentBlock::Text(t) = block {
                        text.push_str(&t.text);
                    }
                }
                if output.is_error { Err(text) } else { Ok(text) }
            }
            Err(err) => Err(err.to_string()),
        }
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
    fn spawn(code: &str, budget: Duration) -> Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flags = RealmFlags::new(budget, Arc::clone(&cancelled));
        let code = code.to_string();
        std::thread::Builder::new()
            .name("ptc-quickjs".into())
            .stack_size(REALM_THREAD_STACK_BYTES)
            .spawn(move || realm_thread(&code, &flags, &tx, budget))
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

fn realm_thread(code: &str, flags: &RealmFlags, tx: &Sender<RealmMessage>, budget: Duration) {
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
                match context.with(|ctx| install_globals(&ctx, &console, tx, flags, budget)) {
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

/// Operator grant for the approval-gated bridge tools, from `PTC_CAPABILITIES`.
///
/// Interim wiring until the config/CLI carries it: the value is read once at
/// construction, from the environment of whatever launched the agent — a human
/// decision, not something model-authored code can set. Unrecognized names are
/// dropped by [`RunCodeTool::with_capabilities`].
fn capabilities_from_env() -> Vec<&'static str> {
    let Ok(raw) = std::env::var("PTC_CAPABILITIES") else {
        return Vec::new();
    };
    GRANTABLE_TOOLS
        .into_iter()
        .filter(|name| raw.split(',').any(|part| part.trim() == *name))
        .collect()
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
) -> rquickjs::Result<()> {
    let globals = ctx.globals();

    // console.* appends to the per-run buffer; the host surfaces it in
    // `details.console` instead of letting it reach the protocol.
    let console_obj = Object::new(ctx.clone())?;
    for level in ["log", "info", "debug", "warn", "error"] {
        let sink = Rc::clone(console);
        let func = Func::from(move |parts: Rest<Coerced<String>>| {
            let text: Vec<String> = parts.0.into_iter().map(|part| part.0).collect();
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

    // Escape hatch: full argument set for any whitelisted tool. The host is
    // still the thing that decides whether the tool exists.
    let call_tx = tx.clone();
    let call_flags = flags.clone();
    let call = Func::from(
        move |ctx: Ctx<'js>, tool: String, args: Opt<JsValue<'js>>| {
            let payload = args
                .0
                .as_ref()
                .and_then(|value| json_arg(&ctx, value))
                .unwrap_or_else(|| json!({}));
            bridge_call(&ctx, &call_tx, &call_flags, budget, &tool, payload)
        },
    );
    sdk.set("call", call)?;

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
        move |ctx: Ctx<'js>, first: JsValue<'js>, second: Opt<JsValue<'js>>| {
            let mut args = normalize_args(&ctx, &first, key, name, optional)
                .map_err(|msg| rquickjs::Exception::throw_message(&ctx, &msg))?;
            if scope {
                args = merge_scope(&ctx, args, second.0.as_ref(), name)
                    .map_err(|msg| rquickjs::Exception::throw_message(&ctx, &msg))?;
            }
            bridge_call(&ctx, &tx, &flags, budget, tool, args)
        },
    );
    sdk.set(name, func)
}

/// Send one bridge call and block until the host answers, the run is
/// cancelled, or the budget runs out.
fn bridge_call(
    ctx: &Ctx<'_>,
    tx: &Sender<RealmMessage>,
    flags: &RealmFlags,
    budget: Duration,
    tool: &str,
    args: Value,
) -> rquickjs::Result<String> {
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    if tx
        .send(RealmMessage::Call(BridgeCall {
            tool: tool.to_string(),
            args,
            reply: reply_tx,
        }))
        .is_err()
    {
        return Err(rquickjs::Exception::throw_message(
            ctx,
            "PTC_CANCELLED: the host stopped listening",
        ));
    }
    let deadline = Instant::now() + budget;
    loop {
        match reply_rx.recv_timeout(BRIDGE_POLL_INTERVAL) {
            Ok(Ok(text)) => return Ok(text),
            Ok(Err(err)) => return Err(rquickjs::Exception::throw_message(ctx, &err)),
            Err(RecvTimeoutError::Disconnected) => {
                return Err(rquickjs::Exception::throw_message(
                    ctx,
                    "PTC_CANCELLED: the host dropped the bridge",
                ));
            }
            Err(RecvTimeoutError::Timeout) => {
                if flags.cancelled.load(Ordering::SeqCst) {
                    return Err(rquickjs::Exception::throw_message(
                        ctx,
                        "PTC_CANCELLED: the run was cancelled",
                    ));
                }
                if Instant::now() > deadline {
                    return Err(rquickjs::Exception::throw_message(
                        ctx,
                        &format!(
                            "PTC_BRIDGE_TIMEOUT: tool `{tool}` did not answer within the run budget"
                        ),
                    ));
                }
            }
        }
    }
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

/// Normalize the first helper argument: positional shorthand or options object.
///
/// Mirrors the documented contract — a non-empty string becomes `{ key: value }`,
/// an options object is forwarded verbatim but must carry `key`, and (for `ls`)
/// both may be omitted.
fn normalize_args<'js>(
    ctx: &Ctx<'js>,
    value: &JsValue<'js>,
    key: &str,
    form: &str,
    optional: bool,
) -> std::result::Result<Value, String> {
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
            Err(err) => Phase1::Done(ProgramOutcome::Failed(error_payload(&ctx, &err))),
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
            Some(Err(err)) => ProgramOutcome::Failed(error_payload(&ctx, &err)),
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
         sdk.call(tool, args)` reaches the full argument set. The program runs \
         on the built-in QuickJS engine with no filesystem, process, or network \
         API: `sdk.*` is the only way out, and it only reaches the read-only \
         tools `read`, `grep`, `find`, and `ls`. Errors carry `ptc-program:<line>` \
         frames pointing at your own code; `console.log` is captured separately. \
         Only what you return is program output — curate it. One run_code \
         replaces many model round-trips. The operator may also grant the \
         approval-gated tools `bash`, `write` and `edit` for this session, in \
         which case `await sdk.call('bash', { command: '...' })` and friends are \
         reachable; otherwise such a call is refused with PTC_BRIDGE_DENIED."
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

    fn effects(&self) -> ToolEffects {
        // Arbitrary code execution: serialized fail-closed, same policy as
        // bash/eval. A run whose bridge can reach the write/bash tools declares
        // those effects too, so outer barriers and plan gates see the real
        // reach of the call instead of the read-only default.
        let base = ToolEffects::process();
        if self.capabilities.iter().any(|name| !name.is_empty()) {
            base.union(ToolEffects::write())
        } else {
            base
        }
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

        let realm = JsRealm::spawn(code, timeout)?;
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
            return Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(format!(
                    "run_code failed: {error}"
                )))],
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
        let mut details = json!({
            "schema": PTC_RUN_CODE_SCHEMA,
            "ok": true,
            "truncated": truncation.truncated,
            "description": input.description,
            "runtime": "quickjs",
        });
        attach_console(&mut details, &console);
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(truncation.content))],
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
    fn whitelist_is_read_only() {
        // Safety invariant: the read-only set is the only thing the bridge may
        // reach unless the OPERATOR granted more, and the grant is never
        // something model-authored code can set.
        for name in BRIDGE_WHITELIST {
            assert!(matches!(name, "read" | "grep" | "find" | "ls"));
        }
        // Every read-only name is constructible and always allowed.
        let tool = RunCodeTool::new(".");
        for name in BRIDGE_WHITELIST {
            assert!(tool.allowed(name), "{name} is read-only and always allowed");
            assert!(
                tool.bridge_tool(name).is_some(),
                "{name} should be buildable"
            );
        }
        // Approval-gated tools are constructible for a granted run, but a
        // default run cannot reach them — and the program cannot ask for them.
        for name in GRANTABLE_TOOLS {
            assert!(!tool.allowed(name), "{name} must not be granted by default");
            assert!(
                tool.bridge_tool(name).is_some(),
                "{name} must be constructible when granted"
            );
        }
        assert!(!tool.allowed("ast_edit"));
    }

    #[test]
    fn bridge_denies_non_whitelisted_tool() {
        // Host-side rejection path: `sdk.bash("...")` must fail with the
        // whitelist message and never reach a real bash tool.
        let tool = RunCodeTool::new(".");
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .build()
            .expect("runtime build");
        let err = runtime
            .block_on(tool.bridge_call("bash", json!({ "command": "echo hi" })))
            .expect_err("bash must be denied by the bridge");
        assert!(err.contains("PTC_BRIDGE_DENIED"), "{err}");
        assert!(err.contains("bash"), "{err}");
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
    fn bridge_denial_surfaces_to_program_code() {
        // The whitelist is enforced host-side, but the program must observe the
        // refusal as an ordinary catchable error — not a hang, not a silent
        // no-op, and not an escape.
        let code = r"
            const outcomes = {};
            try { await sdk.call('bash', { command: 'echo hi' }); outcomes.bash = 'allowed'; }
            catch (err) { outcomes.bash = String(err && err.message); }
            try { await sdk.call('write', { path: 'x', content: 'y' }); outcomes.write = 'allowed'; }
            catch (err) { outcomes.write = String(err && err.message); }
            return outcomes;
        ";
        let text = run_text(&RunCodeTool::new("."), code);
        assert!(!text.contains("allowed"), "{text}");
        assert!(text.contains("PTC_BRIDGE_DENIED"), "{text}");
        assert!(text.contains("bash"), "{text}");
        assert!(text.contains("write"), "{text}");
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
    fn bash_grant_is_off_by_default() {
        // There is no model-supplied grant: the read-only set is always there,
        // and the approval-gated tools are not.
        let tool = RunCodeTool::new(".");
        assert!(tool.allowed("read"));
        assert!(tool.allowed("grep"));
        assert!(!tool.allowed("bash"));
        assert!(!tool.allowed("write"));
        assert!(!tool.allowed("edit"));
        assert!(tool.capabilities.is_empty());
    }

    #[test]
    fn grant_is_limited_to_grantable_names() {
        // A typo (or an attempt to grant something outside the list) cannot
        // widen the surface.
        let tool = RunCodeTool::new(".").with_capabilities(vec!["bash", "read", "nope"]);
        assert!(tool.allowed("bash"));
        assert!(tool.allowed("read"));
        assert!(!tool.allowed("nope"));
        assert_eq!(tool.capabilities, vec!["bash"]);
    }

    #[test]
    fn granting_bash_escalates_declared_effects() {
        // Outer barriers and plan gates must see the real reach of the call.
        assert_eq!(RunCodeTool::new(".").effects(), ToolEffects::process());
        let granted = RunCodeTool::new(".")
            .with_capabilities(vec!["bash"])
            .effects();
        assert!(granted.processes() && granted.writes(), "{granted:?}");
    }

    #[test]
    fn granted_bash_runs_multiple_commands_and_combines_output() {
        // The point of the grant: several commands in ONE round trip, combined
        // with program logic instead of a shell pipeline.
        let tool = RunCodeTool::new(".").with_capabilities(vec!["bash"]);
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
    fn granted_write_reaches_the_real_write_tool() {
        let dir = scratch_dir("grant-write", &[]);
        let tool = RunCodeTool::new(&dir).with_capabilities(vec!["write"]);
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
}
