use super::*;

use crate::models::{
    ExtensionProviderBinding, ModelEntry, extension_provider_bindings, normalize_api_key_opt,
};
use crate::provider_metadata::{
    ProviderMetadata, ProviderOnboardingMode, provider_ids_match, provider_metadata,
};

#[cfg(feature = "clipboard")]
use arboard::Clipboard as ArboardClipboard;

const BASH_COMPLETION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExcludedBashPersistenceOutcome {
    Saved,
    Disabled,
    NotConfirmed {
        pending_mutations: Option<usize>,
        failed_flushes: Option<u64>,
    },
}

impl ExcludedBashPersistenceOutcome {
    fn warning_text(self) -> Option<String> {
        let Self::NotConfirmed {
            pending_mutations,
            failed_flushes,
        } = self
        else {
            return None;
        };

        let pending =
            pending_mutations.map_or_else(|| "unavailable".to_string(), |count| count.to_string());
        let failed =
            failed_flushes.map_or_else(|| "unavailable".to_string(), |count| count.to_string());
        Some(format!(
            "[Persistence warning]\n\
- Execution ended and may have performed side effects; do not rerun it to repair this save problem.\n\
- Session record: not confirmed saved.\n\
- Pending mutation slots (bounded/coalescing): {pending}\n\
- Total failed save attempts: {failed}"
        ))
    }
}

async fn persist_excluded_bash_execution(
    session: Arc<Mutex<Session>>,
    message: SessionMessage,
    save_enabled: bool,
    cx: &Cx,
) -> ExcludedBashPersistenceOutcome {
    let mut session_guard = match OwnedMutexGuard::lock(session, cx).await {
        Ok(guard) => guard,
        Err(err) => {
            tracing::error!(
                error = %err,
                "completed excluded-context bash command could not lock its session for recording"
            );
            return ExcludedBashPersistenceOutcome::NotConfirmed {
                pending_mutations: None,
                failed_flushes: None,
            };
        }
    };

    session_guard.append_message(message);
    if !save_enabled {
        return ExcludedBashPersistenceOutcome::Disabled;
    }

    if let Err(err) = session_guard.save().await {
        let metrics = session_guard.autosave_metrics();
        tracing::error!(
            error = %err,
            pending_mutations = metrics.pending_mutations,
            failed_flushes = metrics.flush_failed,
            "completed excluded-context bash command was retained in memory but its session save was not confirmed"
        );
        return ExcludedBashPersistenceOutcome::NotConfirmed {
            pending_mutations: Some(metrics.pending_mutations),
            failed_flushes: Some(metrics.flush_failed),
        };
    }

    ExcludedBashPersistenceOutcome::Saved
}

async fn persist_excluded_bash_execution_bounded(
    session: Arc<Mutex<Session>>,
    message: SessionMessage,
    save_enabled: bool,
    cx: &Cx,
) -> ExcludedBashPersistenceOutcome {
    asupersync::time::timeout(
        asupersync::time::wall_now(),
        BASH_COMPLETION_TIMEOUT,
        persist_excluded_bash_execution(session, message, save_enabled, cx),
    )
    .await
    .unwrap_or_else(|_| {
        tracing::error!(
            "completed excluded-context bash command exceeded its persistence cleanup budget"
        );
        ExcludedBashPersistenceOutcome::NotConfirmed {
            pending_mutations: None,
            failed_flushes: None,
        }
    })
}

async fn deliver_bash_result(
    event_tx: &asupersync::channel::mpsc::Sender<RaMsg>,
    cx: &Cx,
    message: RaMsg,
) {
    if !crate::interactive::enqueue_pi_event(event_tx, cx, message).await {
        tracing::error!("terminal bash result was not delivered before runtime shutdown");
    }
}

fn spawn_bash_completion(
    runtime_handle: &asupersync::runtime::RuntimeHandle,
    event_tx: asupersync::channel::mpsc::Sender<RaMsg>,
    persistence: Option<(Arc<Mutex<Session>>, SessionMessage, bool)>,
    mut display: String,
    content_for_agent: Option<Vec<ContentBlock>>,
) {
    if let Err(err) = runtime_handle.try_spawn_with_cx(move |completion_cx| async move {
        if let Some((session, message, save_enabled)) = persistence {
            let persistence = persist_excluded_bash_execution_bounded(
                session,
                message,
                save_enabled,
                &completion_cx,
            )
            .await;
            if let Some(warning) = persistence.warning_text() {
                display.push_str("\n\n");
                display.push_str(&warning);
            }
        }
        deliver_bash_result(
            &event_tx,
            &completion_cx,
            RaMsg::BashResult {
                display,
                content_for_agent,
            },
        )
        .await;
    }) {
        tracing::error!(
            error = %err,
            "terminal bash completion could not be admitted by the runtime"
        );
    }
}

/// Available slash commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCommand {
    Help,
    Login,
    Logout,
    Clear,
    Model,
    Thinking,
    ScopedModels,
    Exit,
    History,
    Export,
    Session,
    Settings,
    Theme,
    Resume,
    New,
    Copy,
    Name,
    Hotkeys,
    Changelog,
    Tree,
    Fork,
    Compact,
    Reload,
    Template,
    Share,
    Mcp,
    Plan,
    Advisor,
    Checkpoint,
    Rewind,
    Fresh,
    Retry,
    Undo,
    Redo,
    Usage,
    Approval,
    Handoff,
    Rules,
    Omfg,
    ModelUpdate,
    Commit,
    Review,
    AddDir,
    RemoveDir,
    Crash,
    Btw,
    Tan,
}

impl SlashCommand {
    /// The spelling this command is written as in the help text and the
    /// completion menu — the first alternative [`Self::parse`] accepts.
    ///
    /// Exhaustive on purpose. A new variant will not compile until it is named
    /// here, and the tests below then force it into `/help` and the completion
    /// menu. Four commands had reached users without either (`/checkpoint`,
    /// `/rewind`, `/fresh`, `/retry`) and three more without a completion
    /// (`/add-dir`, `/remove-dir`, `/crash`), because nothing connected the
    /// parser to the two lists that advertise it.
    ///
    /// Adding a variant means adding it here AND to [`Self::ALL`].
    #[must_use]
    pub const fn canonical(self) -> &'static str {
        match self {
            Self::Help => "/help",
            Self::Login => "/login",
            Self::Logout => "/logout",
            Self::Clear => "/clear",
            Self::Model => "/model",
            Self::Thinking => "/thinking",
            Self::ScopedModels => "/scoped-models",
            Self::Exit => "/exit",
            Self::History => "/history",
            Self::Export => "/export",
            Self::Session => "/session",
            Self::Settings => "/settings",
            Self::Theme => "/theme",
            Self::Resume => "/resume",
            Self::New => "/new",
            Self::Copy => "/copy",
            Self::Name => "/name",
            Self::Hotkeys => "/hotkeys",
            Self::Changelog => "/changelog",
            Self::Tree => "/tree",
            Self::Fork => "/fork",
            Self::Compact => "/compact",
            Self::Reload => "/reload",
            Self::Template => "/template",
            Self::Share => "/share",
            Self::Mcp => "/mcp",
            Self::Plan => "/plan",
            Self::Advisor => "/advisor",
            Self::Checkpoint => "/checkpoint",
            Self::Rewind => "/rewind",
            Self::Fresh => "/fresh",
            Self::Retry => "/retry",
            Self::Undo => "/undo",
            Self::Redo => "/redo",
            Self::Usage => "/usage",
            Self::Approval => "/approval",
            Self::Handoff => "/handoff",
            Self::Review => "/review",
            Self::Rules => "/rules",
            Self::AddDir => "/add-dir",
            Self::RemoveDir => "/remove-dir",
            Self::Btw => "/btw",
            Self::Tan => "/tan",
            Self::Crash => "/crash",
            Self::Omfg => "/omfg",
            Self::ModelUpdate => "/model-update",
            Self::Commit => "/commit",
        }
    }

    /// Every slash command, so the help text and the completion menu can be
    /// checked against the parser rather than against each other.
    ///
    /// Hand-kept, unavoidably: Rust cannot enumerate a plain enum. The
    /// mitigation is that [`Self::canonical`] is exhaustive, so a new variant
    /// stops the build at a doc comment that says to add it here too; and
    /// `every_listed_command_parses_back_to_itself` catches a wrong entry.
    /// A forgotten entry is the one mistake that still slips.
    pub const ALL: &'static [Self] = &[
        Self::Help,
        Self::Login,
        Self::Logout,
        Self::Clear,
        Self::Model,
        Self::Thinking,
        Self::ScopedModels,
        Self::Exit,
        Self::History,
        Self::Export,
        Self::Session,
        Self::Settings,
        Self::Theme,
        Self::Resume,
        Self::New,
        Self::Copy,
        Self::Name,
        Self::Hotkeys,
        Self::Changelog,
        Self::Tree,
        Self::Fork,
        Self::Compact,
        Self::Reload,
        Self::Template,
        Self::Share,
        Self::Mcp,
        Self::Plan,
        Self::Advisor,
        Self::Checkpoint,
        Self::Rewind,
        Self::Fresh,
        Self::Retry,
        Self::Undo,
        Self::Redo,
        Self::Usage,
        Self::Approval,
        Self::Handoff,
        Self::Review,
        Self::Rules,
        Self::AddDir,
        Self::RemoveDir,
        Self::Btw,
        Self::Tan,
        Self::Crash,
        Self::Omfg,
        Self::ModelUpdate,
        Self::Commit,
    ];

    /// Parse a slash command from input.
    pub fn parse(input: &str) -> Option<(Self, &str)> {
        let input = input.trim();
        if !input.starts_with('/') {
            return None;
        }

        let (cmd, args) = input.split_once(char::is_whitespace).unwrap_or((input, ""));

        let command = match cmd.to_lowercase().as_str() {
            "/help" | "/h" | "/?" => Self::Help,
            "/login" => Self::Login,
            "/logout" => Self::Logout,
            "/clear" | "/cls" => Self::Clear,
            "/model" | "/m" => Self::Model,
            "/thinking" | "/think" | "/t" => Self::Thinking,
            "/scoped-models" | "/scoped" => Self::ScopedModels,
            "/exit" | "/quit" | "/q" => Self::Exit,
            "/history" | "/hist" => Self::History,
            "/export" => Self::Export,
            "/session" | "/info" => Self::Session,
            "/settings" => Self::Settings,
            "/theme" => Self::Theme,
            "/resume" | "/r" => Self::Resume,
            "/new" => Self::New,
            "/copy" | "/cp" => Self::Copy,
            "/name" => Self::Name,
            "/hotkeys" | "/keys" | "/keybindings" => Self::Hotkeys,
            "/changelog" => Self::Changelog,
            "/tree" => Self::Tree,
            "/fork" => Self::Fork,
            "/compact" => Self::Compact,
            "/reload" => Self::Reload,
            "/template" => Self::Template,
            "/share" => Self::Share,
            "/mcp" => Self::Mcp,
            "/plan" => Self::Plan,
            "/advisor" => Self::Advisor,
            "/checkpoint" | "/cp2" => Self::Checkpoint,
            "/rewind" => Self::Rewind,
            "/fresh" => Self::Fresh,
            "/retry" => Self::Retry,
            "/undo" => Self::Undo,
            "/redo" => Self::Redo,
            "/usage" => Self::Usage,
            "/approval" => Self::Approval,
            "/handoff" => Self::Handoff,
            "/review" => Self::Review,
            "/rules" => Self::Rules,
            "/add-dir" => Self::AddDir,
            "/remove-dir" => Self::RemoveDir,
            "/btw" => Self::Btw,
            "/tan" => Self::Tan,
            "/crash" => Self::Crash,
            "/omfg" => Self::Omfg,
            "/model-update" | "/update-models" => Self::ModelUpdate,
            "/commit" => Self::Commit,
            _ => return None,
        };

        Some((command, args.trim()))
    }

    /// Get help text for all commands.
    pub const fn help_text() -> &'static str {
        r"Available commands:
  /help, /h, /?      - Show this help message
  /login [provider]  - Login/setup credentials; without provider shows status table
  /logout [provider] - Remove stored credentials
  /clear, /cls       - Clear conversation history
  /model, /m [id|provider/id] - Open model selector or switch directly
  /thinking, /t [level] - Set thinking level (off/minimal/low/medium/high/xhigh/max)
  /scoped-models [patterns|clear] - Show or set scoped models for cycling
  /history, /hist    - Show input history
  /export [path]     - Export conversation to HTML
  /session, /info    - Show session info (path, tokens, cost)
  /settings          - Open settings selector
  /theme [name]      - List or switch themes (dark/light/auto/custom)
  /resume, /r        - Pick and resume a previous session
  /new               - Start a new session
  /copy, /cp         - Copy last assistant message to clipboard
  /name <name>       - Set session display name
  /hotkeys, /keys    - Show keyboard shortcuts
  /changelog         - Show changelog entries
  /tree              - Show session branch tree summary
  /fork [id|index]   - Fork from a user message (default: last on current path)
  /compact [shake|aggressive] [notes] - Compact older context (shake: instant no-LLM tool-result dropping)
  /reload            - Reload skills/prompts from disk
  /template <name> [args] - Expand a prompt template by name
  /share             - Upload current branch to an unlisted gist (not private; inspect sensitive context)
  /mcp               - Manage MCP servers: list, add, remove, test, trust (Model Context Protocol)
  /plan [approve|reject|off|status] - Enter plan mode / review a submitted plan
  /approval [always-ask|write|yolo|status] - Set or show tool approval mode
  /handoff [to] [path] - Generate structured cross-session/cross-agent handoff brief
  /rules [list|remove|toggle] - Manage time-traveling stream rules (TTSR)
  /add-dir <dir>     - Grant access to an additional workspace root
  /remove-dir <dir>  - Revoke an additional workspace root immediately
  /crash [show|delete] - Inspect or clear redacted crash bundles
  /btw <question>    - Ephemeral side question on the smol role (never persisted)
  /tan <work>        - Run tangential work in a background task-role child
  /omfg <complaint>  - Record user grievance and draft a candidate stream rule
  /model-update [provider] - Refresh live model catalogs for configured providers
  /commit [dry-run|all|bead] - Create dependency-ordered atomic commits from changes
  /review [target]   - Run prioritized code review on changes with ship verdict card
  /advisor [status|pause|resume] - Manage the turn-review advisor model
  /checkpoint, /cp2 [name] [note] - Mark a restore point on the current branch
  /rewind [name]     - Collapse everything since a checkpoint into a summary
  /fresh             - Reset provider stream state; the transcript is untouched
  /retry             - Re-send the last user turn as a sibling branch
  /undo [n] [force]  - Roll back the last n agent file edits (force: skip external-change guard)
  /redo [n] [force]  - Re-apply previously undone file edits
  /usage [refresh]   - Show provider usage/quota state
  /exit, /quit, /q   - Exit Pi

  Tips:
    • Use ↑/↓ arrows to navigate input history
    • Use Ctrl+L to open model selector
    • Use Ctrl+P to cycle scoped models
    • Use Shift+Enter (Ctrl+Enter on Windows) to insert a newline
    • Use PageUp/PageDown to scroll conversation history
    • Use Escape to cancel current input
    • Use /skill:name or /template to expand resources"
    }
}

pub(super) fn normalize_api_key_input(raw: &str) -> std::result::Result<String, String> {
    let key = raw.trim();
    if key.is_empty() {
        return Err("API key cannot be empty".to_string());
    }
    if key.chars().any(char::is_whitespace) {
        return Err("API key must not contain whitespace".to_string());
    }
    Ok(key.to_string())
}

pub(super) fn normalize_auth_provider_input(raw: &str) -> String {
    let provider = raw.trim().to_ascii_lowercase();
    crate::provider_metadata::canonical_provider_id(&provider)
        .unwrap_or(provider.as_str())
        .to_string()
}

fn provider_has_dedicated_login_flow(provider: &str) -> bool {
    BUILTIN_LOGIN_PROVIDERS
        .iter()
        .any(|(builtin, _)| provider_ids_match(builtin, provider))
}

/// Choose the GitHub Copilot device flow over the browser flow when the
/// current process cannot rely on a localhost OAuth redirect — i.e. the
/// session is running headless / over SSH and the user's browser cannot reach
/// the callback server bound on this host. `RECUR_AGENT_COPILOT_FORCE_DEVICE_FLOW=1`
/// opts in unconditionally.
///
/// When `GITHUB_COPILOT_CLIENT_ID` is unset we fall back to the well-known
/// public Copilot client id (`crate::auth::DEFAULT_COPILOT_CLIENT_ID`), so both
/// flows now succeed out of the box (#97). We still prefer the device flow when
/// no client id is explicitly configured, since that path is the most robust on
/// headless/SSH sessions where a localhost OAuth redirect can't be reached.
pub(super) fn should_use_copilot_device_flow() -> bool {
    if std::env::var("RECUR_AGENT_COPILOT_FORCE_DEVICE_FLOW")
        .is_ok_and(|v| matches!(v.as_str(), "1" | "true" | "yes"))
    {
        return true;
    }
    if std::env::var("GITHUB_COPILOT_CLIENT_ID").map_or(true, |v| v.trim().is_empty()) {
        return true;
    }
    std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some()
}

fn provider_supports_interactive_api_key_login(metadata: &ProviderMetadata) -> bool {
    if metadata.auth_env_keys.is_empty() || provider_has_dedicated_login_flow(metadata.canonical_id)
    {
        return false;
    }

    match metadata.onboarding {
        ProviderOnboardingMode::OpenAICompatiblePreset => metadata.routing_defaults.is_some(),
        ProviderOnboardingMode::BuiltInNative => metadata
            .routing_defaults
            .is_some_and(|defaults| !defaults.base_url.is_empty()),
        ProviderOnboardingMode::NativeAdapterRequired => false,
    }
}

fn generic_api_key_login_prompt(metadata: &ProviderMetadata) -> String {
    let provider = metadata.canonical_id;
    let label = metadata.display_name.unwrap_or(provider);
    let mut prompt = format!(
        "API key login: {provider}\n\n\
Paste your {label} API key to save it in auth.json under {provider}.\n"
    );

    if let Some(defaults) = metadata.routing_defaults
        && !defaults.base_url.is_empty()
    {
        let _ = writeln!(prompt, "Default base URL: {}", defaults.base_url);
    }

    if !metadata.auth_env_keys.is_empty() {
        let _ = writeln!(
            prompt,
            "Accepted env vars: {}",
            metadata.auth_env_keys.join(", ")
        );
    }

    prompt
        .push_str("\nYour input will be treated as sensitive and is not added to message history.");
    prompt
}

pub(super) fn api_key_login_prompt(provider: &str) -> Option<String> {
    match provider {
        "openai" => Some(String::from(
            "API key login: openai\n\n\
Paste your OpenAI API key to save it in auth.json.\n\
Get a key from platform.openai.com/api-keys.\n\
Rotate/revoke keys from that dashboard if compromised.\n\n\
Your input will be treated as sensitive and is not added to message history.",
        )),
        "google" => Some(String::from(
            "API key login: google/gemini\n\n\
Paste your Google Gemini API key to save it in auth.json under google.\n\
Get a key from ai.google.dev/gemini-api/docs/api-key.\n\
Rotate/revoke keys from Google AI Studio if compromised.\n\n\
Your input will be treated as sensitive and is not added to message history.",
        )),
        _ => provider_metadata(provider)
            .filter(|metadata| provider_supports_interactive_api_key_login(metadata))
            .map(generic_api_key_login_prompt),
    }
}

pub(super) fn save_provider_credential(
    auth: &mut crate::auth::AuthStorage,
    provider: &str,
    credential: crate::auth::AuthCredential,
) {
    let requested = provider.trim().to_ascii_lowercase();
    let canonical = normalize_auth_provider_input(&requested);
    let _ = auth.remove_provider_aliases(&requested);
    if requested != canonical {
        let _ = auth.remove_provider_aliases(&canonical);
    }
    auth.set(canonical.clone(), credential);
}

pub(super) fn remove_provider_credentials(
    auth: &mut crate::auth::AuthStorage,
    requested_provider: &str,
) -> bool {
    let requested = requested_provider.trim().to_ascii_lowercase();
    let canonical = normalize_auth_provider_input(&requested);

    let mut removed = auth.remove_provider_aliases(&canonical);
    if requested != canonical {
        removed |= auth.remove_provider_aliases(&requested);
    }
    removed
}

const BUILTIN_LOGIN_PROVIDERS: [(&str, &str); 7] = [
    ("anthropic", "OAuth"),
    ("openai-codex", "OAuth"),
    ("google-gemini-cli", "OAuth"),
    ("google-antigravity", "OAuth"),
    ("kimi-for-coding", "OAuth"),
    ("github-copilot", "OAuth"),
    ("gitlab", "OAuth"),
];

const STARTUP_PRIORITY_OAUTH_PROVIDERS: [(&str, &str); 3] = [
    ("anthropic", "Claude Code"),
    ("openai-codex", "Codex"),
    ("google-gemini-cli", "Gemini CLI"),
];

fn format_compact_duration(ms: i64) -> String {
    let seconds = (ms.max(0) / 1000).max(1);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 60 * 60 {
        format!("{}m", seconds / 60)
    } else if seconds < 24 * 60 * 60 {
        format!("{}h", seconds / (60 * 60))
    } else {
        format!("{}d", seconds / (24 * 60 * 60))
    }
}

fn format_credential_status(status: &crate::auth::CredentialStatus) -> String {
    match status {
        crate::auth::CredentialStatus::Missing => "Not authenticated".to_string(),
        crate::auth::CredentialStatus::ApiKey
        | crate::auth::CredentialStatus::BearerToken
        | crate::auth::CredentialStatus::AwsCredentials
        | crate::auth::CredentialStatus::ServiceKey => "Authenticated".to_string(),
        crate::auth::CredentialStatus::OAuthValid { expires_in_ms } => {
            format!(
                "Authenticated (expires in {})",
                format_compact_duration(*expires_in_ms)
            )
        }
        crate::auth::CredentialStatus::OAuthExpired { expired_by_ms } => {
            format!(
                "Authenticated (expired {} ago)",
                format_compact_duration(*expired_by_ms)
            )
        }
    }
}

fn format_provider_status(auth: &crate::auth::AuthStorage, provider: &str) -> String {
    if let Some(source) = auth.external_setup_source(provider)
        && !auth.has_stored_credential(provider)
    {
        return format!("Auto-detected from {source}");
    }

    let status = auth.credential_status(provider);
    format_credential_status(&status)
}

fn collect_extension_oauth_providers(
    available_models: &[ModelEntry],
    registered_extension_bindings: &[ExtensionProviderBinding],
) -> Vec<String> {
    let mut providers = registered_extension_bindings
        .iter()
        .filter(|binding| binding.oauth_config.is_some())
        .map(|binding| {
            let provider = binding.provider.as_str();
            crate::provider_metadata::canonical_provider_id(provider)
                .unwrap_or(provider)
                .to_string()
        })
        .collect::<Vec<_>>();
    providers.extend(
        available_models
            .iter()
            .filter(|entry| entry.oauth_config.is_some())
            .map(|entry| {
                let provider = entry.model.provider.as_str();
                crate::provider_metadata::canonical_provider_id(provider)
                    .unwrap_or(provider)
                    .to_string()
            }),
    );

    providers.retain(|provider| {
        !BUILTIN_LOGIN_PROVIDERS
            .iter()
            .any(|(builtin, _)| provider == builtin)
    });
    providers.sort_unstable();
    providers.dedup();
    providers
}

pub(super) fn extension_oauth_config_for_provider(
    available_models: &[ModelEntry],
    registered_extension_bindings: &[ExtensionProviderBinding],
    provider: &str,
) -> Option<crate::models::OAuthConfig> {
    registered_extension_bindings
        .iter()
        .find_map(|binding| {
            let registered_provider = binding.provider.as_str();
            let canonical = crate::provider_metadata::canonical_provider_id(registered_provider)
                .unwrap_or(registered_provider);
            if canonical.eq_ignore_ascii_case(provider) {
                binding.oauth_config.clone()
            } else {
                None
            }
        })
        .or_else(|| {
            available_models.iter().find_map(|entry| {
                let model_provider = entry.model.provider.as_str();
                let canonical = crate::provider_metadata::canonical_provider_id(model_provider)
                    .unwrap_or(model_provider);
                if canonical.eq_ignore_ascii_case(provider) {
                    entry.oauth_config.clone()
                } else {
                    None
                }
            })
        })
}

pub(super) fn registered_extension_provider_bindings(
    extensions: Option<&ExtensionManager>,
) -> crate::error::Result<Vec<ExtensionProviderBinding>> {
    extensions.map_or_else(
        || Ok(Vec::new()),
        |manager| extension_provider_bindings(&manager.extension_providers()),
    )
}

fn append_provider_rows(output: &mut String, heading: &str, rows: &[(String, String, String)]) {
    let provider_width = rows
        .iter()
        .map(|(provider, _, _)| provider.len())
        .max()
        .unwrap_or("provider".len())
        .max("provider".len());
    let method_width = rows
        .iter()
        .map(|(_, method, _)| method.len())
        .max()
        .unwrap_or("method".len())
        .max("method".len());

    let _ = writeln!(output, "{heading}:");
    let _ = writeln!(
        output,
        "  {:<provider_width$}  {:<method_width$}  status",
        "provider", "method"
    );
    for (provider, method, status) in rows {
        let _ = writeln!(
            output,
            "  {provider:<provider_width$}  {method:<method_width$}  {status}"
        );
    }
}

pub(super) fn format_login_provider_listing(
    auth: &crate::auth::AuthStorage,
    available_models: &[ModelEntry],
    registered_extension_bindings: &[ExtensionProviderBinding],
) -> String {
    let mut output = String::from("Available login providers:\n\n");

    let mut built_in_rows: Vec<(String, String, String)> = BUILTIN_LOGIN_PROVIDERS
        .iter()
        .map(|(provider, method)| {
            (
                (*provider).to_string(),
                (*method).to_string(),
                format_provider_status(auth, provider),
            )
        })
        .collect();
    let mut api_key_rows: Vec<(String, String, String)> =
        crate::provider_metadata::PROVIDER_METADATA
            .iter()
            .filter(|meta| provider_supports_interactive_api_key_login(meta))
            .map(|meta| {
                let provider = meta.canonical_id.to_string();
                (
                    provider.clone(),
                    "API key".to_string(),
                    format_provider_status(auth, &provider),
                )
            })
            .collect();
    api_key_rows.sort_by(|left, right| left.0.cmp(&right.0));
    built_in_rows.extend(api_key_rows);
    append_provider_rows(&mut output, "Built-in", &built_in_rows);

    let extension_providers =
        collect_extension_oauth_providers(available_models, registered_extension_bindings);
    if !extension_providers.is_empty() {
        let extension_rows: Vec<(String, String, String)> = extension_providers
            .iter()
            .map(|provider| {
                (
                    provider.clone(),
                    "OAuth".to_string(),
                    format_provider_status(auth, provider),
                )
            })
            .collect();
        output.push('\n');
        append_provider_rows(&mut output, "Extension providers", &extension_rows);
    }

    output.push_str("\nUsage: /login <provider>");
    output
}

pub fn strip_thinking_level_suffix(pattern: &str) -> &str {
    let Some((prefix, suffix)) = pattern.rsplit_once(':') else {
        return pattern;
    };
    match suffix.to_ascii_lowercase().as_str() {
        "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" => prefix,
        _ => pattern,
    }
}

/// Returned by [`copy_text_to_clipboard`] when the text actually reached the
/// system clipboard (as opposed to the temp-file fallback). Callers that only
/// need "did it work" compare against this instead of a duplicated literal.
pub const COPY_OK_MESSAGE: &str = "Copied to clipboard";

/// Put `text` on the system clipboard, falling back to a private temp file,
/// and return the sentence describing what happened.
///
/// Free rather than a `RaApp` method because the ftui stack runs the same
/// `/copy` (bd-cv653): the feature gating, the 0600 fallback file and the
/// exact wording of each outcome must not exist twice. The wording is the
/// charmed stack's, unchanged, so the two stacks report a copy identically.
pub fn copy_text_to_clipboard(text: &str) -> String {
    fn write_fallback(text: &str) -> std::io::Result<std::path::PathBuf> {
        use std::io::Write;
        let dir = std::env::temp_dir();
        let filename = format!("pi_copy_{}.txt", Utc::now().timestamp_millis());
        // ubs:ignore filename is a literal plus a timestamp, never external input
        let path = dir.join(filename);

        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }

        let mut file = options.open(&path)?;
        file.write_all(text.as_bytes())?;

        Ok(path)
    }

    // GH #242: under WSL without WSLg there is no display for arboard, but
    // WSL puts Windows' clip.exe on PATH.
    if running_under_wsl() && copy_via_clip_exe(text).is_ok() {
        return String::from(COPY_OK_MESSAGE);
    }

    #[cfg(feature = "clipboard")]
    {
        match ArboardClipboard::new().and_then(|mut clipboard| clipboard.set_text(text.to_string()))
        {
            Ok(()) => String::from(COPY_OK_MESSAGE),
            Err(err) => match write_fallback(text) {
                Ok(path) => format!(
                    "Clipboard support is disabled or unavailable ({err}). Wrote to {}",
                    path.display()
                ),
                Err(io_err) => format!(
                    "Clipboard support is disabled or unavailable ({err}); also failed to write fallback file: {io_err}"
                ),
            },
        }
    }

    #[cfg(not(feature = "clipboard"))]
    {
        match write_fallback(text) {
            Ok(path) => format!("Clipboard support is disabled. Wrote to {}", path.display()),
            Err(err) => {
                format!("Clipboard support is disabled; failed to write fallback file: {err}")
            }
        }
    }
}

/// Whether this process runs inside WSL (GH #242).
pub fn running_under_wsl() -> bool {
    wsl_detected(
        std::env::var_os("WSL_DISTRO_NAME").is_some() || std::env::var_os("WSL_INTEROP").is_some(),
        std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .ok()
            .as_deref(),
    )
}

fn wsl_detected(wsl_env: bool, kernel_release: Option<&str>) -> bool {
    wsl_env
        || kernel_release.is_some_and(|release| release.to_ascii_lowercase().contains("microsoft"))
}

/// What `clip.exe` is fed: UTF-16LE with a byte-order mark. Plain UTF-8 is
/// read in the console code page and garbles anything non-ASCII.
fn clip_exe_payload(text: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// Put `text` on the Windows clipboard from inside WSL.
pub fn copy_via_clip_exe(text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("clip.exe")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&clip_exe_payload(text))?;
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "clip.exe exited with {status}"
        )))
    }
}

pub fn parse_scoped_model_patterns(args: &str) -> Vec<String> {
    args.split(|c: char| c == ',' || c.is_whitespace())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

pub fn model_entry_matches(left: &ModelEntry, right: &ModelEntry) -> bool {
    let left_provider = crate::provider_metadata::canonical_provider_id(&left.model.provider)
        .unwrap_or(&left.model.provider);
    let right_provider = crate::provider_metadata::canonical_provider_id(&right.model.provider)
        .unwrap_or(&right.model.provider);

    left_provider.eq_ignore_ascii_case(right_provider)
        && left.model.id.eq_ignore_ascii_case(&right.model.id)
}

pub(super) fn resolve_model_key_with_auth(
    auth: &crate::auth::AuthStorage,
    entry: &ModelEntry,
) -> Option<String> {
    normalize_api_key_opt(auth.resolve_api_key(&entry.model.provider, None))
        .or_else(|| normalize_api_key_opt(entry.api_key.clone()))
}

pub(super) fn resolve_model_key_from_default_auth(entry: &ModelEntry) -> Option<String> {
    let auth_path = crate::config::Config::auth_path();
    crate::auth::AuthStorage::load(auth_path)
        .ok()
        .and_then(|auth| resolve_model_key_with_auth(&auth, entry))
        .or_else(|| normalize_api_key_opt(entry.api_key.clone()))
}

fn session_thinking_level(
    session: &crate::session::Session,
) -> Option<crate::model::ThinkingLevel> {
    session
        .effective_thinking_level_for_current_path()
        .as_deref()
        .and_then(|value| value.parse::<crate::model::ThinkingLevel>().ok())
}

fn model_entry_event_payload(entry: &ModelEntry) -> Value {
    json!({
        "id": entry.model.id.clone(),
        "name": entry.model.name.clone(),
        "provider": entry.model.provider.clone(),
        "api": entry.model.api.clone(),
        "baseUrl": entry.model.base_url.clone(),
        "contextWindow": entry.model.context_window,
        "maxTokens": entry.model.max_tokens,
        "input": &entry.model.input,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SessionThinkingSyncPlan {
    effective: crate::model::ThinkingLevel,
    thinking_changed: bool,
    persist_needed: bool,
}

fn plan_session_thinking_sync(
    session_thinking: Option<&str>,
    current_thinking: crate::model::ThinkingLevel,
    target_entry: &ModelEntry,
) -> SessionThinkingSyncPlan {
    let parsed_session_thinking = session_thinking.and_then(|raw| {
        raw.parse::<crate::model::ThinkingLevel>().map_or_else(
            |_| {
                tracing::warn!("Ignoring invalid session thinking level: {raw}");
                None
            },
            Some,
        )
    });
    let requested_thinking = parsed_session_thinking.unwrap_or(current_thinking);
    let effective = target_entry.clamp_thinking_level(requested_thinking);
    let thinking_changed = effective != current_thinking;
    let persist_needed = if session_thinking.is_some() {
        parsed_session_thinking != Some(effective)
    } else {
        thinking_changed
    };

    SessionThinkingSyncPlan {
        effective,
        thinking_changed,
        persist_needed,
    }
}

fn parse_user_bash_event_result(value: &Value) -> Option<crate::tools::BashRunResult> {
    let result = value
        .as_object()
        .map_or(value, |obj| obj.get("result").unwrap_or(value));
    let obj = result.as_object()?;

    let output = obj
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let exit_code = obj
        .get("exitCode")
        .and_then(Value::as_i64)
        .or_else(|| obj.get("exit_code").and_then(Value::as_i64))
        .unwrap_or(0);
    let cancelled = obj
        .get("cancelled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let truncated = obj
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let full_output_path = obj
        .get("fullOutputPath")
        .or_else(|| obj.get("full_output_path"))
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let cancellation = obj.get("cancellation").and_then(Value::as_object);
    let cancellation_reason = cancellation
        .and_then(|details| details.get("reason"))
        .and_then(Value::as_str)
        .and_then(|reason| match reason {
            "timeout" => Some(crate::tools::BashCancellationReason::Timeout),
            "ambient_cancellation" => {
                Some(crate::tools::BashCancellationReason::AmbientCancellation)
            }
            _ => None,
        });
    let timeout_ms = cancellation
        .and_then(|details| details.get("timeoutMs"))
        .or_else(|| obj.get("timeoutMs"))
        .and_then(Value::as_u64);

    Some(crate::tools::BashRunResult {
        output,
        exit_code: i32::try_from(exit_code).unwrap_or(0),
        cancelled,
        cancellation_reason,
        timeout_ms,
        truncated,
        full_output_path,
        truncation: None,
    })
}

pub fn resolve_scoped_model_entries(
    patterns: &[String],
    available_models: &[ModelEntry],
) -> Result<Vec<ModelEntry>, String> {
    let mut resolved: Vec<ModelEntry> = Vec::new();

    for pattern in patterns {
        let raw_pattern = strip_thinking_level_suffix(pattern);
        let is_glob =
            raw_pattern.contains('*') || raw_pattern.contains('?') || raw_pattern.contains('[');

        if is_glob {
            let glob = Pattern::new(&raw_pattern.to_lowercase())
                .map_err(|err| format!("Invalid model pattern \"{pattern}\": {err}"))?;

            for entry in available_models {
                let full_id = format!("{}/{}", entry.model.provider, entry.model.id);
                let full_id_lower = full_id.to_lowercase();
                let id_lower = entry.model.id.to_lowercase();

                if (glob.matches(&full_id_lower) || glob.matches(&id_lower))
                    && !resolved
                        .iter()
                        .any(|existing| model_entry_matches(existing, entry))
                {
                    resolved.push(entry.clone());
                }
            }
            continue;
        }

        for entry in available_models {
            let full_id = format!("{}/{}", entry.model.provider, entry.model.id);
            if raw_pattern.eq_ignore_ascii_case(&full_id)
                || raw_pattern.eq_ignore_ascii_case(&entry.model.id)
            {
                if !resolved
                    .iter()
                    .any(|existing| model_entry_matches(existing, entry))
                {
                    resolved.push(entry.clone());
                }
                break;
            }
        }
    }

    resolved.sort_by(|a, b| {
        let left = format!("{}/{}", a.model.provider, a.model.id);
        let right = format!("{}/{}", b.model.provider, b.model.id);
        left.cmp(&right)
    });

    Ok(resolved)
}

pub(super) const fn kind_rank(kind: &DiagnosticKind) -> u8 {
    match kind {
        DiagnosticKind::Warning => 0,
        DiagnosticKind::Collision => 1,
    }
}

pub(super) fn format_resource_diagnostics(
    label: &str,
    diagnostics: &[ResourceDiagnostic],
) -> (String, usize) {
    let mut ordered: Vec<&ResourceDiagnostic> = diagnostics.iter().collect();
    ordered.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| kind_rank(&a.kind).cmp(&kind_rank(&b.kind)))
            .then_with(|| a.message.cmp(&b.message))
    });

    let mut out = String::new();
    let _ = writeln!(out, "{label}:");
    for diag in ordered {
        let kind = match diag.kind {
            DiagnosticKind::Warning => "warning",
            DiagnosticKind::Collision => "collision",
        };
        let _ = write!(out, "- {kind}: {} ({})", diag.message, diag.path.display());
        if let Some(collision) = &diag.collision {
            let _ = write!(
                out,
                " [winner: {} loser: {}]",
                collision.winner_path.display(),
                collision.loser_path.display()
            );
        }
        out.push('\n');
    }
    (out, diagnostics.len())
}

fn build_reload_diagnostics(
    models_error: Option<String>,
    resources: &ResourceLoader,
) -> (Option<String>, usize) {
    let mut sections = Vec::new();
    let mut count = 0usize;

    if let Some(err) = models_error {
        count = count.saturating_add(1);
        sections.push(format!("models.json:\n{err}"));
    }

    let mut resource_sections = Vec::new();
    let (skills_text, skills_count) =
        format_resource_diagnostics("Skills", resources.skill_diagnostics());
    if skills_count > 0 {
        resource_sections.push(skills_text);
        count = count.saturating_add(skills_count);
    }

    let (prompts_text, prompts_count) =
        format_resource_diagnostics("Prompts", resources.prompt_diagnostics());
    if prompts_count > 0 {
        resource_sections.push(prompts_text);
        count = count.saturating_add(prompts_count);
    }

    let (themes_text, themes_count) =
        format_resource_diagnostics("Themes", resources.theme_diagnostics());
    if themes_count > 0 {
        resource_sections.push(themes_text);
        count = count.saturating_add(themes_count);
    }

    if !resource_sections.is_empty() {
        sections.push(format!(
            "Resource diagnostics:\n{}",
            resource_sections.join("\n")
        ));
    }

    if sections.is_empty() {
        (None, 0)
    } else {
        (
            Some(format!("Reload diagnostics:\n\n{}", sections.join("\n\n"))),
            count,
        )
    }
}

/// Expand a leading `~`/`~/` to `$HOME` for /add-dir and /remove-dir.
/// `Path::join` on an absolute component replaces the base, so the slash
/// must be stripped before joining.
fn expand_home_path(raw: &str) -> std::path::PathBuf {
    let Ok(home) = std::env::var("HOME") else {
        return std::path::PathBuf::from(raw);
    };
    if raw == "~" {
        return std::path::PathBuf::from(home);
    }
    raw.strip_prefix("~/").map_or_else(
        || std::path::PathBuf::from(raw),
        |rest| std::path::PathBuf::from(home).join(rest),
    )
}

