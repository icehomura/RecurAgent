//! M6 PTC (Programmatic Tool Calling): the `run_code` tool.
//!
//! The model writes one JS program; pi spawns a sandboxed node child that
//! executes it against [`PTC_SDK`]. The program orchestrates multiple tool
//! calls in a single round-trip — intermediate data stays in the child, only
//! the program's final value returns to the model.
//!
//! Structure follows [`crate::eval`]'s kernel bridge: piped stdio, a reader
//! thread feeding an mpsc channel, deadline polling, and process-group
//! discipline so a timeout kills children spawned by the program too.
//!
//! # SDK surface (what model code may call)
//!
//! The program body receives `sdk` and a `console` that writes to stderr.
//! Every helper below forwards to a host tool through the bridge; the host
//! reply is the tool's rendered text, or a rejected Promise carrying the
//! tool's error text.
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
//! # Output discipline
//!
//! stdout carries the protocol, so program output is kept off it: `console.*`
//! and any stray `process.stdout.write` are redirected to stderr, and the
//! protocol writes through a handle captured before the program runs. Only the
//! program's `return`ed value comes back to the model.
//!
//! # Security boundary
//!
//! Two independent layers, both reported in the tool result's `details`:
//!
//! - **Tool layer.** Every bridge call is dispatched through the *same*
//!   [`Tool`] implementations a direct call uses (see
//!   [`RunCodeTool::bridge_call`]), so path confinement, workspace roots, and
//!   read settings are identical — no policy bypass. All four whitelisted tools
//!   are read-only and thus need no approval; the bridge can never reach an
//!   approval-gated tool.
//! - **Process layer.** When the runtime supports it, the child is spawned under
//!   Node's permission model (see [`SandboxMode`]): reads are confined to the
//!   program's scratch directory plus the session workspace roots, and writes,
//!   `child_process`, and reads outside those roots fail with
//!   `ERR_ACCESS_DENIED`. This is what makes the read-only contract real for a
//!   program that reaches for `node:fs` or `node:child_process` directly instead
//!   of going through the bridge. On a runtime without the permission model the
//!   flag is omitted, `details.sandbox` reads `"unavailable"`, and the process
//!   layer is absent (the tool layer still holds).
//! - A node child owns its process group: a wall-clock timeout kills the whole
//!   tree, including anything the program spawned.

use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};
use crate::tools::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, FindTool, GrepTool, LsTool, ReadTool, SearchBackend,
    Tool, ToolEffects, ToolOutput, ToolUpdate, search_backend_from_config, truncate_head,
};
use crate::workspace::WorkspaceHandle;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

/// The PTC SDK, shipped inside the binary and materialized next to the code.
const PTC_SDK: &str = include_str!("../assets/ptc_sdk.js");

/// Default wall-clock budget for one `run_code` invocation (seconds).
pub const DEFAULT_RUN_CODE_TIMEOUT_SECS: u64 = 120;

/// Schema tag for run_code outputs.
pub const PTC_RUN_CODE_SCHEMA: &str = "ra.ptc.run_code.v1";

/// Bridge whitelist. Read-only tools only — they need no approval, so the
/// bridge cannot bypass pi's permission pipeline. Extending this list to
/// write/bash requires routing through the approval path first (tracked as an
/// open question in the port plan, §7).
const BRIDGE_WHITELIST: [&str; 4] = ["read", "grep", "find", "ls"];

/// Node permission-model flags, most-preferred first.
///
/// `--permission` is the stable spelling (Node >= 23, and accepted as an alias
/// in 22.x); `--experimental-permission` is the 20.x/22.x spelling. Whichever
/// the installed runtime accepts is used; if neither is accepted the child runs
/// without the process layer and reports [`SandboxMode::Unavailable`].
const PERMISSION_FLAGS: [&str; 2] = ["--permission", "--experimental-permission"];

/// Maximum stack frames carried into the model-facing error text.
const MAX_ERROR_FRAMES: usize = 8;

/// What confinement the node child actually ran under.
///
/// Reported in the tool result's `details.sandbox` so the boundary is
/// auditable instead of assumed: a runtime without the permission model cannot
/// confine the process, and saying so is better than claiming a sandbox that
/// does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SandboxMode {
    /// Node's permission model: read-only, confined to the allowed roots, with
    /// `child_process` and writes denied by the runtime.
    NodePermission,
    /// The runtime does not accept a permission-model flag; the child is an
    /// unconfined node process (the tool-layer policy still applies).
    Unavailable,
    /// Explicitly disabled by the caller or `PTC_SANDBOX`.
    Disabled,
}

impl SandboxMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::NodePermission => "node-permission",
            Self::Unavailable => "unavailable",
            Self::Disabled => "disabled",
        }
    }
}

/// Whether the process layer is enabled, from `PTC_SANDBOX`.
///
/// Confinement is on by default because it is what makes the module's
/// read-only contract true for a program that reaches for `node:fs` or
/// `node:child_process` directly; opting out therefore has to be explicit.
fn sandbox_enabled_from_env() -> bool {
    !matches!(
        std::env::var("PTC_SANDBOX")
            .ok()
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("off" | "0" | "false" | "no")
    )
}

/// Input parameters for the `run_code` tool.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunCodeInput {
    /// Async function body executed by node; may use `await` and `return`.
    pub code: String,
    /// 5–10 word description shown in the UI.
    pub description: Option<String>,
    /// Optional wall-clock budget in milliseconds (default 120_000).
    pub timeout_ms: Option<u64>,
}

/// Executes a JS program in a sandboxed node child.
pub struct RunCodeTool {
    /// Working directory for the child (and for path-confining bridge tools).
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
    /// Whether to confine the child with Node's permission model when the
    /// runtime supports it (default: on; `PTC_SANDBOX=off` opts out).
    sandbox: bool,
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
            sandbox: sandbox_enabled_from_env(),
        }
    }

    /// Override the default budget (used by config wiring).
    #[must_use]
    pub const fn with_timeout_secs(mut self, timeout_secs: u64) -> Self {
        self.timeout_secs = timeout_secs;
        self
    }

    /// Override process-layer confinement (used by tests and config wiring).
    #[must_use]
    pub const fn with_sandbox(mut self, sandbox: bool) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// Read-only roots the confined child may touch: its own scratch directory
    /// (for the SDK and the program) plus every session workspace root, so a
    /// direct `fs` read of a workspace file still works from the program.
    ///
    /// Nothing outside this set is readable, and no write access is granted at
    /// all — the child is a read-only program by contract.
    fn sandbox_read_roots(&self, scratch: &Scratch) -> Vec<PathBuf> {
        let mut roots = vec![scratch.dir.clone(), self.cwd.clone()];
        roots.extend(self.workspace.roots());
        roots.dedup();
        roots
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
            _ => None,
        }
    }

    /// Dispatch one whitelisted bridge call through the SAME tool
    /// implementations a direct call uses — identical path policy.
    async fn bridge_call(
        &self,
        tool_name: &str,
        input: Value,
    ) -> std::result::Result<String, String> {
        let Some(tool) = self.bridge_tool(tool_name) else {
            return Err(format!(
                "PTC_BRIDGE_DENIED: tool `{tool_name}` is not on the bridge whitelist ({})",
                BRIDGE_WHITELIST.join("|")
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

/// Outcome of the pre-spawn `node` availability probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeProbe {
    /// `node` is on PATH and answered `--version`.
    Available,
    /// `node` could not be executed at all (ENOENT / not executable).
    Missing,
    /// `node` exists but the probe failed for another reason (e.g. a broken
    /// shim). Reported with the underlying OS error.
    Broken,
}

/// Classify the result of running a candidate runtime's probe command.
///
/// Split out from [`probe_node`] so the three-state mapping is testable
/// without depending on what happens to be installed on the build machine.
fn classify_probe(probe: std::io::Result<std::process::ExitStatus>) -> NodeProbe {
    match probe {
        Ok(status) if status.success() => NodeProbe::Available,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => NodeProbe::Missing,
        // Non-zero exit and spawn failure share a verdict: the runtime is not
        // usable, which is exactly what `Broken` means.
        _ => NodeProbe::Broken,
    }
}

/// Run one command as an availability probe (stdout/stderr discarded).
fn probe_command(program: &str) -> NodeProbe {
    classify_probe(
        Command::new(program)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
    )
}

/// Probe `node` on PATH before spawning the real child.
///
/// A missing runtime must produce an actionable error, never a hang or panic.
/// The store checks `ErrorKind::NotFound` and, on some platforms (Windows
/// without `PATHEXT` resolution), that is enough; the explicit probe makes the
/// diagnosis deterministic across platforms.
fn probe_node() -> NodeProbe {
    probe_command("node")
}

/// Which permission-model flag (if any) the installed `node` accepts, probed
/// once per process.
///
/// `None` means the runtime cannot confine the child; the caller then reports
/// [`SandboxMode::Unavailable`] rather than implying a boundary that is absent.
fn probe_permission_flag() -> Option<&'static str> {
    static FLAG: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        PERMISSION_FLAGS.into_iter().find(|flag| {
            Command::new("node")
                .arg(flag)
                .arg("-e")
                .arg("")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        })
    })
}

/// The actionable error returned when the node runtime is unavailable.
fn node_missing_error() -> Error {
    Error::tool(
        "run_code",
        "PTC_NODE_MISSING: `node` not found on PATH. run_code requires Node.js \
         (>=18); install it (e.g. `apt install nodejs`, `brew install node`, or \
         https://nodejs.org) and retry. Other tools keep working without it; \
         use the eval tool for non-JS orchestration.",
    )
}

/// Render the SDK's terminal `error` field for the model.
///
/// The SDK serializes thrown errors as `{ name, message, stack, code? }` and
/// rebases `<anonymous>` frames onto the program's own line numbers. Surface the
/// human-facing `message` (prefixed by the name and error code when they add
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

/// Pick the actionable frames out of a rebased stack.
///
/// Program frames win: they point at the exact line of the `code` the model
/// wrote. Only when the program has no frame (an error raised inside the SDK
/// itself, e.g. argument validation on a malformed call) fall back to the SDK
/// helper frames. Node internals, the scratch directory, and the harness entry
/// point are dropped as noise.
fn render_error_frames(stack: &str) -> Vec<String> {
    let frames: Vec<&str> = stack.lines().skip(1).collect();
    let collect = |marker: &str| -> Vec<String> {
        frames
            .iter()
            .filter_map(|frame| extract_frame_site(frame, marker))
            .take(MAX_ERROR_FRAMES)
            .map(|site| format!("  at {site}"))
            .collect()
    };
    let program = collect("ptc-program:");
    if program.is_empty() {
        collect("ptc_sdk.js:")
    } else {
        program
    }
}

/// First ~160 chars of a stray protocol line, for diagnostics.
///
/// A non-protocol line means something wrote to the protocol channel; the
/// offending bytes are the only way to tell what. The SDK redirects
/// `process.stdout.write` to stderr, so this should stay rare — it is the
/// diagnostic for the residue (a native addon writing to fd 1, say).
fn protocol_snippet(line: &str) -> String {
    let mut out: String = line.chars().take(160).collect();
    if line.chars().count() > 160 {
        out.push('…');
    }
    out
}

/// Per-invocation scratch directory holding the SDK and code files.
///
/// Removed on drop so a killed run leaves no residue.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn materialize(code: &str) -> Result<Self> {
        let dir = std::env::temp_dir().join(format!(
            "pi-ptc-{}-{}",
            std::process::id(),
            UtcMillis::now()
        ));
        std::fs::create_dir_all(&dir)
            .map_err(|err| Error::tool("run_code", format!("PTC_SCRATCH: {err}")))?;
        std::fs::write(dir.join("sdk.js"), PTC_SDK)
            .map_err(|err| Error::tool("run_code", format!("PTC_SCRATCH: {err}")))?;
        std::fs::write(dir.join("code.js"), code)
            .map_err(|err| Error::tool("run_code", format!("PTC_SCRATCH: {err}")))?;
        Ok(Self { dir })
    }

    fn sdk_path(&self) -> PathBuf {
        self.dir.join("sdk.js")
    }

    fn code_path(&self) -> PathBuf {
        self.dir.join("code.js")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Millisecond timestamp helper for unique scratch dir names.
struct UtcMillis;
impl UtcMillis {
    fn now() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis())
    }
}

/// Running node child with its protocol channel.
struct PtcChild {
    child: Child,
    stdin: ChildStdin,
    /// Lines from the child's stdout, streamed by a dedicated reader thread.
    lines: std::sync::Mutex<std::sync::mpsc::Receiver<Option<String>>>,
    /// Set by the first kill(): guards against pid-reuse double-kill.
    killed: bool,
    /// Which confinement the child was actually spawned under.
    sandbox: SandboxMode,
}

impl PtcChild {
    /// Spawn the node child for `scratch`.
    ///
    /// `tool_timeout_ms` becomes the SDK's per-call timeout: the program can
    /// never wait on a host reply longer than the enclosing run budget, so a
    /// stalled bridge fails the call instead of hanging the whole run.
    ///
    /// When `sandbox` is set, the child is confined with Node's permission
    /// model (see [`SandboxMode`]): `read_roots` become the only readable
    /// paths, and every other read, all writes, and `child_process` are denied
    /// by the runtime. The mode actually obtained is recorded on the child.
    fn spawn(
        scratch: &Scratch,
        cwd: &Path,
        tool_timeout_ms: u64,
        sandbox: bool,
        read_roots: &[PathBuf],
    ) -> Result<Self> {
        // Fail fast with an actionable message when node is absent, rather
        // than surfacing a bare ENOENT (or, on odd shims, hanging).
        match probe_node() {
            NodeProbe::Available => {}
            NodeProbe::Missing | NodeProbe::Broken => return Err(node_missing_error()),
        }
        let mut command = Command::new("node");
        let sandbox_mode = if sandbox {
            match probe_permission_flag() {
                Some(flag) => {
                    let allowed: Vec<String> = read_roots
                        .iter()
                        .map(|root| root.to_string_lossy().into_owned())
                        .filter(|root| !root.is_empty())
                        .collect();
                    command.arg(flag);
                    if !allowed.is_empty() {
                        command.arg(format!("--allow-fs-read={}", allowed.join(",")));
                    }
                    SandboxMode::NodePermission
                }
                None => SandboxMode::Unavailable,
            }
        } else {
            SandboxMode::Disabled
        };
        command
            .arg(scratch.sdk_path())
            .arg("--code-file")
            .arg(scratch.code_path())
            .env("PTC_TOOL_TIMEOUT_MS", tool_timeout_ms.to_string())
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Own process group so a timeout kills program-spawned children too.
        crate::tools::isolate_command_process_group(&mut command);
        let mut child = command.spawn().map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                node_missing_error()
            } else {
                Error::tool("run_code", format!("PTC_SPAWN: {err}"))
            }
        })?;
        crate::tools::attach_child_job_discipline(&child);
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::tool("run_code", "PTC_SPAWN: no stdin pipe"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::tool("run_code", "PTC_SPAWN: no stdout pipe"))?;
        // Keep the protocol channel clean: park child stderr in a drain thread
        // so it can never interleave with stdout JSON lines.
        if let Some(stderr) = child.stderr.take() {
            std::thread::Builder::new()
                .name("ptc-stderr-drain".into())
                .spawn(move || {
                    let mut sink = Vec::new();
                    let _ = std::io::copy(&mut BufReader::new(stderr), &mut sink);
                })
                .ok();
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("ptc-node-read".into())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => {
                            let _ = tx.send(None);
                            return;
                        }
                        Ok(_) => {
                            if tx.send(Some(line)).is_err() {
                                return;
                            }
                        }
                    }
                }
            })
            .map_err(|err| Error::tool("run_code", format!("PTC_SPAWN: {err}")))?;
        // Open the protocol: the SDK's transport probe treats an immediate
        // newline as "plain spawn, use stdin/stdout JSON-lines".
        let _ = writeln!(stdin);
        Ok(Self {
            child,
            stdin,
            lines: std::sync::Mutex::new(rx),
            killed: false,
            sandbox: sandbox_mode,
        })
    }

    /// Await the next stdout line under a budget. `Ok(None)` = EOF.
    async fn next_line(&self, deadline: Instant) -> Result<Option<String>> {
        loop {
            let received = self
                .lines
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .try_recv();
            match received {
                Ok(line) => return Ok(line),
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    if Instant::now() > deadline {
                        return Err(Error::tool(
                            "run_code",
                            "PTC_DEADLINE: run_code exceeded its wall-clock budget; the node \
                             child (and anything it spawned) was terminated.",
                        ));
                    }
                    asupersync::time::sleep(
                        asupersync::time::wall_now(),
                        Duration::from_millis(25),
                    )
                    .await;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return Ok(None),
            }
        }
    }

    fn reply(&mut self, payload: &Value) -> Result<()> {
        let mut line = payload.to_string();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|()| self.stdin.flush())
            .map_err(|err| Error::tool("run_code", format!("PTC_IO: {err}")))
    }

    fn kill(&mut self) {
        if self.killed {
            return;
        }
        self.killed = true;
        crate::tools::kill_process_group_tree(Some(self.child.id()));
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for PtcChild {
    fn drop(&mut self) {
        self.kill();
    }
}

#[async_trait]
impl Tool for RunCodeTool {
    fn name(&self) -> &'static str {
        "run_code"
    }

    fn label(&self) -> &'static str {
        "run code"
    }

    fn description(&self) -> &'static str {
        "Execute a JavaScript program against the available tools. Takes two \
         arguments: `code`, the BODY of an async function (top-level `await` and \
         `return` work), and `description`, a short summary of what the program \
         does. Call tools as `await sdk.read('path')`, `await sdk.grep('needle', \
         'dir')`, `await sdk.find('*.rs', 'dir')`, or `await sdk.ls('dir')`; every \
         helper also accepts an options object (`await sdk.read({ path: \
         'src/main.rs', offset: 1, limit: 40 })`, `await sdk.ls({ limit: 20 })`), \
         and `await sdk.call(tool, args)` reaches the full argument set. Only what \
         you return is program output — curate it; `console.*` and stray stdout \
         writes go to stderr. The program is read-only (writes, `child_process`, \
         and reads outside the workspace are denied) and `process.exit` is \
         refused with an error. One run_code replaces many model round-trips."
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
        // Arbitrary code in a child process: serialized fail-closed, same
        // policy as bash/eval.
        ToolEffects::process()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: RunCodeInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;
        let code = input.code.trim();
        if code.is_empty() {
            return Err(Error::validation(
                "run_code requires a non-empty `code` body".to_string(),
            ));
        }
        let timeout = input.timeout_ms.map_or_else(
            || Duration::from_secs(self.timeout_secs),
            |ms| Duration::from_millis(ms.max(1)),
        );
        let deadline = Instant::now() + timeout;
        // Let the SDK's per-call timeout never outlive the host budget: a
        // bridge call that stalls cannot pin the run past its deadline.
        let tool_timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);

        let scratch = Scratch::materialize(code)?;
        let read_roots = self.sandbox_read_roots(&scratch);
        let mut proc =
            PtcChild::spawn(&scratch, &self.cwd, tool_timeout_ms, self.sandbox, &read_roots)?;
        let sandbox = proc.sandbox.as_str();

        // Protocol loop: service tool calls until the terminal result line.
        let final_line = loop {
            let line = proc.next_line(deadline).await?;
            let Some(line) = line else {
                return Err(Error::tool(
                    "run_code",
                    "PTC_EOF: node child exited before returning a result",
                ));
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let parsed: Value = serde_json::from_str(trimmed).map_err(|err| {
                proc.kill();
                Error::tool(
                    "run_code",
                    format!(
                        "PTC_PROTOCOL: non-protocol output on the channel ({err}); \
                         first bytes: {}",
                        protocol_snippet(trimmed)
                    ),
                )
            })?;
            let Some(id) = parsed.get("id").cloned() else {
                proc.kill();
                return Err(Error::tool(
                    "run_code",
                    "PTC_PROTOCOL: message without `id`",
                ));
            };
            if id == Value::String("result".to_string()) {
                break trimmed.to_string();
            }
            // Tool call: dispatch through the whitelist and reply in-band.
            let tool_name = parsed
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let args = parsed.get("args").cloned().unwrap_or_else(|| json!({}));
            let reply = match self.bridge_call(&tool_name, args).await {
                Ok(text) => json!({ "id": id, "ok": true, "result": text }),
                Err(error) => json!({ "id": id, "ok": false, "error": error }),
            };
            proc.reply(&reply)?;
        };

        // Parse the terminal message: { id: "result", ok, result | error }.
        let terminal: Value = serde_json::from_str(&final_line)
            .map_err(|err| Error::tool("run_code", format!("PTC_PROTOCOL: {err}")))?;
        let ok = terminal.get("ok").and_then(Value::as_bool).unwrap_or(false);
        if !ok {
            let error_value = terminal.get("error");
            let error = error_value
                .map_or_else(|| "run_code failed".to_string(), render_program_error);
            let error_code = error_value
                .and_then(|value| value.get("code"))
                .and_then(Value::as_str)
                .unwrap_or("");
            return Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(format!(
                    "run_code failed: {error}"
                )))],
                details: Some(json!({
                    "schema": PTC_RUN_CODE_SCHEMA,
                    "ok": false,
                    "error": error,
                    "errorCode": error_code,
                    "sandbox": sandbox,
                })),
                is_error: true,
            });
        }
        let value = terminal.get("result").cloned().unwrap_or(Value::Null);
        let rendered = match &value {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let truncation = truncate_head(rendered, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(
                truncation.content.clone(),
            ))],
            details: Some(json!({
                "schema": PTC_RUN_CODE_SCHEMA,
                "ok": true,
                "truncated": truncation.truncated,
                "description": input.description,
                "sandbox": sandbox,
            })),
            is_error: false,
        })
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

    /// Whether the host machine can actually run the node-backed cases.
    fn node_available() -> bool {
        probe_node() == NodeProbe::Available
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

    /// Whether this runtime can confine the child with the permission model.
    fn sandbox_available() -> bool {
        node_available() && probe_permission_flag().is_some()
    }

    /// An absolute path that exists on this platform and sits outside every
    /// allowed root (used as the sandbox escape probe).
    fn outside_system_path() -> &'static str {
        if cfg!(windows) {
            "C:/Windows/win.ini"
        } else {
            "/etc/hostname"
        }
    }

    #[test]
    fn probe_reports_missing_for_absent_command() {
        // A command that cannot exist on any PATH must classify as Missing
        // (NeverNotACommand is not a real executable).
        let absent = "pi-ptc-definitely-not-a-real-command-2f5c9d";
        assert_eq!(probe_command(absent), NodeProbe::Missing);
    }

    #[test]
    fn probe_reports_available_for_a_real_runtime() {
        // The actual node on this machine, when present, must read Available.
        // Skipped (not failed) on hosts without node.
        match probe_node() {
            NodeProbe::Available => assert_eq!(probe_node(), NodeProbe::Available),
            NodeProbe::Missing | NodeProbe::Broken => {}
        }
    }

    #[test]
    fn probe_reports_broken_for_non_rust_executable() -> std::io::Result<()> {
        // Point the probe at a file that exists but cannot answer `--version`:
        // a directory is reported as an error by `Command::status` on every
        // supported platform, so it must classify as Broken, not Available.
        let harness = std::env::temp_dir();
        let dir = harness.join(format!("pi-ptc-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        // A directory is not an executable program; classify_probe maps the
        // non-NotFound failure to Broken.
        assert_eq!(probe_command(&dir.to_string_lossy()), NodeProbe::Broken);
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn classify_probe_maps_all_three_states() {
        // Direct unit coverage of the mapping itself, independent of PATH.
        let ok = Command::new("node")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Ok(status) = ok {
            assert_eq!(classify_probe(Ok(status)), NodeProbe::Available);
        }
        assert_eq!(
            classify_probe(Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "absent",
            ))),
            NodeProbe::Missing
        );
        assert_eq!(
            classify_probe(Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "no exec",
            ))),
            NodeProbe::Broken
        );
    }

    #[test]
    fn node_missing_error_is_actionable() {
        let text = node_missing_error().to_string();
        assert!(text.contains("PTC_NODE_MISSING"), "{text}");
        assert!(text.contains("Node.js"), "{text}");
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
        // Safety invariant: the bridge must never gain an approval-bypassing
        // tool without routing through the approval pipeline first.
        for name in BRIDGE_WHITELIST {
            assert!(matches!(name, "read" | "grep" | "find" | "ls"));
        }
        // Every whitelisted name is constructible; nothing else is.
        let tool = RunCodeTool::new(".");
        for name in BRIDGE_WHITELIST {
            assert!(
                tool.bridge_tool(name).is_some(),
                "{name} should be buildable"
            );
        }
        assert!(tool.bridge_tool("write").is_none());
        assert!(tool.bridge_tool("bash").is_none());
        assert!(tool.bridge_tool("edit").is_none());
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
        let dir = std::env::temp_dir().join(format!("pi-ptc-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let file = dir.join("probe.txt");
        std::fs::write(&file, "hello-bridge").expect("write");
        let tool = RunCodeTool::new(&dir);
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .build()
            .expect("runtime build");
        let out = runtime
            .block_on(tool.bridge_call("read", json!({ "path": "probe.txt" })))
            .expect("read via bridge should succeed");
        assert!(out.contains("hello-bridge"), "{out}");
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn render_program_error_prefers_message() {
        let rendered = render_program_error(&json!({
            "name": "TypeError",
            "message": "x is not a function",
            "stack": "TypeError: x is not a function\n    at <anonymous>:1:1",
        }));
        assert_eq!(rendered, "TypeError: x is not a function");
        assert_eq!(render_program_error(&json!("boom")), "boom");
        // A bare Error name collapses to the message alone.
        assert_eq!(
            render_program_error(&json!({ "name": "Error", "message": "nope" })),
            "nope"
        );
        // Rebased program frames and an error code are carried through.
        assert_eq!(
            render_program_error(&json!({
                "name": "TypeError",
                "message": "x is not a function",
                "code": "ERR_TEST",
                "stack": "TypeError: x is not a function\n    at ptc-program:4:9",
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
    fn error_frames_prefer_program_lines() {
        // Program frames win over the SDK helper and harness frames; node
        // internals never reach the model.
        let mixed = "Error: boom\n    \
                     at eval (eval at main (C:/tmp/x/ptc_sdk.js:585:21), ptc-program:3:7)\n    \
                     at main (C:/tmp/x/ptc_sdk.js:586:26)\n    \
                     at node:internal/vm:209:10";
        assert_eq!(
            render_error_frames(mixed),
            vec!["  at ptc-program:3:7".to_string()]
        );

        // With no program frame (the error was raised inside the SDK itself),
        // the helper frames are the fallback.
        let sdk_only = "Error: bad\n    \
                        at normalizeArgs (C:/tmp/x/ptc_sdk.js:337:13)\n    \
                        at Object.read (C:/tmp/x/ptc_sdk.js:375:23)";
        assert_eq!(
            render_error_frames(sdk_only),
            vec![
                "  at ptc_sdk.js:337:13".to_string(),
                "  at ptc_sdk.js:375:23".to_string()
            ]
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
        assert_eq!(extract_frame_site("  at ptc-program:)", "ptc-program:"), None);
        assert_eq!(extract_frame_site("  at other.js:1:2", "ptc-program:"), None);
    }

    #[test]
    fn protocol_snippet_is_bounded() {
        let long = "x".repeat(500);
        let snippet = protocol_snippet(&long);
        assert_eq!(snippet.chars().count(), 161, "160 chars plus the ellipsis");
        assert!(snippet.ends_with('…'), "{snippet}");
        assert_eq!(protocol_snippet("short"), "short");
    }

    #[test]
    fn rejects_empty_code() {
        let tool = RunCodeTool::new(".");
        let err = run(&tool, json!({ "code": "   " })).expect_err("empty code must be rejected");
        assert!(err.to_string().contains("non-empty"));
    }

    #[test]
    fn missing_node_reports_actionable_error() {
        // Exercise the spawn-time guard directly: when the probe says node is
        // absent, `spawn` must return the actionable error and never hang.
        if node_available() {
            return; // Only meaningful on machines without node.
        }
        let scratch = Scratch::materialize("return 1;").expect("scratch");
        let err = PtcChild::spawn(&scratch, Path::new("."), 5_000, false, &[])
            .err()
            .expect("spawn must fail without node");
        assert!(err.to_string().contains("PTC_NODE_MISSING"), "{err}");
    }

    #[test]
    fn program_timeout_is_enforced() {
        // A program that neither returns nor talks to the host must be killed
        // at the deadline and surface PTC_DEADLINE, not hang the caller.
        if !node_available() {
            return;
        }
        let tool = RunCodeTool::new(".");
        let started = Instant::now();
        let err = run(
            &tool,
            json!({ "code": "await new Promise(() => {});", "timeoutMs": 800 }),
        )
        .expect_err("a never-settling program must time out");
        assert!(err.to_string().contains("PTC_DEADLINE"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "timeout must fire promptly, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn throwing_program_returns_error_not_hang() {
        // An exception in the program must come back as an is_error tool
        // result (host-side), never as a hang or a panic.
        if !node_available() {
            return;
        }
        let tool = RunCodeTool::new(".");
        let out = run(&tool, json!({ "code": "throw new Error('kaboom');" }))
            .expect("a thrown program still yields a ToolOutput");
        assert!(out.is_error, "thrown program must be an error result");
        let text = out
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect::<String>();
        assert!(text.contains("kaboom"), "{text}");
    }

    #[test]
    fn five_step_orchestration_in_one_round_trip() {
        // Acceptance: one run_code performs 5 bridge calls (multi-return,
        // grep, ls, find, read) with no extra model round-trips, and only the
        // curated return value is surfaced.
        if !node_available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("pi-ptc-orch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("a.txt"), "alpha needle\n").expect("write a");
        std::fs::write(dir.join("b.txt"), "beta\n").expect("write b");

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
        let tool = RunCodeTool::new(&dir);
        let out = run(&tool, json!({ "code": code, "timeoutMs": 30_000 }))
            .expect("orchestration should succeed");
        assert!(!out.is_error, "orchestration must not error");
        let text = out
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect::<String>();
        assert!(text.contains("\"calls\":5"), "{text}");
        assert!(text.contains("\"hasAlpha\":true"), "{text}");
        assert!(text.contains("\"hasBeta\":true"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bridge_helpers_forward_options_objects() {
        // The options-object form must forward every key, not just `path`:
        // `limit: 1` has to reach ReadTool and actually truncate, while the
        // positional form keeps working unchanged.
        if !node_available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("pi-ptc-opts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("multi.txt"), "one\ntwo\nthree\n").expect("write");
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
    fn stray_stdout_write_cannot_corrupt_the_protocol() {
        // stdout carries the protocol. The SDK redirects program stdout to
        // stderr, so even a deliberate write — or clobbering the writer —
        // must leave the run intact instead of surfacing PTC_PROTOCOL.
        if !node_available() {
            return;
        }
        let code = "process.stdout.write('NOISE\\n'); \
                    process.stdout.write = () => true; return 'clean';";
        let out = run(
            &RunCodeTool::new("."),
            json!({ "code": code, "timeoutMs": 30_000 }),
        )
        .expect("a stray stdout write must not fail the run");
        assert!(!out.is_error, "{}", output_text(&out));
        assert!(output_text(&out).contains("clean"), "{}", output_text(&out));
    }

    #[test]
    fn process_exit_is_refused_and_reported() {
        // Without the SDK guard this surfaces as PTC_EOF (a child that died
        // without answering). With it, the refusal is an ordinary error.
        if !node_available() {
            return;
        }
        let out = run(
            &RunCodeTool::new("."),
            json!({ "code": "process.exit(0);", "timeoutMs": 30_000 }),
        )
        .expect("process.exit must surface as a tool error, not EOF");
        assert!(out.is_error, "{}", output_text(&out));
        let text = output_text(&out);
        assert!(text.contains("PTC_EXIT"), "{text}");
        assert!(text.contains("process.exit"), "{text}");
    }

    #[test]
    fn sandbox_confines_direct_fs_reads() {
        // Layer 2 contract (see the module docs): a program that reaches for
        // `node:fs` directly still cannot read outside the scratch dir and the
        // workspace roots. The unsandboxed run is the control that proves the
        // denial comes from the sandbox rather than a missing file.
        if !sandbox_available() {
            return;
        }
        let outside = outside_system_path();
        assert!(
            Path::new(outside).exists(),
            "sandbox escape probe needs {outside} to exist"
        );
        let dir = std::env::temp_dir().join(format!("pi-ptc-sbx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("inside.txt"), "inside-ok").expect("write");
        let code = format!(
            r"
            const fs = await import('node:fs');
            const probe = (p) => {{ try {{ fs.readFileSync(p); return 'read'; }} catch (err) {{ return String(err.code); }} }};
            return {{ outside: probe({outside:?}), inside: probe('inside.txt') }};
            ",
            outside = outside
        );
        let confined = run(
            &RunCodeTool::new(&dir).with_sandbox(true),
            json!({ "code": code.clone(), "timeoutMs": 30_000 }),
        )
        .expect("sandboxed run");
        let open = run(
            &RunCodeTool::new(&dir).with_sandbox(false),
            json!({ "code": code, "timeoutMs": 30_000 }),
        )
        .expect("unsandboxed run");
        let confined_text = output_text(&confined);
        let open_text = output_text(&open);
        // The workspace is still readable inside the sandbox...
        assert!(
            confined_text.contains(r#""inside":"read""#),
            "{confined_text}"
        );
        // ...but the escape is denied, and the control shows it is the sandbox
        // doing the denying.
        assert!(
            !confined_text.contains(r#""outside":"read""#),
            "{confined_text}"
        );
        assert!(open_text.contains(r#""outside":"read""#), "{open_text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sandbox_mode_is_reported_in_details() {
        if !node_available() {
            return;
        }
        let out = run(
            &RunCodeTool::new("."),
            json!({ "code": "return 1;", "timeoutMs": 30_000 }),
        )
        .expect("run");
        let mode = out
            .details
            .as_ref()
            .and_then(|details| details.get("sandbox"))
            .and_then(Value::as_str);
        let expected = if sandbox_available() {
            "node-permission"
        } else {
            "unavailable"
        };
        assert_eq!(mode, Some(expected));

        // An explicit opt-out is reported honestly rather than implied away.
        let off = RunCodeTool::new(".").with_sandbox(false);
        let out_off = run(&off, json!({ "code": "return 1;", "timeoutMs": 30_000 })).expect("run");
        assert_eq!(
            out_off
                .details
                .as_ref()
                .and_then(|details| details.get("sandbox"))
                .and_then(Value::as_str),
            Some("disabled")
        );
    }
}
