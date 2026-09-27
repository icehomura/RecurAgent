//! UPCR-2026-035 — host-registered tools of a host-owned app peer:
//! `peer/tools/register` authorization and replacement, exact tool
//! visibility per turn, routing to the host (`peer/tool/call` →
//! `peer/tool/result`), timeouts, and risk gating through the existing
//! approval path.
use super::*;

use crate::peers::host_tools::{apply_session_host_tools, resolve_session_host_tools};
use octos_core::ui_protocol::ApprovalRespondParams;

struct Fx {
    _tmp: tempfile::TempDir,
    state: Arc<AppState>,
    runtime: Arc<crate::runtime::ProfileRuntime>,
    data_dir: PathBuf,
    apps: PathBuf,
    system: SessionKey,
}

async fn fixture() -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let profile = crate::profiles::UserProfile {
        id: "dev".to_string(),
        name: "Dev".to_string(),
        enabled: true,
        data_dir: None,
        parent_id: None,
        public_subdomain: None,
        config: crate::profiles::ProfileConfig {
            llm: Some(crate::profiles::LlmProfileConfig {
                primary: Some(crate::profiles::LlmModelSelectionConfig {
                    family_id: Some("openai".to_string()),
                    model_id: Some("gpt-4o-mini".to_string()),
                    route: Some(crate::profiles::LlmRouteConfig {
                        route_id: None,
                        label: None,
                        base_url: None,
                        api_key_env: Some("HOST_TOOLS_TEST_KEY".to_string()),
                        api_type: None,
                    }),
                    ..Default::default()
                }),
                fallbacks: Vec::new(),
            }),
            env_vars: [("HOST_TOOLS_TEST_KEY".to_string(), "test-key".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        },
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let runtime = crate::runtime::ProfileRuntime::bootstrap(
        &profile,
        &data_dir,
        None,
        crate::runtime::BootstrapRole::Serve,
    )
    .await
    .expect("bootstrap dev runtime");
    let mut state = AppState::empty_for_tests();
    state.profiles.insert("dev".to_string(), runtime.clone());
    let apps = tmp.path().join("apps");
    std::fs::create_dir_all(apps.join("news")).unwrap();
    Fx {
        state: Arc::new(state),
        runtime,
        data_dir,
        apps,
        system: SessionKey::with_profile_topic("dev", "api", "host", "system"),
        _tmp: tmp,
    }
}

fn ws_connection_for_test(capacity: usize) -> (WsConnection, mpsc::Receiver<WsMessage>) {
    let (tx, rx) = mpsc::channel(capacity);
    (WsConnection::new(tx), rx)
}

fn rpc(method: &str, params: Value) -> RpcRequest<Value> {
    RpcRequest::new("host-1".to_string(), method, params)
}

/// Stage the News app peer; returns its host token.
async fn prepare_news(fx: &Fx) -> String {
    let result = raw_peer_prepare(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_PREPARE,
            json!({
                "brief": "You are the News app's assistant.",
                "names": ["News"],
                "cwd": fx.apps.join("news").to_string_lossy(),
                "session_id": fx.system,
                "memory_namespace": "app/news/acct-1",
                "resume": true,
            }),
        ),
        None,
    )
    .await
    .expect("stage the news peer");
    result["host_token"].as_str().unwrap().to_owned()
}

fn peer_key(fx: &Fx) -> SessionKey {
    SessionKey(format!("{}#peer-news", fx.system.base_key()))
}

fn peers_root(fx: &Fx) -> PathBuf {
    fx.data_dir.join("peers")
}

fn register(fx: &Fx, ws: &WsConnection, token: &str, extra: Value) -> Result<Value, RpcError> {
    let mut params = json!({
        "session_id": fx.system,
        "peer": "news",
        "host_token": token,
    });
    for (key, value) in extra.as_object().unwrap() {
        params[key] = value.clone();
    }
    raw_peer_tools_register(
        ws,
        &fx.state,
        &rpc(APPUI_METHOD_PEER_TOOLS_REGISTER, params),
        None,
    )
}

fn news_list() -> Value {
    json!({
        "name": "news.list",
        "description": "List the latest news items.",
        "input_schema": {"type": "object", "properties": {"topic": {"type": "string"}}},
        "output_schema": {"type": "object"},
        "risk": "read",
        "background": true,
    })
}

fn mail_send() -> Value {
    json!({
        "name": "mail.send",
        "description": "Send a drafted mail.",
        "input_schema": {"type": "object", "required": ["draft_id"]},
        "risk": "destructive",
        "background": true,
    })
}

/// The tool registry one turn of `key` would offer the model.
async fn turn_registry(fx: &Fx, key: &SessionKey, turn: &str) -> octos_agent::ToolRegistry {
    let runtime = crate::runtime::SessionRuntime::bootstrap(&fx.runtime, key.clone(), None)
        .await
        .expect("bound session");
    let mut registry = runtime.tools.snapshot_excluding(&[]);
    let resolved = resolve_session_host_tools(&peers_root(fx), key);
    apply_session_host_tools(&mut registry, &resolved, &peers_root(fx), key, turn);
    registry
}

fn sorted_names(registry: &octos_agent::ToolRegistry) -> Vec<String> {
    let mut names = registry.tool_names();
    names.sort();
    names
}

fn call_ctx(id: &str) -> octos_agent::tools::ToolContext {
    octos_agent::tools::ToolContext {
        tool_id: id.to_owned(),
        ..octos_agent::tools::ToolContext::zero()
    }
}

fn frame_json(message: WsMessage) -> Value {
    let WsMessage::Text(text) = message else {
        panic!("expected a text frame");
    };
    serde_json::from_str(text.as_str()).expect("json frame")
}

/// Next notification with `method` (skips any other frame).
async fn next_frame(rx: &mut mpsc::Receiver<WsMessage>, method: &str) -> Value {
    loop {
        let message = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("a frame in time")
            .expect("connection open");
        let frame = frame_json(message);
        if frame["method"] == method {
            return frame["params"].clone();
        }
    }
}

fn answer(fx: &Fx, token: &str, call_id: &str, reply: Value) -> Result<Value, RpcError> {
    let mut params = json!({
        "session_id": fx.system,
        "peer": "news",
        "host_token": token,
        "call_id": call_id,
    });
    for (key, value) in reply.as_object().unwrap() {
        params[key] = value.clone();
    }
    raw_peer_tool_result(&fx.state, &rpc(APPUI_METHOD_PEER_TOOL_RESULT, params), None)
}

/// A fake host: answers every `peer/tool/call` with `reply(params)`.
fn spawn_fake_host(
    fx: &Fx,
    token: String,
    mut rx: mpsc::Receiver<WsMessage>,
    reply: impl Fn(&Value) -> Value + Send + 'static,
) -> tokio::task::JoinHandle<Vec<Value>> {
    let state = fx.state.clone();
    let system = fx.system.clone();
    tokio::spawn(async move {
        let mut calls = Vec::new();
        while let Some(message) = rx.recv().await {
            let frame = frame_json(message);
            if frame["method"] != "peer/tool/call" {
                continue;
            }
            let params = frame["params"].clone();
            let mut body = json!({
                "session_id": system,
                "peer": "news",
                "host_token": token,
                "call_id": params["call_id"],
            });
            for (key, value) in reply(&params).as_object().unwrap() {
                body[key] = value.clone();
            }
            raw_peer_tool_result(&state, &rpc(APPUI_METHOD_PEER_TOOL_RESULT, body), None)
                .expect("the kernel accepts the result");
            calls.push(params);
        }
        calls
    })
}

fn audit_rows(fx: &Fx) -> Vec<Value> {
    std::fs::read_to_string(peers_root(fx).join("news/tool_audit.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn should_advertise_and_dispatch_the_peer_tool_methods() {
    for method in [
        APPUI_METHOD_PEER_TOOLS_REGISTER,
        APPUI_METHOD_PEER_TOOL_RESULT,
    ] {
        assert!(APPUI_EXTRA_METHODS.contains(&method), "{method} advertised");
        assert!(
            raw_method_is_dispatched(method, false),
            "{method} dispatched"
        );
    }
}

#[tokio::test]
async fn should_refuse_a_registration_without_the_host_token() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let (ws, _rx) = ws_connection_for_test(8);
    let tools = json!({ "tools": [news_list()] });

    for bad in ["guess", ""] {
        let err = register(&fx, &ws, bad, tools.clone()).expect_err("wrong token");
        assert_eq!(err.data.unwrap()["kind"], "peer_host_token_mismatch");
    }
    let mut foreign = json!({
        "session_id": SessionKey::with_profile_topic("dev", "api", "host", "other"),
        "peer": "news", "host_token": token,
    });
    foreign["tools"] = tools["tools"].clone();
    let err = raw_peer_tools_register(
        &ws,
        &fx.state,
        &rpc(APPUI_METHOD_PEER_TOOLS_REGISTER, foreign),
        None,
    )
    .expect_err("foreign originator");
    assert_eq!(err.data.unwrap()["kind"], "peer_originator_mismatch");
    assert!(
        !peers_root(&fx).join("news/host_tools.json").exists(),
        "a refused registration writes nothing"
    );

    let invalid = register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [{"name": "list", "description": "x",
                           "input_schema": {"type": "object"}, "risk": "read"}] }),
    )
    .expect_err("un-namespaced name");
    assert_eq!(invalid.data.unwrap()["kind"], "peer_tools_invalid");

    let ok = register(&fx, &ws, &token, tools).expect("registered");
    assert_eq!(ok["version"], 1);
    assert_eq!(ok["tools"][0]["model_name"], "news_list");
    assert_eq!(ok["applies"], "next_turn");
}

#[tokio::test]
async fn should_offer_exactly_the_registered_tools_and_refuse_an_unlisted_one() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);

    // Before registration the peer keeps the ordinary roster.
    let before = turn_registry(&fx, &key, "turn-0").await;
    assert!(before.get("shell").is_some() && before.get("read_file").is_some());

    let (ws, _rx) = ws_connection_for_test(8);
    register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [news_list()], "generic_tools": ["read_file", "no_such_tool"] }),
    )
    .expect("registered");

    let registry = turn_registry(&fx, &key, "turn-1").await;
    assert_eq!(sorted_names(&registry), ["news_list", "read_file"]);
    let specs: Vec<String> = registry.specs().into_iter().map(|s| s.name).collect();
    assert!(!specs.iter().any(|name| name == "shell"), "{specs:?}");

    // Forcing an unlisted tool is refused by the registry itself.
    let forced = registry
        .execute_with_context(&call_ctx("c1"), "shell", &json!({"command": "id"}))
        .await;
    let Err(error) = forced else {
        panic!("an unlisted tool must be refused");
    };
    assert!(error.to_string().contains("unknown tool"), "{error}");

    // Request contexts of the peer get the same roster.
    let opened = raw_peer_context_open(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_CONTEXT_OPEN,
            json!({"session_id": fx.system, "peer": "news", "context_id": "mini-1",
                   "host_token": token}),
        ),
        None,
    )
    .expect("context open");
    let context_key: SessionKey = serde_json::from_value(opened["session_id"].clone()).unwrap();
    let registry = turn_registry(&fx, &context_key, "turn-2").await;
    assert_eq!(sorted_names(&registry), ["news_list", "read_file"]);

    // An unreadable set fails closed: no tools at all.
    std::fs::write(peers_root(&fx).join("news/host_tools.json"), "{torn").unwrap();
    let registry = turn_registry(&fx, &key, "turn-3").await;
    assert!(registry.tool_names().is_empty());
}

#[tokio::test]
async fn should_route_an_app_tool_call_to_the_host_and_back() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (ws, rx) = ws_connection_for_test(32);
    register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [news_list()], "max_result_bytes": 64 }),
    )
    .unwrap();
    let host = spawn_fake_host(&fx, token.clone(), rx, |params| {
        if params["args"]["topic"] == "huge" {
            json!({ "ok": true, "data": {"items": ["x".repeat(200)]} })
        } else {
            json!({ "ok": true, "data": {"items": ["hn-1"], "topic": params["args"]["topic"]} })
        }
    });

    let registry = turn_registry(&fx, &key, "turn-1").await;
    let result = registry
        .execute_with_context(&call_ctx("c1"), "news_list", &json!({"topic": "rust"}))
        .await
        .unwrap();
    assert!(result.success, "{}", result.output);
    let data: Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(data, json!({"items": ["hn-1"], "topic": "rust"}));

    let too_big = registry
        .execute_with_context(&call_ctx("c2"), "news_list", &json!({"topic": "huge"}))
        .await
        .unwrap();
    assert!(!too_big.success && too_big.output.contains("result_too_large"));

    drop(registry);
    drop(ws);
    crate::peers::host_tools::set_host_route(&peers_root(&fx), "news", Arc::new(|_, _| false));
    let calls = host.await.unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["name"], "news.list");
    assert_eq!(calls[0]["session_id"], json!(key));
    assert_eq!(calls[0]["tool_call_id"], "c1");
    assert_eq!(calls[0]["risk"], "read");

    let rows = audit_rows(&fx);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["tool"], "news.list");
    assert_eq!(rows[0]["decision"], "allowed");
    assert_eq!(rows[0]["outcome"], "ok");
    assert_eq!(rows[1]["outcome"], "error:result_too_large");
    assert!(rows[0]["args_bytes"].as_u64().unwrap() > 0);
    assert!(rows[0]["result_bytes"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn should_time_out_and_cancel_a_call_the_host_never_answers() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (ws, mut rx) = ws_connection_for_test(32);
    register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [news_list()], "call_timeout_ms": 100 }),
    )
    .unwrap();

    let registry = turn_registry(&fx, &key, "turn-1").await;
    let result = registry
        .execute_with_context(&call_ctx("c1"), "news_list", &json!({}))
        .await
        .unwrap();
    assert!(
        !result.success && result.output.contains("timeout"),
        "{}",
        result.output
    );

    let call = next_frame(&mut rx, "peer/tool/call").await;
    let cancel = next_frame(&mut rx, "peer/tool/cancel").await;
    assert_eq!(cancel["call_id"], call["call_id"]);
    assert_eq!(cancel["reason"], "timeout");

    // A late answer finds nothing to complete.
    let late = answer(
        &fx,
        &token,
        call["call_id"].as_str().unwrap(),
        json!({"ok": true, "data": {}}),
    )
    .expect_err("late result");
    assert_eq!(late.data.unwrap()["kind"], "peer_tool_call_not_found");
    assert!(crate::peers::host_tools::pending_calls_for(&peers_root(&fx), "news").is_empty());

    // With no live host at all the call fails without waiting.
    crate::peers::host_tools::set_host_route(&peers_root(&fx), "news", Arc::new(|_, _| false));
    let result = registry
        .execute_with_context(&call_ctx("c2"), "news_list", &json!({}))
        .await
        .unwrap();
    assert!(
        result.output.contains("host_unavailable"),
        "{}",
        result.output
    );
}

/// The app's own conversation: the approval bridge of a turn on the peer's
/// session, as the serve turn installs it.
fn app_approver(
    fx: &Fx,
    key: &SessionKey,
    contracts: &Arc<UiProtocolContractStores>,
    turn: &TurnId,
) -> Arc<dyn octos_agent::ToolApprovalRequester> {
    let (ws, rx) = ws_connection_for_test(64);
    std::mem::forget(rx);
    Arc::new(UiProtocolApprovalRequester {
        ws,
        ledger: Arc::new(UiProtocolLedger::new(64)),
        contracts: contracts.clone(),
        state: fx.state.clone(),
        peers_root: peers_root(fx),
        session_id: key.clone(),
        turn_id: turn.clone(),
        features: ConnectionUiFeatures::default(),
    })
}

async fn wait_for_pending(
    contracts: &UiProtocolContractStores,
    key: &SessionKey,
) -> Vec<ApprovalRequestedEvent> {
    for _ in 0..200 {
        let pending = contracts.approvals.pending_for_session(key);
        if !pending.is_empty() {
            return pending;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("no approval was raised");
}

#[tokio::test]
async fn should_run_a_destructive_tool_only_after_the_persons_approval() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (ws, rx) = ws_connection_for_test(32);
    register(&fx, &ws, &token, json!({ "tools": [mail_send()] })).unwrap();
    let host = spawn_fake_host(
        &fx,
        token.clone(),
        rx,
        |_| json!({ "ok": true, "data": {"sent": true} }),
    );
    let registry = Arc::new(turn_registry(&fx, &key, "turn-1").await);
    let args = json!({"draft_id": "d-42"});

    // No interactive client (no approval bridge): refused, the host is not asked.
    let unattended = registry
        .execute_with_context(&call_ctx("c0"), "mail_send", &args)
        .await
        .unwrap();
    assert!(!unattended.success && unattended.output.contains("approval"));

    // With the app's conversation attached: an approval with the exact args.
    let contracts = Arc::new(UiProtocolContractStores::default());
    let turn = TurnId::new();
    let approver = app_approver(&fx, &key, &contracts, &turn);
    let run = {
        let registry = registry.clone();
        let args = args.clone();
        tokio::spawn(
            octos_agent::tools::TOOL_APPROVAL_CTX.scope(approver, async move {
                registry
                    .execute_with_context(&call_ctx("c1"), "mail_send", &args)
                    .await
                    .unwrap()
            }),
        )
    };
    let pending = wait_for_pending(&contracts, &key).await;
    assert_eq!(pending.len(), 1);
    let approval_id = pending[0].approval_id.clone();

    // The system agent cannot answer it (the person answers in the app),
    // even with the peer's session open on the wire.
    crate::peers::peer_wire_registry()
        .register(crate::peers::peer_wire_key("dev", "news"), key.clone());
    let refused = crate::peers::peer_respond_resolve(
        &peers_root(&fx),
        &fx.system.0,
        "dev",
        &contracts,
        &|_, _| panic!("nothing is decided"),
        octos_agent::PeerRespondRequest {
            slug: "news".into(),
            id: Some(approval_id.0.to_string()),
            decision: Some("approve".into()),
            answers: None,
        },
    )
    .expect_err("the system agent may not approve");
    assert!(refused.contains("person in the app"), "{refused}");
    assert_eq!(contracts.approvals.pending_for_session(&key).len(), 1);

    // The person approves on the peer's session: the call runs.
    contracts
        .approvals
        .respond_with_context(ApprovalRespondParams::new(
            key.clone(),
            approval_id,
            ApprovalDecision::Approve,
        ))
        .expect("the person approves");
    let result = run.await.unwrap();
    assert!(result.success, "{}", result.output);

    drop(ws);
    crate::peers::host_tools::set_host_route(&peers_root(&fx), "news", Arc::new(|_, _| false));
    let calls = host.await.unwrap();
    assert_eq!(calls.len(), 1, "only the approved call reached the host");
    assert_eq!(calls[0]["args"], args);
    let decisions: Vec<Value> = audit_rows(&fx)
        .iter()
        .map(|r| r["decision"].clone())
        .collect();
    assert_eq!(
        decisions,
        [json!("approval_unavailable"), json!("approved")]
    );
}

#[tokio::test]
async fn should_not_run_a_declined_or_expired_destructive_call_nor_ask_twice() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (ws, mut rx) = ws_connection_for_test(32);
    register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [mail_send()], "approval_ttl_secs": 1 }),
    )
    .unwrap();
    let registry = Arc::new(turn_registry(&fx, &key, "turn-1").await);
    let args = json!({"draft_id": "d-42"});
    let contracts = Arc::new(UiProtocolContractStores::default());
    let turn = TurnId::new();

    // Declined.
    let run = {
        let registry = registry.clone();
        let args = args.clone();
        let approver = app_approver(&fx, &key, &contracts, &turn);
        tokio::spawn(
            octos_agent::tools::TOOL_APPROVAL_CTX.scope(approver, async move {
                registry
                    .execute_with_context(&call_ctx("c1"), "mail_send", &args)
                    .await
                    .unwrap()
            }),
        )
    };
    let pending = wait_for_pending(&contracts, &key).await;
    contracts
        .approvals
        .respond_with_context(ApprovalRespondParams::new(
            key.clone(),
            pending[0].approval_id.clone(),
            ApprovalDecision::Deny,
        ))
        .unwrap();
    let declined = run.await.unwrap();
    assert!(!declined.success && declined.output.contains("declined"));

    // The same occurrence (session/turn/tool_call_id) is never asked twice.
    let approver = app_approver(&fx, &key, &contracts, &turn);
    let again = octos_agent::tools::TOOL_APPROVAL_CTX
        .scope(
            approver.clone(),
            registry.execute_with_context(&call_ctx("c1"), "mail_send", &args),
        )
        .await
        .unwrap();
    assert!(!again.success && again.output.contains("not asked twice"));
    assert!(contracts.approvals.pending_for_session(&key).is_empty());

    // Expired: nobody answers within the TTL; the parked approval is released.
    let expired = octos_agent::tools::TOOL_APPROVAL_CTX
        .scope(
            approver,
            registry.execute_with_context(&call_ctx("c2"), "mail_send", &args),
        )
        .await
        .unwrap();
    assert!(
        !expired.success && expired.output.contains("expired"),
        "{}",
        expired.output
    );
    assert!(
        contracts.approvals.pending_for_session(&key).is_empty(),
        "an expired approval does not stay pending"
    );

    // The host never saw a call.
    assert!(rx.try_recv().is_err());
    let decisions: Vec<Value> = audit_rows(&fx)
        .iter()
        .map(|r| r["decision"].clone())
        .collect();
    assert_eq!(
        decisions,
        [json!("denied"), json!("duplicate"), json!("expired")]
    );
}

#[tokio::test]
async fn should_replace_the_tool_set_atomically_and_refuse_a_stale_version() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (ws, _rx) = ws_connection_for_test(8);
    register(&fx, &ws, &token, json!({ "tools": [news_list()] })).unwrap();
    assert_eq!(
        sorted_names(&turn_registry(&fx, &key, "t1").await),
        ["news_list"]
    );

    let mut read = news_list();
    read["name"] = json!("news.read");
    let v2 = register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [read], "if_version": 1 }),
    )
    .unwrap();
    assert_eq!(v2["version"], 2);
    assert_eq!(v2["previous_version"], 1);
    assert_eq!(
        sorted_names(&turn_registry(&fx, &key, "t2").await),
        ["news_read"],
        "the old tool is gone, not merged"
    );

    let stale = register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [news_list()], "if_version": 1 }),
    )
    .expect_err("stale version");
    let data = stale.data.unwrap();
    assert_eq!(data["kind"], "peer_tools_version_conflict");
    assert_eq!(data["current_version"], 2);

    // An empty registration leaves the model with no tools, never the default roster.
    register(&fx, &ws, &token, json!({ "tools": [] })).unwrap();
    assert!(turn_registry(&fx, &key, "t3").await.tool_names().is_empty());
}

#[tokio::test]
async fn should_let_the_app_confirm_when_the_person_is_in_the_app_and_ask_otherwise() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (ws, rx) = ws_connection_for_test(32);
    register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [{
            "name": "news.share",
            "description": "Share a story with a contact.",
            "input_schema": {"type": "object", "required": ["story"]},
            "risk": "destructive",
            "confirm": "app",
            "background": true,
        }] }),
    )
    .unwrap();
    let host = spawn_fake_host(
        &fx,
        token.clone(),
        rx,
        |_| json!({ "ok": true, "data": {"shared": true} }),
    );
    let opened = raw_peer_context_open(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_CONTEXT_OPEN,
            json!({"session_id": fx.system, "peer": "news", "context_id": "ui-1",
                   "host_token": token}),
        ),
        None,
    )
    .unwrap();
    let context_key: SessionKey = serde_json::from_value(opened["session_id"].clone()).unwrap();
    let args = json!({"story": "hn-1"});

    // Person present (the app's own conversation): straight to the app's
    // confirmation sheet, no kernel approval.
    let contracts = Arc::new(UiProtocolContractStores::default());
    let registry = turn_registry(&fx, &context_key, "turn-1").await;
    let approver = app_approver(&fx, &context_key, &contracts, &TurnId::new());
    let present = octos_agent::tools::TOOL_APPROVAL_CTX
        .scope(
            approver,
            registry.execute_with_context(&call_ctx("c1"), "news_share", &args),
        )
        .await
        .unwrap();
    assert!(present.success, "{}", present.output);
    assert!(
        contracts
            .approvals
            .pending_for_session(&context_key)
            .is_empty()
    );

    // Person absent (the peer's own session): an approval request in the
    // app's conversation, and an approved call is not confirmed again.
    let registry = Arc::new(turn_registry(&fx, &key, "turn-2").await);
    let approver = app_approver(&fx, &key, &contracts, &TurnId::new());
    let run = {
        let registry = registry.clone();
        let args = args.clone();
        tokio::spawn(
            octos_agent::tools::TOOL_APPROVAL_CTX.scope(approver, async move {
                registry
                    .execute_with_context(&call_ctx("c2"), "news_share", &args)
                    .await
                    .unwrap()
            }),
        )
    };
    let pending = wait_for_pending(&contracts, &key).await;
    contracts
        .approvals
        .respond_with_context(ApprovalRespondParams::new(
            key.clone(),
            pending[0].approval_id.clone(),
            ApprovalDecision::Approve,
        ))
        .unwrap();
    assert!(run.await.unwrap().success);

    drop(ws);
    crate::peers::host_tools::set_host_route(&peers_root(&fx), "news", Arc::new(|_, _| false));
    let calls = host.await.unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["context_id"], "ui-1");
    assert_eq!(calls[0]["confirm_required"], true);
    assert_eq!(calls[1]["context_id"], Value::Null);
    assert_eq!(calls[1]["confirm_required"], false);
    let decisions: Vec<Value> = audit_rows(&fx)
        .iter()
        .map(|r| r["decision"].clone())
        .collect();
    assert_eq!(decisions, [json!("app_confirms"), json!("approved")]);
}
