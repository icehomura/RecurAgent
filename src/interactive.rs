//! Shared interactive-support surface for the FrankenTUI stack.
//!
//! This module used to host the classic charmed_rust/bubbletea front-end. That
//! stack has been removed; what remains here is the stack-independent surface
//! the ftui driver shares: the [`RaMsg`] agent-event vocabulary, the status
//! snapshot, the async→UI enqueue helpers, VCS/model display helpers, and the
//! re-exports of the shared conversation/command/tree modules.

use asupersync::Cx;
use asupersync::channel::mpsc;
use asupersync::sync::{Mutex, OwnedMutexGuard};
use async_trait::async_trait;
use chrono::Utc;
use glob::Pattern;
use serde_json::{Value, json};

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::agent::{Agent, QueuedAgentMessage, SessionActionAdmissionGate};
use crate::config::Config;
use crate::extensions::{
    ExtensionDeliverAs, ExtensionHostActions, ExtensionManager, ExtensionSendMessage,
    ExtensionSendUserMessage, ExtensionSession, ExtensionUiRequest, ExtensionUiResponse,
};
use crate::model::{ContentBlock, CustomMessage, Message as ModelMessage, StopReason, Usage};
use crate::models::ModelEntry;
use crate::resources::{DiagnosticKind, ResourceDiagnostic, ResourceLoader};
use crate::session::{Session, SessionEntry, SessionMessage};

#[cfg(all(feature = "clipboard", feature = "image-resize"))]
use arboard::Clipboard as ArboardClipboard;

mod commands;
mod conversation;
mod ext_session;
mod file_refs;
/// Crate-visible because the ftui stack drives the same `/share` implementation
/// rather than growing a second copy of it (bd-ydz1t.1). Only `run_share` and
/// `ShareOutcome` are exported; everything else stays private to this stack.
pub(crate) mod login_flow;
pub(crate) mod share;
mod state;
mod terminal_text;
mod text_utils;
mod tool_summary;
mod tree;
pub(crate) mod workspace_reports;

#[cfg(feature = "ftui")]
pub(crate) use self::tool_summary::tool_invocation_summary;
use self::tool_summary::build_user_message;
pub(crate) use self::tool_summary::extension_commands_for_catalog;
/// Shared with the ftui stack so `/copy` behaves and reports identically on
/// both; see the function's own note.
pub(crate) use self::commands::{COPY_OK_MESSAGE, copy_text_to_clipboard};
pub use self::commands::{
    SlashCommand, model_entry_matches, parse_scoped_model_patterns, resolve_scoped_model_entries,
    strip_thinking_level_suffix,
};
// Session→conversation snapshot; re-exported for the ftui migration stack
// (bd-cv653.9.1) to rebuild its transcript after /resume.
pub use self::conversation::conversation_from_session;
pub(crate) use self::tree::{fork_candidates, format_fork_candidates, select_fork_candidate};

/// Where `/export` writes when it is given no argument.
///
/// Free rather than a `RaApp` method because the ftui stack runs the same
/// `/export` (bd-cv653) and must not name its files differently: an exported
/// conversation should land in the same place whichever stack wrote it.
pub(crate) fn default_export_path(cwd: &Path, session: &Session) -> PathBuf {
    if let Some(path) = session.path.as_ref() {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("session");
        return cwd.join(format!("pi-session-{stem}.html"));
    }
    let id = crate::session_picker::truncate_session_id(&session.header.id, 8);
    cwd.join(format!("pi-session-unsaved-{id}.html"))
}

/// Resolve an explicit `/export <path>` argument against the working
/// directory. A relative path is joined; an absolute one is taken as given.
pub(crate) fn resolve_output_path(cwd: &Path, raw: &str) -> PathBuf {
    let raw = raw.trim();
    if raw.is_empty() {
        return cwd.join("pi-session.html");
    }
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}
pub use self::ext_session::{format_extension_ui_prompt, parse_extension_ui_response};
use self::file_refs::format_file_ref;
pub(crate) use self::file_refs::{
    extract_file_references, looks_like_dropped_paths, normalize_pasted_file_refs,
};
pub use self::state::{AgentState, InputMode, PendingInput};
// Shared with the ftui stack (issue #208): one dropdown state machine, one
// command catalog, so slash-command completion cannot drift between surfaces.
pub(crate) use self::state::AutocompleteState;
pub use self::state::{ConversationMessage, MessageRole};
use self::state::{InjectedMessageQueue, InteractiveMessageQueue, QueuedMessageKind};
use self::text_utils::truncate;
pub(crate) async fn enqueue_pi_event(event_tx: &mpsc::Sender<RaMsg>, cx: &Cx, msg: RaMsg) -> bool {
    event_tx.send(cx, msg).await.is_ok()
}

pub(crate) async fn enqueue_ui_shutdown(event_tx: &mpsc::Sender<RaMsg>, cx: &Cx) {
    let _ = enqueue_pi_event(event_tx, cx, RaMsg::UiShutdown).await;
}

/// What the FTUI powerline status line shows (OMP-style: display name, not
/// the provider/id identity), captured by the driver after every command.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FtuiStatusSnapshot {
    /// The model's display label: catalog/`models.json` name, else the id.
    pub model: String,
    pub thinking: Option<String>,
    /// Plan mode (`act` when off).
    pub mode: String,
    pub cwd: String,
    pub vcs: Option<String>,
    /// Last prompt's context use as a percentage of the model's window.
    pub context_pct: u8,
    pub cost_usd: f64,
    pub tokens: u64,
    pub session_name: String,
}

#[derive(Debug, Clone)]
pub enum RaMsg {
    /// Agent started processing.
    AgentStart,
    /// Trigger processing of the next queued input (CLI startup messages).
    RunPending,
    /// Enqueue an input only while its originating session remains current.
    EnqueuePendingInput {
        session_id: String,
        input: PendingInput,
    },
    /// Internal: shut down the async→UI message bridge (used for clean exit).
    UiShutdown,
    /// Host-driven terminal (tab) title update (issue #200). Emitted by
    /// driver commands (`/name`, `/resume`, `/new`) for surfaces whose
    /// renderer cannot embed OSC sequences in frame content (ftui); the
    /// charmed stack ignores it because its header re-emits the title every
    /// frame.
    TerminalTitle(String),
    /// FTUI `/login` state (the charmed stack keeps its own pending login and
    /// ignores this). `Some(provider)` routes the next submitted line to the
    /// login as a code or key, never echoed into the transcript; `None` ends
    /// that. `accepts_empty_input` is true for device flows, where a bare
    /// Enter polls.
    LoginPending {
        provider: Option<String>,
        accepts_empty_input: bool,
    },
    /// FTUI status-line snapshot from the driver, which owns the session
    /// state the line shows. The charmed stack renders its own and ignores
    /// this.
    StatusSnapshot(FtuiStatusSnapshot),
    /// Periodic autocomplete refresh tick (background file index).
    AutocompleteRefresh,
    /// Replacement completion catalog (issue #208). The ftui driver sends it
    /// once its session exists, so extension-contributed commands join the
    /// built-in list; the charmed stack builds its catalog inline and
    /// ignores this.
    AutocompleteCatalog(crate::autocomplete::AutocompleteCatalog),
    /// A model catalog refresh finished (`/model-update` or the startup
    /// background refresh). `models` is the new `provider/id` list; `status`
    /// is the human-readable summary.
    ModelCatalogRefreshed {
        models: Vec<String>,
        status: String,
    },
    /// Text delta from assistant.
    TextDelta(String),
    /// Thinking delta from assistant.
    ThinkingDelta(String),
    /// Tool execution started.
    ToolStart { name: String, tool_id: String },
    /// Human-readable summary of the running tool's invocation (e.g. the bash
    /// command line). Sent immediately after `ToolStart` when derivable.
    ToolInvocation { tool_id: String, summary: String },
    /// Tool execution update (streaming output).
    ToolUpdate {
        name: String,
        tool_id: String,
        content: Vec<ContentBlock>,
        details: Option<Value>,
    },
    /// Tool execution ended. `output` carries an OPTIONAL size-capped text
    /// preview of the tool result (bd-cv653.9.2 diff cards); `None` when
    /// the surface folds output elsewhere (e.g. the ftui bash flow) or the
    /// result had no text content.
    ToolEnd {
        name: String,
        tool_id: String,
        is_error: bool,
        output: Option<String>,
    },
    /// Session todo list changed (bd-cv653.3.9). Carries the compact
    /// `todo_list.v1` summary line for the footer; `None` clears it.
    TodoSummary { summary: Option<String> },
    /// The ask tool needs the user to answer question cards (bd-cv653.3.8).
    AskUiRequest(crate::ask::AskUiRequest),
    /// Agent finished with final message.
    AgentDone {
        usage: Option<Usage>,
        stop_reason: StopReason,
        error_message: Option<String>,
    },
    /// Auto-titling result: a tiny/smol-role model suggested a session name
    /// (bd-cv653.3.1). Applied only if the session is still unnamed.
    SessionTitleSuggestion {
        owner_session_id: String,
        title: String,
    },
    /// Agent error.
    AgentError(String),
    /// Credentials changed for a provider; refresh in-memory provider auth state.
    CredentialUpdated { provider: String },
    /// Non-error system message.
    System(String),
    /// System note that does not mutate agent state (safe during streaming).
    SystemNote(String),
    /// Session-bound system note; discarded if its origin is no longer current.
    SessionSystemNote {
        owner_session_id: String,
        message: String,
    },
    /// Update last user message content (input transform/redaction).
    UpdateLastUserMessage(String),
    /// Bash command result (non-agent).
    BashResult {
        display: String,
        content_for_agent: Option<Vec<ContentBlock>>,
    },
    /// Async OAuth device flow start
    OAuthDeviceFlowStarted {
        provider: String,
        device_code: String,
        user_code: String,
        verification_uri: String,
        expires_in: u64,
    },
    /// Replace conversation state from session (compaction/fork).
    ConversationReset {
        session_id: String,
        messages: Vec<ConversationMessage>,
        usage: Usage,
        status: Option<String>,
    },
    /// Classic `/retry` committed the sibling leaf; reset UI from Session
    /// and enqueue the abandoned prompt without slash-command reparse.
    RetryCommitted {
        session_id: String,
        messages: Vec<ConversationMessage>,
        usage: Usage,
        text: String,
        status: Option<String>,
    },
    /// Set the editor contents (used by /tree selection of user/custom messages).
    SetEditorText {
        owner_session_id: String,
        text: String,
    },
    /// Open the session tree selector (async from extension hooks).
    OpenTree {
        owner_session_id: String,
        initial_selected_id: Option<String>,
        label: Option<String>,
    },
    /// Internal bounded retry for a session-scoped event whose authoritative
    /// Session lock was transiently busy. The boxed event is always the
    /// original owner-tagged event, never another retry envelope.
    SessionEventRetry {
        event: Box<Self>,
        attempts_remaining: u8,
    },
    /// Reloaded skills/prompts/themes/extensions.
    ResourcesReloaded {
        resources: ResourceLoader,
        status: String,
        diagnostics: Option<String>,
    },
    /// Extension UI request (select/confirm/input/editor/custom/notify).
    ExtensionUiRequest(ExtensionUiRequest),
    /// Periodic redraw or final deadline wake for one capability prompt.
    ///
    /// Carries request, prompt, and timer generations so late or duplicated
    /// wakes cannot resolve or rearm a replacement timer/overlay.
    CapabilityPromptTick {
        id: String,
        generation: u64,
        timer_generation: u64,
    },
    /// Extension command finished execution.
    ExtensionCommandDone {
        command: String,
        display: String,
        is_error: bool,
    },
    /// OAuth callback server received the browser redirect.
    /// The string is the full callback URL (e.g. `http://localhost:1455/auth/callback?code=abc&state=xyz`).
    OAuthCallbackReceived(String),
}

/// Read the current git branch from `.git/HEAD` in the given directory.
///
/// Returns `Some("branch-name")` for a normal branch,
/// `Some("abc1234")` (7-char short SHA) for detached HEAD,
/// or `None` if not in a git repo or `.git/HEAD` is unreadable.
fn read_git_branch(cwd: &Path) -> Option<String> {
    let git_head = find_git_head_path(cwd)?;
    let content = std::fs::read_to_string(git_head).ok()?;
    let content = content.trim();
    content.strip_prefix("ref: refs/heads/").map_or_else(
        || {
            // Detached HEAD — show short SHA
            (content.len() >= 7 && content.chars().all(|c| c.is_ascii_hexdigit()))
                .then(|| content[..7].to_string())
        },
        |ref_path| Some(ref_path.to_string()),
    )
}

/// Return whether any ancestor of `cwd` (or `cwd` itself) contains a `.jj`
/// directory. Walks up the tree; no subprocess cost.
fn is_inside_jj_repo(cwd: &Path) -> bool {
    let mut current = cwd.to_path_buf();
    loop {
        if current.join(".jj").is_dir() {
            return true;
        }
        if !current.pop() {
            return false;
        }
    }
}

/// Read the current jj working-copy change via `jj log`, if we are inside
/// a jj repo and the `jj` binary is available. Returns a short display
/// string like `"jj:abc12345 feat: description"`, or `None` if the probe
/// fails for any reason — in which case the caller should fall back to
/// `read_git_branch`.
///
/// We check for `.jj` on disk first so that on the vastly more common
/// pure-git repo we never even fork a subprocess.
fn read_jj_change(cwd: &Path) -> Option<String> {
    if !is_inside_jj_repo(cwd) {
        return None;
    }

    let output = std::process::Command::new("jj")
        .args([
            "log",
            "-r",
            "@",
            "--no-graph",
            "--template",
            r#"change_id.short(8) ++ " " ++ description.first_line()"#,
        ])
        .current_dir(cwd)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let line = String::from_utf8(output.stdout).ok()?;
    let line = line.trim();
    if line.is_empty() {
        return None;
    }

    // Prefix so jj context is visually distinct from a bare git branch
    // name in the status bar (useful in colocated repos).
    Some(format!("jj:{line}"))
}

/// Save the clipboard's image as a temporary PNG and return an `@file`
/// reference to it for the editor.
/// `None` when the clipboard holds no image (or clipboard support is off).
pub(crate) fn paste_clipboard_image_ref() -> Option<String> {
    let path = paste_image_from_clipboard()?;
    Some(format_file_ref(&path.display().to_string()))
}

/// Save the clipboard's image as a PNG under the agent directory and return its
/// path. GH #242: under WSL, ask Windows for the image because arboard has no
/// display there.
fn paste_image_from_clipboard() -> Option<PathBuf> {
    if commands::running_under_wsl()
        && let Some(path) = paste_image_via_powershell()
    {
        return Some(path);
    }

    #[cfg(all(feature = "clipboard", feature = "image-resize"))]
    {
        use image::ImageEncoder;

        let mut clipboard = ArboardClipboard::new().ok()?;
        let image = clipboard.get_image().ok()?;

        let width = u32::try_from(image.width).ok()?;
        let height = u32::try_from(image.height).ok()?;
        let bytes = image.bytes.into_owned();
        let width_usize = usize::try_from(width).ok()?;
        let height_usize = usize::try_from(height).ok()?;
        let expected = width_usize.checked_mul(height_usize)?.checked_mul(4)?;
        if bytes.len() != expected {
            return None;
        }

        // Under the agent dir, not the system temp dir: `@file` reading is
        // confined to the cwd and the agent dir, so a pasted image saved to
        // /tmp could never be attached.
        let dir = crate::config::Config::global_dir().join("pastes");
        std::fs::create_dir_all(&dir).ok()?;
        let mut temp_file = tempfile::Builder::new()
            .prefix("pi-paste-")
            .suffix(".png")
            .tempfile_in(&dir)
            .ok()?;
        let encoder = image::codecs::png::PngEncoder::new(&mut temp_file);
        if encoder
            .write_image(&bytes, width, height, image::ExtendedColorType::Rgba8)
            .is_err()
        {
            return None;
        }
        let (_file, path) = temp_file.keep().ok()?;
        Some(path)
    }

    #[cfg(not(all(feature = "clipboard", feature = "image-resize")))]
    {
        None
    }
}

/// The PowerShell that saves Windows' clipboard image as a PNG at
/// `windows_path` (exit 1 when the clipboard holds no image).
fn powershell_save_clipboard_png(windows_path: &str) -> String {
    let quoted = windows_path.replace('\'', "''");
    format!(
        "Add-Type -AssemblyName System.Windows.Forms; Add-Type -AssemblyName System.Drawing; \
         $img = [System.Windows.Forms.Clipboard]::GetImage(); \
         if ($null -eq $img) {{ exit 1 }}; \
         $img.Save('{quoted}', [System.Drawing.Imaging.ImageFormat]::Png)"
    )
}

/// GH #242: under WSL, have `powershell.exe` save the Windows clipboard's
/// image into the agent dir (where `@file` may read it), through the path
/// `wslpath -w` gives for it. `None` when there is no image or either interop
/// binary is missing. Interop output is discarded so it cannot reach the TUI.
fn paste_image_via_powershell() -> Option<PathBuf> {
    use std::process::{Command, Stdio};
    let dir = crate::config::Config::global_dir().join("pastes");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("pi-paste-{}.png", uuid::Uuid::new_v4().simple()));
    let windows_path = Command::new("wslpath")
        .arg("-w")
        .arg(&path)
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())?;
    let saved = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command"])
        .arg(powershell_save_clipboard_png(windows_path.trim()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if saved && std::fs::metadata(&path).is_ok_and(|meta| meta.len() > 0) {
        return Some(path);
    }
    // A save that failed part-way can leave an empty or partial PNG behind.
    let _ = std::fs::remove_file(&path);
    None
}

/// What to show for a model (gh #214): its models.json `name` when one is set
/// and differs from the id, else `provider/id`. Display only: selection,
/// cycling, and lookups keep using `provider/id`.
pub(crate) fn model_display_label(entry: &ModelEntry) -> String {
    let name = entry.model.name.trim();
    if name.is_empty() || name == entry.model.id {
        format!("{}/{}", entry.model.provider, entry.model.id)
    } else {
        name.to_string()
    }
}

/// A model for `/session`-style listings: the display name with the
/// `provider/id` it resolves to, or just `provider/id` when there is no
/// distinct name.
pub(crate) fn session_model_line(entry: &ModelEntry) -> String {
    let identity = format!("{}/{}", entry.model.provider, entry.model.id);
    let display = model_display_label(entry);
    if display == identity {
        identity
    } else {
        format!("{display} ({identity})")
    }
}

/// Read VCS info for the interactive status bar: prefers jj in colocated
/// repos (where both `.jj` and `.git` exist) so the status bar reflects
/// the VCS the user is actually driving, and falls back to the git
/// branch name in pure-git repos. Returns `None` when neither is
/// detectable.
pub(crate) fn read_vcs_info(cwd: &Path) -> Option<String> {
    read_jj_change(cwd).or_else(|| read_git_branch(cwd))
}

fn find_git_head_path(cwd: &Path) -> Option<PathBuf> {
    let mut current = cwd.to_path_buf();
    loop {
        let dot_git = current.join(".git");
        if let Some(git_head) = resolve_git_head_path(&dot_git) {
            return Some(git_head);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn resolve_git_head_path(dot_git: &Path) -> Option<PathBuf> {
    if dot_git.is_dir() {
        let head = dot_git.join("HEAD");
        return head.is_file().then_some(head);
    }

    if dot_git.is_file() {
        let dot_git_contents = std::fs::read_to_string(dot_git).ok()?;
        let gitdir = dot_git_contents
            .trim()
            .strip_prefix("gitdir:")
            .map(str::trim)?;
        if gitdir.is_empty() {
            return None;
        }
        let resolved_gitdir = Path::new(gitdir);
        let resolved_gitdir = if resolved_gitdir.is_absolute() {
            resolved_gitdir.to_path_buf()
        } else {
            dot_git.parent()?.join(resolved_gitdir)
        };
        let head = resolved_gitdir.join("HEAD");
        return head.is_file().then_some(head);
    }

    None
}

