//! Host-routed app tools of a host-owned app peer (UPCR-2026-035).
//!
//! A host (an OctoSense shell) declares, per app peer, the app's own tools
//! (`news.list`, `mail.send`, …) with `peer/tools/register`. The kernel offers
//! the model exactly those tools plus the generic kernel tools the host names,
//! and each call to an app tool is routed back to the HOST, which implements
//! it where the capability lives. This module is the model-facing half: one
//! [`HostRoutedTool`] per declared tool. It knows nothing about the wire; the
//! serve path supplies a [`HostToolRouter`] per turn that delivers the call
//! (`peer/tool/call`), waits for `peer/tool/result`, and writes the audit log.
//!
//! Risk enforcement happens here, before the host is ever asked. A call is
//! **attended** when it comes from one of the app's interactive clients (an
//! open request context of the peer, the app's own conversation) AND the turn
//! carries an approval bridge; otherwise the person is absent.
//!
//! - `read` and `act` tools run. A tool not marked `background` runs only
//!   attended.
//! - A gated tool (`destructive`, or marked `outward`) with `confirm: host`
//!   needs an explicit approval through the turn's EXISTING approval bridge
//!   ([`TOOL_APPROVAL_CTX`]): the same `approval/requested` →
//!   `approval/respond` path every other tool uses, on the calling session,
//!   with the exact arguments in the request. Declined or expired → an error
//!   result for the model; the host is not called. No approval bridge in the
//!   turn → refused (fail closed), never run unasked.
//! - A gated tool with `confirm: app` (the app's own sheet asks the person)
//!   goes straight to the host when attended, with `confirm_required: true`
//!   and no kernel approval, so the person is never asked twice. With the
//!   person absent it needs the kernel approval exactly like `confirm: host`.
//! - Every non-`read` call is claimed once, before any approval or host
//!   call, under `<session>/<turn>/<tool_call_id>/<argument digest>`: a
//!   re-dispatch of the same call never raises a second approval or reaches
//!   the host twice, while a provider that reuses ids (`call_1`) with other
//!   arguments is not mistaken for a duplicate.
//! - A non-`read` call whose host answer does not arrive in time ends as
//!   `outcome_unknown` ("do not retry"), never as a plain failure the model
//!   would retry.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    ConcurrencyClass, TOOL_APPROVAL_CTX, Tool, ToolApprovalDecision, ToolApprovalRequest,
    ToolContext, ToolResult,
};

/// Maximum serialized size of one call's arguments.
pub const HOST_TOOL_MAX_ARGS_BYTES: usize = 64 * 1024;

/// A declared tool's risk (ADR 0002 section 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostToolRisk {
    /// Looks at something. Runs.
    Read,
    /// Changes the app's own state. Runs.
    Act,
    /// Deletes, sends, spends, or otherwise reaches past the app. Runs only
    /// after the person confirms.
    Destructive,
}

/// Who confirms a gated call with the person.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostToolConfirm {
    /// The kernel's approval request, in the app's conversation.
    #[default]
    Host,
    /// The app's own confirmation sheet, when the person is present.
    App,
}

impl HostToolConfirm {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::App => "app",
        }
    }
}

impl HostToolRisk {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Act => "act",
            Self::Destructive => "destructive",
        }
    }
}

/// One host-declared app tool, as validated by the kernel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostToolDecl {
    /// Declared name in the app's namespace, e.g. `news.list`.
    pub name: String,
    /// Name the model sees (`news_list`): provider tool names cannot hold `.`.
    pub model_name: String,
    pub description: String,
    /// JSON Schema object for the arguments.
    pub input_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    pub risk: HostToolRisk,
    /// May run in a turn with no interactive client.
    #[serde(default)]
    pub background: bool,
    /// Reaches past the app (send, post, share); gated like `destructive`.
    #[serde(default)]
    pub outward: bool,
    /// Who confirms a gated call when the person is present.
    #[serde(default)]
    pub confirm: HostToolConfirm,
}

impl HostToolDecl {
    /// Destructive or outward: the person must confirm.
    pub fn gated(&self) -> bool {
        self.risk == HostToolRisk::Destructive || self.outward
    }

    /// Whether a call needs the kernel's explicit approval first.
    pub fn requires_kernel_approval(&self, attended: bool) -> bool {
        self.gated() && !(attended && self.confirm == HostToolConfirm::App)
    }
}

/// One call handed to the router.
#[derive(Debug, Clone, PartialEq)]
pub struct HostToolCall {
    /// The provider's tool-call id (the occurrence within the turn).
    pub tool_call_id: String,
    /// Declared name (`news.list`).
    pub name: String,
    pub args: Value,
    pub risk: HostToolRisk,
    /// The app must confirm with the person itself (`confirm: app`,
    /// attended). `false` when the kernel already has the person's approval.
    pub confirm_required: bool,
    /// Destructive or outward: the host may still be waiting on the person,
    /// so the router honours an "awaiting confirmation" acknowledgement.
    pub gated: bool,
    /// `sha256:<hex>` of the arguments, for the host's own dedupe.
    pub args_digest: String,
}

/// How a routed call ended.
#[derive(Debug, Clone, PartialEq)]
pub enum HostToolCallOutcome {
    /// The host ran the tool; `data` is its structured result.
    Ok(Value),
    /// The call failed. `kind` is machine-readable (`host_error`,
    /// `timeout`, `host_unavailable`, `result_too_large`, `cancelled`, …).
    Error { kind: String, message: String },
}

/// One audit row, written by the router.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostToolAudit {
    pub tool: String,
    pub tool_call_id: String,
    pub risk: &'static str,
    /// `allowed`, `approved`, `app_confirms`, `denied`, `expired`,
    /// `approval_unavailable`, `duplicate`, `not_background`, `invalid_args`
    /// (and `late_result`, written by the router).
    pub decision: &'static str,
    /// `ok`, `error:<kind>`, `unknown` (no answer in time; the app may have
    /// acted), or `not_called`.
    pub outcome: String,
    pub duration_ms: u64,
    pub args_bytes: usize,
    pub result_bytes: usize,
}

/// The serve-side half: delivers calls to the host and records them.
#[async_trait]
pub trait HostToolRouter: Send + Sync {
    /// Claim the occurrence `(tool_call_id, args_digest)` in this turn.
    /// `false` means it was already claimed: the call is a duplicate.
    fn claim_occurrence(&self, tool_call_id: &str, args_digest: &str) -> bool;
    /// Deliver the call to the host and wait for its result (the router owns
    /// the timeout, cancellation and result size cap).
    async fn call(&self, call: HostToolCall) -> HostToolCallOutcome;
    /// Append one audit row.
    fn record(&self, audit: HostToolAudit);
    /// Upper bound of one host call, used as the tool's dispatch timeout.
    fn call_timeout(&self) -> Duration;
}

/// A host-declared app tool offered to the model.
pub struct HostRoutedTool {
    decl: HostToolDecl,
    router: Arc<dyn HostToolRouter>,
    approval_ttl: Duration,
    /// The calling session is one of the app's interactive clients (an open
    /// request context of the peer).
    interactive_session: bool,
}

impl HostRoutedTool {
    pub fn new(
        decl: HostToolDecl,
        router: Arc<dyn HostToolRouter>,
        approval_ttl: Duration,
        interactive_session: bool,
    ) -> Self {
        Self {
            decl,
            router,
            approval_ttl,
            interactive_session,
        }
    }

    pub fn decl(&self) -> &HostToolDecl {
        &self.decl
    }

    fn refuse(
        &self,
        ctx: &ToolContext,
        started: Instant,
        args_bytes: usize,
        decision: &'static str,
        message: String,
    ) -> ToolResult {
        self.router.record(HostToolAudit {
            tool: self.decl.name.clone(),
            tool_call_id: ctx.tool_id.clone(),
            risk: self.decl.risk.as_str(),
            decision,
            outcome: "not_called".into(),
            duration_ms: started.elapsed().as_millis() as u64,
            args_bytes,
            result_bytes: 0,
        });
        ToolResult {
            output: message,
            success: false,
            structured_metadata: Some(json!({
                "kind": "peer_host_tool_refused",
                "tool": self.decl.name,
                "decision": decision,
            })),
            ..Default::default()
        }
    }
}

/// Minimal argument check against the declared schema: an object carrying
/// every `required` property. The host validates fully.
fn check_args(schema: &Value, args: &Value) -> Result<(), String> {
    let Some(object) = args.as_object() else {
        return Err("arguments must be a JSON object".into());
    };
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        let missing: Vec<&str> = required
            .iter()
            .filter_map(Value::as_str)
            .filter(|key| !object.contains_key(*key))
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "missing required argument(s): {}",
                missing.join(", ")
            ));
        }
    }
    Ok(())
}

#[async_trait]
impl Tool for HostRoutedTool {
    // The model-facing name: provider tool names cannot hold the `.` of the
    // declared name.
    #[allow(clippy::misnamed_getters)]
    fn name(&self) -> &str {
        &self.decl.model_name
    }

    fn description(&self) -> &str {
        &self.decl.description
    }

    fn input_schema(&self) -> Value {
        self.decl.input_schema.clone()
    }

    fn concurrency_class(&self) -> ConcurrencyClass {
        match self.decl.risk {
            HostToolRisk::Read => ConcurrencyClass::Safe,
            _ => ConcurrencyClass::Exclusive,
        }
    }

    fn execution_timeout_secs(&self) -> Option<u64> {
        Some(self.router.call_timeout().as_secs().saturating_add(5))
    }

    fn blocks_on_human_input(&self) -> bool {
        self.decl.gated()
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        self.execute_with_context(&ToolContext::zero(), args).await
    }

    async fn execute_with_context(&self, ctx: &ToolContext, args: &Value) -> Result<ToolResult> {
        let started = Instant::now();
        let args_text = serde_json::to_string(args).unwrap_or_default();
        let args_bytes = args_text.len();
        if args_bytes > HOST_TOOL_MAX_ARGS_BYTES {
            return Ok(self.refuse(
                ctx,
                started,
                args_bytes,
                "invalid_args",
                format!(
                    "{}: arguments are {args_bytes} bytes (max {HOST_TOOL_MAX_ARGS_BYTES})",
                    self.decl.name
                ),
            ));
        }
        if let Err(err) = check_args(&self.decl.input_schema, args) {
            return Ok(self.refuse(
                ctx,
                started,
                args_bytes,
                "invalid_args",
                format!("{}: {err}", self.decl.name),
            ));
        }

        let approvals = TOOL_APPROVAL_CTX.try_with(Clone::clone).ok();
        let attended = self.interactive_session && approvals.is_some();
        if !attended && !self.decl.background {
            return Ok(self.refuse(
                ctx,
                started,
                args_bytes,
                "not_background",
                format!(
                    "{} may only run while the person is in the app (the app did not mark it background)",
                    self.decl.name
                ),
            ));
        }

        let args_digest = crate::approval::digest_tool_args(args);
        if self.decl.risk != HostToolRisk::Read
            && !self.router.claim_occurrence(&ctx.tool_id, &args_digest)
        {
            return Ok(self.refuse(
                ctx,
                started,
                args_bytes,
                "duplicate",
                format!(
                    "{}: this exact call was already submitted; it is not asked or sent twice",
                    self.decl.name
                ),
            ));
        }

        let decision = if self.decl.requires_kernel_approval(attended) {
            let Some(requester) = approvals else {
                return Ok(self.refuse(
                    ctx,
                    started,
                    args_bytes,
                    "approval_unavailable",
                    format!(
                        "{} needs the person's approval and no approval channel is available; it was not run",
                        self.decl.name
                    ),
                ));
            };
            let pretty = serde_json::to_string_pretty(args).unwrap_or(args_text.clone());
            let request = ToolApprovalRequest {
                tool_id: ctx.tool_id.clone(),
                tool_name: self.decl.model_name.clone(),
                title: format!("Approve {}", self.decl.name),
                body: format!(
                    "{} ({}{}) wants to run with these exact arguments:\n{pretty}",
                    self.decl.name,
                    self.decl.risk.as_str(),
                    if self.decl.outward { ", outward" } else { "" },
                ),
                command: None,
                cwd: None,
                once_only: true,
            };
            match tokio::time::timeout(self.approval_ttl, requester.request_approval(request)).await
            {
                Err(_) => {
                    return Ok(self.refuse(
                        ctx,
                        started,
                        args_bytes,
                        "expired",
                        format!(
                            "{}: the approval request expired before the person answered; it was not run",
                            self.decl.name
                        ),
                    ));
                }
                Ok(ToolApprovalDecision::Deny) => {
                    return Ok(self.refuse(
                        ctx,
                        started,
                        args_bytes,
                        "denied",
                        format!("{}: the person declined; it was not run", self.decl.name),
                    ));
                }
                Ok(ToolApprovalDecision::Approve) => "approved",
            }
        } else if self.decl.gated() {
            "app_confirms"
        } else {
            "allowed"
        };

        let outcome = self
            .router
            .call(HostToolCall {
                tool_call_id: ctx.tool_id.clone(),
                name: self.decl.name.clone(),
                args: args.clone(),
                risk: self.decl.risk,
                confirm_required: decision == "app_confirms",
                gated: self.decl.gated(),
                args_digest,
            })
            .await;
        let (result, outcome_label, result_bytes) = match outcome {
            HostToolCallOutcome::Ok(data) => {
                let output = serde_json::to_string(&data).unwrap_or_default();
                let bytes = output.len();
                (
                    ToolResult {
                        output,
                        success: true,
                        structured_metadata: Some(json!({
                            "kind": "peer_host_tool",
                            "tool": self.decl.name,
                            "decision": decision,
                        })),
                        ..Default::default()
                    },
                    "ok".to_owned(),
                    bytes,
                )
            }
            HostToolCallOutcome::Error { kind, message } => (
                ToolResult {
                    output: format!("{} failed ({kind}): {message}", self.decl.name),
                    success: false,
                    structured_metadata: Some(json!({
                        "kind": "peer_host_tool_error",
                        "tool": self.decl.name,
                        "error_kind": kind,
                        "decision": decision,
                    })),
                    ..Default::default()
                },
                if kind == "outcome_unknown" {
                    "unknown".to_owned()
                } else {
                    format!("error:{kind}")
                },
                message.len(),
            ),
        };
        self.router.record(HostToolAudit {
            tool: self.decl.name.clone(),
            tool_call_id: ctx.tool_id.clone(),
            risk: self.decl.risk.as_str(),
            decision,
            outcome: outcome_label,
            duration_ms: started.elapsed().as_millis() as u64,
            args_bytes,
            result_bytes,
        });
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolApprovalRequester;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct FakeRouter {
        calls: Mutex<Vec<HostToolCall>>,
        audits: Mutex<Vec<HostToolAudit>>,
        claimed: Mutex<std::collections::HashSet<String>>,
    }

    #[async_trait]
    impl HostToolRouter for FakeRouter {
        fn claim_occurrence(&self, id: &str, digest: &str) -> bool {
            self.claimed
                .lock()
                .unwrap()
                .insert(format!("{id}/{digest}"))
        }
        async fn call(&self, call: HostToolCall) -> HostToolCallOutcome {
            self.calls.lock().unwrap().push(call.clone());
            HostToolCallOutcome::Ok(json!({ "echo": call.args }))
        }
        fn record(&self, audit: HostToolAudit) {
            self.audits.lock().unwrap().push(audit);
        }
        fn call_timeout(&self) -> Duration {
            Duration::from_secs(30)
        }
    }

    impl FakeRouter {
        fn decisions(&self) -> Vec<&'static str> {
            self.audits
                .lock()
                .unwrap()
                .iter()
                .map(|a| a.decision)
                .collect()
        }
    }

    struct Approver {
        decision: Option<ToolApprovalDecision>,
        asked: AtomicUsize,
        last: Mutex<Option<ToolApprovalRequest>>,
    }

    impl Approver {
        fn new(decision: Option<ToolApprovalDecision>) -> Arc<Self> {
            Arc::new(Self {
                decision,
                asked: AtomicUsize::new(0),
                last: Mutex::new(None),
            })
        }
        fn asked(&self) -> usize {
            self.asked.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ToolApprovalRequester for Approver {
        async fn request_approval(&self, request: ToolApprovalRequest) -> ToolApprovalDecision {
            self.asked.fetch_add(1, Ordering::SeqCst);
            *self.last.lock().unwrap() = Some(request);
            match self.decision {
                Some(decision) => decision,
                None => std::future::pending().await,
            }
        }
    }

    fn decl(name: &str, risk: HostToolRisk) -> HostToolDecl {
        HostToolDecl {
            name: name.into(),
            model_name: name.replace('.', "_"),
            description: format!("{name} tool"),
            input_schema: json!({"type": "object", "required": ["id"]}),
            output_schema: None,
            risk,
            background: false,
            outward: false,
            confirm: HostToolConfirm::Host,
        }
    }

    /// A tool called from one of the app's interactive clients.
    fn in_app(decl: HostToolDecl, router: &Arc<FakeRouter>, ttl: Duration) -> HostRoutedTool {
        HostRoutedTool::new(decl, router.clone(), ttl, true)
    }

    /// A tool called from the peer's own session (no interactive client).
    fn in_peer(decl: HostToolDecl, router: &Arc<FakeRouter>, ttl: Duration) -> HostRoutedTool {
        HostRoutedTool::new(decl, router.clone(), ttl, false)
    }

    fn ctx(id: &str) -> ToolContext {
        ToolContext {
            tool_id: id.into(),
            ..ToolContext::zero()
        }
    }

    async fn run(
        tool: &HostRoutedTool,
        approver: Option<Arc<Approver>>,
        id: &str,
        args: Value,
    ) -> ToolResult {
        let ctx = ctx(id);
        match approver {
            Some(approver) => TOOL_APPROVAL_CTX
                .scope(
                    approver as Arc<dyn ToolApprovalRequester>,
                    tool.execute_with_context(&ctx, &args),
                )
                .await
                .unwrap(),
            None => tool.execute_with_context(&ctx, &args).await.unwrap(),
        }
    }

    const TTL: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn should_run_read_and_act_tools_without_approval() {
        let router = Arc::new(FakeRouter::default());
        for risk in [HostToolRisk::Read, HostToolRisk::Act] {
            let tool = in_app(decl("news.list", risk), &router, TTL);
            let approver = Approver::new(Some(ToolApprovalDecision::Deny));
            let result = run(&tool, Some(approver.clone()), "c1", json!({"id": 1})).await;
            assert!(result.success, "{}", result.output);
            assert_eq!(approver.asked(), 0);
        }
        assert_eq!(router.calls.lock().unwrap().len(), 2);
        assert_eq!(router.calls.lock().unwrap()[0].name, "news.list");
    }

    #[tokio::test]
    async fn should_run_destructive_only_after_an_explicit_approve_with_the_exact_arguments() {
        let router = Arc::new(FakeRouter::default());
        let tool = in_app(decl("mail.send", HostToolRisk::Destructive), &router, TTL);
        assert!(tool.blocks_on_human_input());
        let approver = Approver::new(Some(ToolApprovalDecision::Approve));
        let result = run(
            &tool,
            Some(approver.clone()),
            "c1",
            json!({"id": "draft-7"}),
        )
        .await;
        assert!(result.success);
        let asked = approver.last.lock().unwrap().clone().unwrap();
        assert_eq!(asked.tool_id, "c1");
        assert_eq!(asked.tool_name, "mail_send");
        assert!(asked.body.contains("\"draft-7\""), "{}", asked.body);
        assert!(asked.once_only, "never answered or remembered by a scope");
        let calls = router.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(!calls[0].confirm_required, "the person already approved");
        assert_eq!(router.decisions(), ["approved"]);
    }

    #[tokio::test]
    async fn should_never_call_the_host_when_approval_is_declined_expired_or_unavailable() {
        let router = Arc::new(FakeRouter::default());
        let mut gated = decl("mail.send", HostToolRisk::Destructive);
        gated.background = true;
        let tool = in_app(gated, &router, Duration::from_millis(50));

        let denied = run(
            &tool,
            Some(Approver::new(Some(ToolApprovalDecision::Deny))),
            "c1",
            json!({"id": 1}),
        )
        .await;
        assert!(!denied.success && denied.output.contains("declined"));

        let expired = run(&tool, Some(Approver::new(None)), "c2", json!({"id": 1})).await;
        assert!(!expired.success && expired.output.contains("expired"));

        let unavailable = run(&tool, None, "c3", json!({"id": 1})).await;
        assert!(!unavailable.success && unavailable.output.contains("no approval channel"));

        assert!(router.calls.lock().unwrap().is_empty());
        assert_eq!(
            router.decisions(),
            ["denied", "expired", "approval_unavailable"]
        );
    }

    #[tokio::test]
    async fn should_gate_an_outward_act_tool_like_destructive() {
        let router = Arc::new(FakeRouter::default());
        let mut post = decl("social.post", HostToolRisk::Act);
        post.outward = true;
        let tool = in_app(post, &router, TTL);
        let approver = Approver::new(Some(ToolApprovalDecision::Deny));
        let result = run(&tool, Some(approver.clone()), "c1", json!({"id": 1})).await;
        assert!(!result.success);
        assert_eq!(approver.asked(), 1);
        assert!(router.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn should_raise_one_approval_per_occurrence() {
        let router = Arc::new(FakeRouter::default());
        let tool = in_app(decl("mail.send", HostToolRisk::Destructive), &router, TTL);
        let approver = Approver::new(Some(ToolApprovalDecision::Approve));
        assert!(
            run(&tool, Some(approver.clone()), "c1", json!({"id": 1}))
                .await
                .success
        );
        let again = run(&tool, Some(approver.clone()), "c1", json!({"id": 1})).await;
        assert!(!again.success && again.output.contains("not asked or sent twice"));
        assert_eq!(approver.asked(), 1);
        assert_eq!(router.calls.lock().unwrap().len(), 1);

        // A provider reusing `c1` with other arguments is a new call.
        assert!(
            run(&tool, Some(approver.clone()), "c1", json!({"id": 2}))
                .await
                .success
        );
        assert_eq!(approver.asked(), 2);
        assert!(
            router.calls.lock().unwrap()[0]
                .args_digest
                .starts_with("sha256:")
        );
    }

    #[tokio::test]
    async fn should_send_an_act_call_once_but_let_reads_repeat() {
        let router = Arc::new(FakeRouter::default());
        let approver = Approver::new(Some(ToolApprovalDecision::Approve));
        let act = in_app(decl("news.topics_set", HostToolRisk::Act), &router, TTL);
        assert!(
            run(&act, Some(approver.clone()), "c1", json!({"id": 1}))
                .await
                .success
        );
        let again = run(&act, Some(approver.clone()), "c1", json!({"id": 1})).await;
        assert!(!again.success && again.output.contains("not asked or sent twice"));

        let read = in_app(decl("news.list", HostToolRisk::Read), &router, TTL);
        assert!(
            run(&read, Some(approver.clone()), "c2", json!({"id": 1}))
                .await
                .success
        );
        assert!(
            run(&read, Some(approver.clone()), "c2", json!({"id": 1}))
                .await
                .success
        );
        assert_eq!(router.calls.lock().unwrap().len(), 3);
        assert_eq!(approver.asked(), 0);
    }

    #[tokio::test]
    async fn should_report_an_unanswered_act_call_as_unknown_not_failed() {
        struct SilentRouter(Mutex<Vec<HostToolAudit>>);
        #[async_trait]
        impl HostToolRouter for SilentRouter {
            fn claim_occurrence(&self, _: &str, _: &str) -> bool {
                true
            }
            async fn call(&self, _: HostToolCall) -> HostToolCallOutcome {
                HostToolCallOutcome::Error {
                    kind: "outcome_unknown".into(),
                    message: "do not retry".into(),
                }
            }
            fn record(&self, audit: HostToolAudit) {
                self.0.lock().unwrap().push(audit);
            }
            fn call_timeout(&self) -> Duration {
                Duration::from_secs(1)
            }
        }
        let router = Arc::new(SilentRouter(Mutex::new(Vec::new())));
        let tool = HostRoutedTool::new(
            decl("news.topics_set", HostToolRisk::Act),
            router.clone(),
            TTL,
            true,
        );
        let approver = Approver::new(Some(ToolApprovalDecision::Approve));
        let result = run(&tool, Some(approver), "c1", json!({"id": 1})).await;
        assert!(!result.success && result.output.contains("do not retry"));
        assert_eq!(router.0.lock().unwrap()[0].outcome, "unknown");
    }

    #[tokio::test]
    async fn should_let_the_app_confirm_when_the_person_is_present() {
        let router = Arc::new(FakeRouter::default());
        let mut send = decl("rinx.send_message", HostToolRisk::Destructive);
        send.confirm = HostToolConfirm::App;
        let tool = in_app(send, &router, TTL);
        let approver = Approver::new(Some(ToolApprovalDecision::Deny));
        assert!(
            run(&tool, Some(approver.clone()), "c1", json!({"id": 1}))
                .await
                .success
        );
        assert_eq!(approver.asked(), 0, "never asked twice");
        assert!(router.calls.lock().unwrap()[0].confirm_required);
        assert_eq!(router.decisions(), ["app_confirms"]);
    }

    #[tokio::test]
    async fn should_ask_for_a_kernel_approval_when_an_app_confirmed_tool_runs_without_the_person() {
        let router = Arc::new(FakeRouter::default());
        let mut send = decl("rinx.send_message", HostToolRisk::Destructive);
        send.confirm = HostToolConfirm::App;
        send.background = true;

        // The peer's own session (a background run): the approval goes to the
        // app's conversation, and an approved call is not confirmed again.
        let tool = in_peer(send.clone(), &router, TTL);
        let approver = Approver::new(Some(ToolApprovalDecision::Approve));
        assert!(
            run(&tool, Some(approver.clone()), "c1", json!({"id": 1}))
                .await
                .success
        );
        assert_eq!(approver.asked(), 1);
        assert!(!router.calls.lock().unwrap()[0].confirm_required);

        // An in-app session whose turn has no approval bridge: the person is
        // not reachable, so the app cannot confirm either.
        let tool = in_app(send, &router, TTL);
        let refused = run(&tool, None, "c2", json!({"id": 1})).await;
        assert!(!refused.success && refused.output.contains("no approval channel"));
        assert_eq!(router.calls.lock().unwrap().len(), 1);
        assert_eq!(router.decisions(), ["approved", "approval_unavailable"]);
    }

    #[tokio::test]
    async fn should_refuse_foreground_tools_unattended_and_bad_arguments() {
        let router = Arc::new(FakeRouter::default());
        let approver = Approver::new(Some(ToolApprovalDecision::Approve));
        let foreground = decl("news.list", HostToolRisk::Read);

        let no_bridge = run(
            &in_app(foreground.clone(), &router, TTL),
            None,
            "c1",
            json!({"id": 1}),
        )
        .await;
        assert!(!no_bridge.success && no_bridge.output.contains("background"));
        let peer_session = run(
            &in_peer(foreground.clone(), &router, TTL),
            Some(approver.clone()),
            "c2",
            json!({"id": 1}),
        )
        .await;
        assert!(!peer_session.success && peer_session.output.contains("background"));

        let tool = in_app(foreground, &router, TTL);
        let missing = run(&tool, Some(approver.clone()), "c3", json!({})).await;
        assert!(!missing.success && missing.output.contains("id"));
        let huge = json!({"id": "x".repeat(HOST_TOOL_MAX_ARGS_BYTES)});
        let too_big = run(&tool, Some(approver.clone()), "c4", huge).await;
        assert!(!too_big.success && too_big.output.contains("bytes"));
        assert!(router.calls.lock().unwrap().is_empty());

        let mut background = decl("news.list", HostToolRisk::Read);
        background.background = true;
        let tool = in_peer(background, &router, TTL);
        assert!(run(&tool, None, "c5", json!({"id": 1})).await.success);
    }
}
