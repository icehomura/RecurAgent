//! Host-registered tools of a host-owned app peer (UPCR-2026-035).
//!
//! The host declares a peer's app tools and the generic kernel tools the app
//! may use with `peer/tools/register` (host-token authorized). The set is
//! durable (`peers/<slug>/host_tools.json`), versioned, and replaced whole.
//! Once a peer has a set, every turn of the peer's session and of each of its
//! request contexts offers the model EXACTLY that set: the registered app
//! tools plus the allowed generic tools that exist in the turn's registry.
//! Everything else is removed from the turn's registry, so it is neither
//! advertised nor callable (the registry refuses an unknown name).
//!
//! App tool calls go to the host: the kernel sends `peer/tool/call` to the
//! connection that registered the set and waits for `peer/tool/result`
//! (bounded by the set's call timeout; on timeout or turn interrupt it sends
//! `peer/tool/cancel`). Risk gating happens in
//! [`octos_agent::HostRoutedTool`] before the host is asked. Every call is
//! appended to `peers/<slug>/tool_audit.jsonl`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use octos_agent::{
    HostRoutedTool, HostToolAudit, HostToolCall, HostToolCallOutcome, HostToolConfirm,
    HostToolDecl, HostToolRisk, HostToolRouter, ToolRegistry,
};
use octos_core::SessionKey;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::app_binding::{PEER_CONTEXT_TOPIC_PREFIX, parse_context_topic, validate_context_id};
use super::{peer_io, peer_slug_is_safe, staged_peer_dir};

/// Peer-dir leaf holding the registered [`PeerHostToolSet`].
pub(crate) const HOST_TOOLS_LEAF: &str = "host_tools.json";
/// Peer-dir leaf of the per-call audit log.
pub(crate) const TOOL_AUDIT_LEAF: &str = "tool_audit.jsonl";

/// Server → host: run an app tool.
pub(crate) const PEER_TOOL_CALL_NOTIFICATION: &str =
    octos_core::ui_protocol::methods::PEER_TOOL_CALL;
/// Server → host: stop a call the kernel no longer waits for.
pub(crate) const PEER_TOOL_CANCEL_NOTIFICATION: &str =
    octos_core::ui_protocol::methods::PEER_TOOL_CANCEL;

pub(crate) const MAX_APP_TOOLS: usize = 64;
pub(crate) const MAX_GENERIC_TOOLS: usize = 32;
const MAX_DESCRIPTION_BYTES: usize = 2 * 1024;
const MAX_SCHEMA_BYTES: usize = 16 * 1024;
const MAX_MODEL_NAME_BYTES: usize = 64;
const DEFAULT_CALL_TIMEOUT_MS: u64 = 30_000;
const MAX_CALL_TIMEOUT_MS: u64 = 300_000;
const DEFAULT_APPROVAL_TTL_SECS: u64 = 3_600;
const MAX_APPROVAL_TTL_SECS: u64 = 7 * 24 * 3_600;
const DEFAULT_MAX_RESULT_BYTES: usize = 256 * 1024;
const MAX_RESULT_BYTES_CEILING: usize = 1024 * 1024;
/// In-flight host calls per peer.
pub(crate) const MAX_PENDING_CALLS_PER_PEER: usize = 16;
/// How long a claimed approval occurrence is remembered.
const OCCURRENCE_RETENTION: Duration = Duration::from_secs(24 * 3_600);

/// A peer's registered tool set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PeerHostToolSet {
    /// Starts at 1; every registration increments it.
    pub(crate) version: u64,
    pub(crate) tools: Vec<HostToolDecl>,
    /// Kernel tool names the app may use (e.g. `deep_search`).
    pub(crate) generic_tools: Vec<String>,
    pub(crate) call_timeout_ms: u64,
    pub(crate) approval_ttl_secs: u64,
    pub(crate) max_result_bytes: usize,
}

// ---------------------------------------------------------------------------
// Registration input
// ---------------------------------------------------------------------------

/// One tool as the host declares it: an entry of the app bundle's
/// `tools.json`, the one declaration source for native modules and script
/// apps alike.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolInput {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) description: String,
    pub(crate) input_schema: Value,
    #[serde(default)]
    pub(crate) output_schema: Option<Value>,
    pub(crate) risk: HostToolRisk,
    #[serde(default)]
    pub(crate) background: bool,
    #[serde(default)]
    pub(crate) outward: bool,
    #[serde(default)]
    pub(crate) confirm: HostToolConfirm,
    /// App Hub metadata the kernel does not act on (callers other than the
    /// app's own agent are a follow-up).
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) shareable: Option<bool>,
}

/// Registration options.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ToolSetOptions {
    #[serde(default)]
    pub(crate) call_timeout_ms: Option<u64>,
    #[serde(default)]
    pub(crate) approval_ttl_secs: Option<u64>,
    #[serde(default)]
    pub(crate) max_result_bytes: Option<usize>,
}

fn segment_is_valid(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 32
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
}

/// `<app>.<tool>[.<more>]`: 2–4 segments of `[a-z][a-z0-9_]{0,31}`.
pub(crate) fn validate_app_tool_name(name: &str) -> Result<(), String> {
    let segments: Vec<&str> = name.split('.').collect();
    if !(2..=4).contains(&segments.len()) || !segments.iter().all(|s| segment_is_valid(s)) {
        return Err(format!(
            "tool name '{name}' must be '<app>.<tool>': 2-4 '.'-separated segments of [a-z][a-z0-9_]{{0,31}}"
        ));
    }
    Ok(())
}

fn validate_generic_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= MAX_MODEL_NAME_BYTES
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "generic tool name '{name}' is not a kernel tool name"
        ))
    }
}

fn parse_schema(tool: &str, field: &str, schema: Value) -> Result<Value, String> {
    let size = serde_json::to_string(&schema).map(|s| s.len()).unwrap_or(0);
    if size > MAX_SCHEMA_BYTES {
        return Err(format!(
            "{tool}: {field} is {size} bytes (max {MAX_SCHEMA_BYTES})"
        ));
    }
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err(format!(
            "{tool}: {field} must be a JSON Schema object (\"type\": \"object\")"
        ));
    }
    Ok(schema)
}

/// Validate a registration and build the next set (`version` is filled by
/// the caller).
pub(crate) fn build_tool_set(
    flat: Vec<ToolInput>,
    generic_tools: Vec<String>,
    options: ToolSetOptions,
) -> Result<PeerHostToolSet, String> {
    if flat.len() > MAX_APP_TOOLS {
        return Err(format!("{} app tools (max {MAX_APP_TOOLS})", flat.len()));
    }
    if generic_tools.len() > MAX_GENERIC_TOOLS {
        return Err(format!(
            "{} generic tools (max {MAX_GENERIC_TOOLS})",
            generic_tools.len()
        ));
    }
    let mut generic: Vec<String> = Vec::new();
    for name in generic_tools {
        validate_generic_name(&name)?;
        if !generic.contains(&name) {
            generic.push(name);
        }
    }
    let mut seen_model_names: Vec<String> = generic.clone();
    let mut decls = Vec::with_capacity(flat.len());
    for tool in flat {
        validate_app_tool_name(&tool.name)?;
        let model_name = tool.name.replace('.', "_");
        if model_name.len() > MAX_MODEL_NAME_BYTES {
            return Err(format!("tool name '{}' is too long", tool.name));
        }
        if seen_model_names.contains(&model_name) {
            return Err(format!(
                "tool '{}' collides with another tool the model would see as '{model_name}'",
                tool.name
            ));
        }
        seen_model_names.push(model_name.clone());
        let description = tool.description.trim().to_owned();
        if description.is_empty() || description.len() > MAX_DESCRIPTION_BYTES {
            return Err(format!(
                "{}: description must be 1..={MAX_DESCRIPTION_BYTES} bytes",
                tool.name
            ));
        }
        let input_schema = parse_schema(&tool.name, "input_schema", tool.input_schema)?;
        let output_schema = match tool.output_schema {
            Some(Value::Null) | None => None,
            Some(raw) => Some(parse_schema(&tool.name, "output_schema", raw)?),
        };
        decls.push(HostToolDecl {
            name: tool.name,
            model_name,
            description,
            input_schema,
            output_schema,
            risk: tool.risk,
            background: tool.background,
            outward: tool.outward,
            confirm: tool.confirm,
        });
    }
    let call_timeout_ms = options
        .call_timeout_ms
        .unwrap_or(DEFAULT_CALL_TIMEOUT_MS)
        .clamp(1, MAX_CALL_TIMEOUT_MS);
    let approval_ttl_secs = options
        .approval_ttl_secs
        .unwrap_or(DEFAULT_APPROVAL_TTL_SECS)
        .clamp(1, MAX_APPROVAL_TTL_SECS);
    let max_result_bytes = options
        .max_result_bytes
        .unwrap_or(DEFAULT_MAX_RESULT_BYTES)
        .clamp(1, MAX_RESULT_BYTES_CEILING);
    Ok(PeerHostToolSet {
        version: 0,
        tools: decls,
        generic_tools: generic,
        call_timeout_ms,
        approval_ttl_secs,
        max_result_bytes,
    })
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

/// What the kernel knows about a peer's registered set.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum StoredToolSet {
    /// Never registered: the peer keeps the ordinary tool roster.
    None,
    Registered(PeerHostToolSet),
    /// The leaf exists but cannot be read: fail closed (no tools at all).
    Unreadable,
}

pub(crate) fn read_tool_set(peers_root: &Path, slug: &str) -> StoredToolSet {
    let Some(dir) = staged_peer_dir(peers_root, slug) else {
        return StoredToolSet::None;
    };
    if dir.join(HOST_TOOLS_LEAF).symlink_metadata().is_err() {
        return StoredToolSet::None;
    }
    match peer_io::read_peer_file(&dir, HOST_TOOLS_LEAF, peer_io::PEER_FILE_READ_CAP_LARGE)
        .and_then(|body| serde_json::from_str::<PeerHostToolSet>(&body).ok())
    {
        Some(set) => StoredToolSet::Registered(set),
        None => StoredToolSet::Unreadable,
    }
}

/// Durably replace the set. The caller serializes registrations per peer.
pub(crate) fn write_tool_set(
    peers_root: &Path,
    slug: &str,
    set: &PeerHostToolSet,
) -> Result<(), String> {
    let dir = staged_peer_dir(peers_root, slug)
        .ok_or_else(|| format!("peer '{slug}' is not a staged peer"))?;
    let body = serde_json::to_string(set).map_err(|err| err.to_string())?;
    if body.len() > peer_io::PEER_FILE_READ_CAP_LARGE {
        return Err(format!(
            "the tool set is {} bytes (max {})",
            body.len(),
            peer_io::PEER_FILE_READ_CAP_LARGE
        ));
    }
    peer_io::write_peer_file_durable(&dir, HOST_TOOLS_LEAF, &body)
        .map_err(|err| format!("failed to record the tool set: {err}"))
}

/// Serializes read-modify-write of a peer's set (version bump).
pub(crate) static REGISTRATION_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

// ---------------------------------------------------------------------------
// Per-session enforcement
// ---------------------------------------------------------------------------

/// The tool roster a session's turns must use.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SessionHostTools {
    /// Not a host peer with a registered set: unchanged roster.
    Unrestricted,
    Enforced {
        slug: String,
        context_id: Option<String>,
        set: PeerHostToolSet,
    },
    /// A registered set that cannot be read: offer no tools.
    FailClosed { slug: String },
}

/// Resolve the roster for `session` (a `peer-<slug>` of a host-bound peer or
/// a `peerctx-<slug>.<context>` request context).
pub(crate) fn resolve_session_host_tools(
    peers_root: &Path,
    session: &SessionKey,
) -> SessionHostTools {
    let Some(topic) = session.topic() else {
        return SessionHostTools::Unrestricted;
    };
    let (slug, context_id) = if topic.starts_with(PEER_CONTEXT_TOPIC_PREFIX) {
        match parse_context_topic(topic) {
            Some((slug, context)) if validate_context_id(context).is_ok() => {
                (slug, Some(context.to_owned()))
            }
            _ => return SessionHostTools::Unrestricted,
        }
    } else if let Some(slug) = topic.strip_prefix("peer-") {
        (slug, None)
    } else {
        return SessionHostTools::Unrestricted;
    };
    if !peer_slug_is_safe(slug) || !super::app_binding::peer_is_host_owned(peers_root, slug) {
        return SessionHostTools::Unrestricted;
    }
    match read_tool_set(peers_root, slug) {
        StoredToolSet::None => SessionHostTools::Unrestricted,
        StoredToolSet::Registered(set) => SessionHostTools::Enforced {
            slug: slug.to_owned(),
            context_id,
            set,
        },
        StoredToolSet::Unreadable => SessionHostTools::FailClosed {
            slug: slug.to_owned(),
        },
    }
}

/// Make `registry` offer exactly the resolved roster for one turn of
/// `session_id`. App tools route through a [`TurnHostToolRouter`].
pub(crate) fn apply_session_host_tools(
    registry: &mut ToolRegistry,
    resolved: &SessionHostTools,
    peers_root: &Path,
    session_id: &SessionKey,
    turn_id: &str,
) {
    match resolved {
        SessionHostTools::Unrestricted => {}
        SessionHostTools::FailClosed { .. } => registry.retain(|_| false),
        SessionHostTools::Enforced {
            slug,
            context_id,
            set,
        } => {
            registry.retain(|name| set.generic_tools.iter().any(|allowed| allowed == name));
            if set.tools.is_empty() {
                return;
            }
            let router: Arc<dyn HostToolRouter> = Arc::new(TurnHostToolRouter {
                peers_root: peers_root.to_path_buf(),
                slug: slug.clone(),
                context_id: context_id.clone(),
                session_id: session_id.clone(),
                turn_id: turn_id.to_owned(),
                version: set.version,
                call_timeout: Duration::from_millis(set.call_timeout_ms),
                max_result_bytes: set.max_result_bytes,
            });
            let ttl = Duration::from_secs(set.approval_ttl_secs);
            // An open request context is one of the app's interactive
            // clients; the peer's own session is not.
            let interactive = context_id.is_some();
            for decl in &set.tools {
                registry.register(HostRoutedTool::new(
                    decl.clone(),
                    router.clone(),
                    ttl,
                    interactive,
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Routing to the host
// ---------------------------------------------------------------------------

/// Sends one notification to the host connection; `false` when it is gone.
pub(crate) type HostSend = Arc<dyn Fn(&'static str, Value) -> bool + Send + Sync>;

struct PendingCall {
    route_key: String,
    max_result_bytes: usize,
    tx: tokio::sync::oneshot::Sender<HostToolCallOutcome>,
}

#[derive(Default)]
struct HostToolHub {
    routes: Mutex<HashMap<String, HostSend>>,
    pending: Mutex<HashMap<String, PendingCall>>,
    occurrences: Mutex<HashMap<String, Instant>>,
}

static HUB: LazyLock<HostToolHub> = LazyLock::new(HostToolHub::default);

/// Process-wide key of one peer's route.
pub(crate) fn route_key(peers_root: &Path, slug: &str) -> String {
    format!("{}\u{0}{slug}", peers_root.display())
}

/// Route the peer's app tool calls to `send` (the registering connection),
/// replacing any earlier route.
pub(crate) fn set_host_route(peers_root: &Path, slug: &str, send: HostSend) {
    HUB.routes
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(route_key(peers_root, slug), send);
}

/// Result of `peer/tool/result`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompleteCall {
    Accepted,
    /// Accepted, but the data exceeded the cap: the model gets an error.
    TooLarge {
        bytes: usize,
        max: usize,
    },
}

/// Complete a pending call of the peer at `peers_root`/`slug`.
pub(crate) fn complete_host_call(
    peers_root: &Path,
    slug: &str,
    call_id: &str,
    outcome: HostToolCallOutcome,
) -> Result<CompleteCall, String> {
    let key = route_key(peers_root, slug);
    let pending = {
        let mut pending = HUB.pending.lock().unwrap_or_else(|p| p.into_inner());
        match pending.get(call_id) {
            Some(call) if call.route_key == key => pending.remove(call_id),
            _ => None,
        }
    };
    let Some(call) = pending else {
        return Err(format!(
            "no pending call '{call_id}' for peer '{slug}' (finished, timed out or cancelled)"
        ));
    };
    let (outcome, status) = match outcome {
        HostToolCallOutcome::Ok(data) => {
            let bytes = serde_json::to_string(&data).map(|s| s.len()).unwrap_or(0);
            if bytes > call.max_result_bytes {
                (
                    HostToolCallOutcome::Error {
                        kind: "result_too_large".into(),
                        message: format!(
                            "the app returned {bytes} bytes (max {})",
                            call.max_result_bytes
                        ),
                    },
                    CompleteCall::TooLarge {
                        bytes,
                        max: call.max_result_bytes,
                    },
                )
            } else {
                (HostToolCallOutcome::Ok(data), CompleteCall::Accepted)
            }
        }
        error => (error, CompleteCall::Accepted),
    };
    let _ = call.tx.send(outcome);
    Ok(status)
}

/// Removes a pending call and tells the host to stop it unless disarmed.
struct PendingGuard {
    call_id: String,
    send: HostSend,
    armed: bool,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        let removed = HUB
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.call_id)
            .is_some();
        if self.armed && removed {
            let _ = (self.send)(
                PEER_TOOL_CANCEL_NOTIFICATION,
                json!({ "call_id": self.call_id, "reason": "cancelled" }),
            );
        }
    }
}

/// The per-turn router of one host peer session.
pub(crate) struct TurnHostToolRouter {
    pub(crate) peers_root: PathBuf,
    pub(crate) slug: String,
    pub(crate) context_id: Option<String>,
    pub(crate) session_id: SessionKey,
    pub(crate) turn_id: String,
    pub(crate) version: u64,
    pub(crate) call_timeout: Duration,
    pub(crate) max_result_bytes: usize,
}

impl TurnHostToolRouter {
    fn error(kind: &str, message: impl Into<String>) -> HostToolCallOutcome {
        HostToolCallOutcome::Error {
            kind: kind.to_owned(),
            message: message.into(),
        }
    }
}

#[async_trait::async_trait]
impl HostToolRouter for TurnHostToolRouter {
    fn claim_occurrence(&self, tool_call_id: &str) -> bool {
        // Same occurrence shape as `peer_send_input` (calling session, turn,
        // provider tool-call id), scoped to this profile's peers root.
        let key = format!(
            "{}\u{0}{}/{}/{tool_call_id}",
            self.peers_root.display(),
            self.session_id.0,
            self.turn_id
        );
        let mut seen = HUB.occurrences.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        seen.retain(|_, at| now.duration_since(*at) < OCCURRENCE_RETENTION);
        if seen.contains_key(&key) {
            return false;
        }
        seen.insert(key, now);
        true
    }

    async fn call(&self, call: HostToolCall) -> HostToolCallOutcome {
        let key = route_key(&self.peers_root, &self.slug);
        let Some(send) = HUB
            .routes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
            .cloned()
        else {
            return Self::error(
                "host_unavailable",
                "the app's host is not connected (it must register its tools on a live connection)",
            );
        };
        let call_id = format!("ptc-{}", uuid::Uuid::new_v4().simple());
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut pending = HUB.pending.lock().unwrap_or_else(|p| p.into_inner());
            if pending.values().filter(|p| p.route_key == key).count() >= MAX_PENDING_CALLS_PER_PEER
            {
                return Self::error(
                    "host_busy",
                    format!("{MAX_PENDING_CALLS_PER_PEER} calls are already in flight"),
                );
            }
            pending.insert(
                call_id.clone(),
                PendingCall {
                    route_key: key,
                    max_result_bytes: self.max_result_bytes,
                    tx,
                },
            );
        }
        let mut guard = PendingGuard {
            call_id: call_id.clone(),
            send: send.clone(),
            armed: true,
        };
        let delivered = send(
            PEER_TOOL_CALL_NOTIFICATION,
            json!({
                "peer": self.slug,
                "session_id": self.session_id,
                "context_id": self.context_id,
                "turn_id": self.turn_id,
                "call_id": call_id,
                "tool_call_id": call.tool_call_id,
                "name": call.name,
                "args": call.args,
                "risk": call.risk.as_str(),
                "confirm_required": call.confirm_required,
                "timeout_ms": self.call_timeout.as_millis() as u64,
                "tools_version": self.version,
            }),
        );
        if !delivered {
            guard.armed = false;
            return Self::error("host_unavailable", "the app's host connection is closed");
        }
        match tokio::time::timeout(self.call_timeout, rx).await {
            Ok(Ok(outcome)) => {
                guard.armed = false;
                outcome
            }
            Ok(Err(_)) => {
                guard.armed = false;
                Self::error("cancelled", "the call was dropped before the app answered")
            }
            Err(_) => {
                guard.armed = false;
                drop(guard);
                let _ = send(
                    PEER_TOOL_CANCEL_NOTIFICATION,
                    json!({ "call_id": call_id, "reason": "timeout" }),
                );
                Self::error(
                    "timeout",
                    format!(
                        "the app did not answer within {} ms",
                        self.call_timeout.as_millis()
                    ),
                )
            }
        }
    }

    fn record(&self, audit: HostToolAudit) {
        let row = json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "peer": self.slug,
            "context_id": self.context_id,
            "session_id": self.session_id,
            "turn_id": self.turn_id,
            "tools_version": self.version,
            "tool": audit.tool,
            "tool_call_id": audit.tool_call_id,
            "risk": audit.risk,
            "decision": audit.decision,
            "outcome": audit.outcome,
            "duration_ms": audit.duration_ms,
            "args_bytes": audit.args_bytes,
            "result_bytes": audit.result_bytes,
        });
        let Some(dir) = staged_peer_dir(&self.peers_root, &self.slug) else {
            return;
        };
        if let Err(error) = peer_io::append_peer_line(&dir, TOOL_AUDIT_LEAF, &format!("{row}\n")) {
            tracing::warn!(slug = %self.slug, %error, "failed to append the peer tool audit row");
        }
    }

    fn call_timeout(&self) -> Duration {
        self.call_timeout
    }
}

#[cfg(test)]
pub(crate) fn pending_calls_for(peers_root: &Path, slug: &str) -> Vec<String> {
    let key = route_key(peers_root, slug);
    HUB.pending
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, call)| call.route_key == key)
        .map(|(id, _)| id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, risk: &str) -> ToolInput {
        serde_json::from_value(json!({
            "name": name,
            "description": format!("{name} tool"),
            "input_schema": {"type": "object"},
            "risk": risk,
        }))
        .unwrap()
    }

    #[test]
    fn should_validate_names_schemas_and_collisions() {
        assert!(validate_app_tool_name("news.list").is_ok());
        assert!(validate_app_tool_name("news.topics.get").is_ok());
        for bad in [
            "news",
            "News.list",
            "news..list",
            "news.list-all",
            "a.b.c.d.e",
        ] {
            assert!(validate_app_tool_name(bad).is_err(), "{bad}");
        }
        let set = build_tool_set(
            vec![tool("news.list", "read")],
            vec!["deep_search".into(), "deep_search".into()],
            ToolSetOptions::default(),
        )
        .unwrap();
        assert_eq!(set.tools[0].model_name, "news_list");
        assert_eq!(set.generic_tools, ["deep_search"]);
        assert_eq!(set.call_timeout_ms, DEFAULT_CALL_TIMEOUT_MS);

        let collision = build_tool_set(
            vec![tool("news.list", "read"), tool("news_list.x", "read")],
            vec!["news_list".into()],
            ToolSetOptions::default(),
        );
        assert!(collision.unwrap_err().contains("collides"));

        let mut not_object = tool("news.list", "read");
        not_object.input_schema = json!({"type": "string"});
        assert!(build_tool_set(vec![not_object], vec![], Default::default()).is_err());
        assert!(build_tool_set(vec![], vec!["rm -rf".into()], Default::default()).is_err());
    }

    #[test]
    fn should_accept_a_tools_json_entry_and_refuse_unknown_fields() {
        let tools: Vec<ToolInput> = serde_json::from_value(json!([
            {"name": "rinx.read_thread", "description": "Read a thread.",
             "input_schema": {"type": "object"}, "risk": "read",
             "background": true, "shareable": false},
            {"name": "rinx.send_message", "description": "Send a message.",
             "input_schema": {"type": "object", "required": ["text"]},
             "output_schema": {"type": "object"},
             "risk": "destructive", "confirm": "app"}
        ]))
        .unwrap();
        let set = build_tool_set(tools, vec![], Default::default()).unwrap();
        assert_eq!(set.tools[0].confirm, HostToolConfirm::Host, "default");
        assert!(set.tools[0].background);
        assert_eq!(set.tools[1].confirm, HostToolConfirm::App);
        assert_eq!(set.tools[1].risk, HostToolRisk::Destructive);

        let typo = serde_json::from_value::<ToolInput>(json!({
            "name": "rinx.x", "description": "x", "input_schema": {"type": "object"},
            "risk": "read", "confirmed": "app"
        }));
        assert!(typo.is_err(), "a misspelled field is refused, not ignored");
        let bad_confirm = serde_json::from_value::<ToolInput>(json!({
            "name": "rinx.x", "description": "x", "input_schema": {"type": "object"},
            "risk": "read", "confirm": "nobody"
        }));
        assert!(bad_confirm.is_err());
    }
}
