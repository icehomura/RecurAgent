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
    // The host's own turn: driven by the connection that registered the set.
    let host = crate::peers::host_tools::host_route_connection(&peers_root(fx), "news");
    apply_session_host_tools(&mut registry, &resolved, &peers_root(fx), key, turn, host);
    registry
}

/// The registry of a turn on `key` driven by `connection`.
async fn turn_registry_on(
    fx: &Fx,
    key: &SessionKey,
    turn: &str,
    connection: u64,
) -> octos_agent::ToolRegistry {
    let runtime = crate::runtime::SessionRuntime::bootstrap(&fx.runtime, key.clone(), None)
        .await
        .expect("bound session");
    let mut registry = runtime.tools.snapshot_excluding(&[]);
    let resolved = resolve_session_host_tools(&peers_root(fx), key);
    apply_session_host_tools(
        &mut registry,
        &resolved,
        &peers_root(fx),
        key,
        turn,
        Some(connection),
    );
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
    // Answered on the connection that holds the route (the one the call
    // was sent to).
    let host = crate::peers::host_tools::host_route_connection(&peers_root(fx), "news")
        .unwrap_or_default();
    raw_peer_tool_result(
        host,
        &fx.state,
        &rpc(APPUI_METHOD_PEER_TOOL_RESULT, params),
        None,
    )
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
    let peers_root = peers_root(fx);
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
            let host = crate::peers::host_tools::host_route_connection(&peers_root, "news")
                .unwrap_or_default();
            raw_peer_tool_result(
                host,
                &state,
                &rpc(APPUI_METHOD_PEER_TOOL_RESULT, body),
                None,
            )
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
        json!({ "tools": [news_list()], "generic_tools": ["read_file"] }),
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
    crate::peers::host_tools::set_host_route(&peers_root(&fx), "news", 0, Arc::new(|_, _| false));
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

    // A late answer is refused and audited.
    let late = answer(
        &fx,
        &token,
        call["call_id"].as_str().unwrap(),
        json!({"ok": true, "data": {}}),
    )
    .expect_err("late result");
    assert_eq!(late.data.unwrap()["kind"], "peer_tool_call_not_found");
    let rows = audit_rows(&fx);
    assert_eq!(rows.last().unwrap()["decision"], "late_result");
    assert_eq!(rows.last().unwrap()["call_id"], call["call_id"]);
    assert!(crate::peers::host_tools::pending_calls_for(&peers_root(&fx), "news").is_empty());

    // With no live host at all the call fails without waiting.
    crate::peers::host_tools::set_host_route(&peers_root(&fx), "news", 0, Arc::new(|_, _| false));
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

/// The approval bridge the serve turn installs for a `turn/start` on `key`:
/// approvals park on that session (a request context's is the app's own
/// conversation; the peer's own session is where the host surfaces the
/// person-absent approvals).
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
    crate::peers::host_tools::set_host_route(&peers_root(&fx), "news", 0, Arc::new(|_, _| false));
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
    assert!(!again.success && again.output.contains("not asked or sent twice"));
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
    crate::peers::host_tools::set_host_route(&peers_root(&fx), "news", 0, Arc::new(|_, _| false));
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

fn news_topics_set() -> Value {
    json!({
        "name": "news.topics_set",
        "description": "Replace the followed topics.",
        "input_schema": {"type": "object", "required": ["topics"]},
        "risk": "act",
        "background": true,
    })
}

#[tokio::test]
async fn should_report_an_unanswered_act_call_as_unknown_and_never_resend_it() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (ws, mut rx) = ws_connection_for_test(32);
    register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [news_topics_set()], "call_timeout_ms": 100 }),
    )
    .unwrap();
    let registry = turn_registry(&fx, &key, "turn-1").await;
    let args = json!({"topics": ["rust"]});
    let result = registry
        .execute_with_context(&call_ctx("c1"), "news_topics_set", &args)
        .await
        .unwrap();
    assert!(!result.success, "{}", result.output);
    assert!(
        result.output.contains("outcome_unknown") && result.output.contains("Do not retry"),
        "{}",
        result.output
    );
    let call = next_frame(&mut rx, "peer/tool/call").await;
    assert!(call["args_digest"].as_str().unwrap().starts_with("sha256:"));
    assert_eq!(
        next_frame(&mut rx, "peer/tool/cancel").await["reason"],
        "timeout"
    );

    // A re-dispatch of the same call never reaches the host again.
    let again = registry
        .execute_with_context(&call_ctx("c1"), "news_topics_set", &args)
        .await
        .unwrap();
    assert!(!again.success && again.output.contains("not asked or sent twice"));
    // Nor does a retry under a new tool-call id with the same arguments.
    let retry = registry
        .execute_with_context(&call_ctx("c2"), "news_topics_set", &args)
        .await
        .unwrap();
    assert!(
        !retry.success && retry.output.contains("not sent again"),
        "{}",
        retry.output
    );
    assert!(rx.try_recv().is_err(), "no second peer/tool/call");

    // A later turn of the same session does not resend it either.
    let later = turn_registry(&fx, &key, "turn-2").await;
    let again = later
        .execute_with_context(&call_ctx("c1"), "news_topics_set", &args)
        .await
        .unwrap();
    assert!(
        !again.success && again.output.contains("not sent again"),
        "{}",
        again.output
    );
    assert!(rx.try_recv().is_err(), "no second peer/tool/call");

    let rows = audit_rows(&fx);
    assert_eq!(rows[0]["outcome"], "unknown");
    assert_eq!(rows[1]["decision"], "duplicate");
}

#[tokio::test]
async fn should_treat_a_call_interrupted_while_the_host_worked_as_unknown() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (ws, mut rx) = ws_connection_for_test(32);
    register(&fx, &ws, &token, json!({ "tools": [news_topics_set()] })).unwrap();
    let registry = Arc::new(turn_registry(&fx, &key, "turn-1").await);
    let args = json!({"topics": ["rust"]});
    let task = {
        let registry = registry.clone();
        let args = args.clone();
        tokio::spawn(async move {
            registry
                .execute_with_context(&call_ctx("c1"), "news_topics_set", &args)
                .await
        })
    };
    next_frame(&mut rx, "peer/tool/call").await;
    // The turn is interrupted while the host is working on the call.
    task.abort();
    let _ = task.await;
    assert_eq!(
        next_frame(&mut rx, "peer/tool/cancel").await["reason"],
        "cancelled"
    );

    let later = turn_registry(&fx, &key, "turn-2").await;
    let again = later
        .execute_with_context(&call_ctx("c9"), "news_topics_set", &args)
        .await
        .unwrap();
    assert!(
        !again.success && again.output.contains("not sent again"),
        "{}",
        again.output
    );
    assert!(rx.try_recv().is_err(), "not sent to the host again");
}

#[tokio::test]
async fn should_give_no_tools_to_a_kernel_internal_continuation() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (ws, _rx) = ws_connection_for_test(8);
    register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [news_list()], "generic_tools": ["read_file"] }),
    )
    .unwrap();
    let runtime = crate::runtime::SessionRuntime::bootstrap(&fx.runtime, key.clone(), None)
        .await
        .unwrap();
    let mut registry = runtime.tools.snapshot_excluding(&[]);
    let resolved = resolve_session_host_tools(&peers_root(&fx), &key);
    // `run_standalone_turn` passes no connection for an internal continuation.
    apply_session_host_tools(&mut registry, &resolved, &peers_root(&fx), &key, "t", None);
    assert!(registry.tool_names().is_empty());
}

#[tokio::test]
async fn should_clamp_host_filesystem_access_for_a_bound_app_session() {
    let fx = fixture().await;
    prepare_news(&fx).await;
    let key = peer_key(&fx);
    let runtime = crate::runtime::SessionRuntime::bootstrap_with_permissions(
        &fx.runtime,
        key,
        None,
        octos_agent::EffectivePermissions::danger_full_access(),
    )
    .await
    .expect("bound session");
    assert!(
        !runtime.permissions.filesystem_scope.is_host(),
        "a bound app session never gets host filesystem access"
    );
    assert!(
        runtime.agent.session_scope().is_some(),
        "file tools are fenced"
    );

    // An ordinary session keeps the operator's grant.
    let plain = crate::runtime::SessionRuntime::bootstrap_with_permissions(
        &fx.runtime,
        SessionKey::with_profile_topic("dev", "api", "host", "plain"),
        None,
        octos_agent::EffectivePermissions::danger_full_access(),
    )
    .await
    .unwrap();
    assert!(plain.permissions.filesystem_scope.is_host());
}

fn rpc_error_kind(message: WsMessage) -> Value {
    frame_json(message)["error"]["data"]["kind"].clone()
}

#[tokio::test]
async fn should_keep_a_host_tool_approval_and_turn_controls_on_the_host_connection() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (host_ws, host_rx) = ws_connection_for_test(64);
    register(&fx, &host_ws, &token, json!({ "tools": [mail_send()] })).unwrap();
    let host = spawn_fake_host(
        &fx,
        token.clone(),
        host_rx,
        |_| json!({ "ok": true, "data": {} }),
    );
    let registry = Arc::new(turn_registry(&fx, &key, "turn-1").await);
    let ledger = Arc::new(UiProtocolLedger::new(64));
    let contracts = Arc::new(UiProtocolContractStores::default());
    let turn = TurnId::new();
    // The host's own turn: its approval bridge writes to the shared ledger.
    let (bridge_ws, _bridge_rx) = ws_connection_for_test(64);
    let approver: Arc<dyn octos_agent::ToolApprovalRequester> =
        Arc::new(UiProtocolApprovalRequester {
            ws: bridge_ws,
            ledger: ledger.clone(),
            contracts: contracts.clone(),
            state: fx.state.clone(),
            peers_root: peers_root(&fx),
            session_id: key.clone(),
            turn_id: turn.clone(),
            features: ConnectionUiFeatures::default(),
        });
    let run = {
        let registry = registry.clone();
        tokio::spawn(
            octos_agent::tools::TOOL_APPROVAL_CTX.scope(approver, async move {
                registry
                    .execute_with_context(&call_ctx("c1"), "mail_send", &json!({"draft_id": "d"}))
                    .await
                    .unwrap()
            }),
        )
    };
    let pending = wait_for_pending(&contracts, &key).await;
    let approval_id = pending[0].approval_id.clone();

    // Another connection of the profile, on the same session.
    let (spoof_ws, mut spoof_rx) = ws_connection_for_test(64);
    let mut requested = None;
    for _ in 0..200 {
        requested = ledger
            .replay_after(
                &key,
                Some(&UiCursor {
                    stream: key.0.clone(),
                    seq: 0,
                }),
            )
            .unwrap()
            .into_iter()
            .find(|e| {
                matches!(
                    &e.event,
                    UiProtocolLedgerEvent::Notification(UiNotification::ApprovalRequested(_))
                )
            });
        if requested.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let requested = requested.expect("the approval is in the shared ledger");
    // Neither live forwarding nor replay shows it to that connection...
    assert!(!ledger_event_visible_to_connection(
        &requested.event,
        spoof_ws.connection_id
    ));
    assert!(ledger_event_visible_to_connection(
        &requested.event,
        host_ws.connection_id
    ));
    forward_live_ledger_event(
        &spoof_ws,
        &ledger,
        requested.clone(),
        0,
        spoof_ws.connection_id,
        ConnectionUiFeatures::default(),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(spoof_rx.try_recv().is_err(), "not forwarded live");

    // ...and it cannot answer it.
    let respond = |approval_id: ApprovalId| {
        ApprovalRespondParams::new(key.clone(), approval_id, ApprovalDecision::Approve)
    };
    handle_approval_respond(
        &spoof_ws,
        &fx.state,
        &ledger,
        &contracts,
        None,
        None,
        "r1".into(),
        respond(approval_id.clone()),
    )
    .await;
    assert_eq!(
        rpc_error_kind(spoof_rx.recv().await.unwrap()),
        "peer_host_connection_only"
    );
    assert_eq!(contracts.approvals.pending_for_session(&key).len(), 1);

    // Nor steer or interrupt the host's turn.
    let active_turns: SharedActiveTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    handle_turn_interrupt(
        &spoof_ws,
        &ledger,
        &active_turns,
        &contracts,
        "i1".into(),
        TurnInterruptParams {
            session_id: key.clone(),
            turn_id: turn.clone(),
        },
    )
    .await;
    assert_eq!(
        rpc_error_kind(spoof_rx.recv().await.unwrap()),
        "peer_host_connection_only"
    );
    assert!(refuse_foreign_host_turn_control(&key, &spoof_ws, "turn/steer").is_some());
    assert!(refuse_foreign_host_turn_control(&key, &host_ws, "turn/steer").is_none());

    // The host connection answers it.
    handle_approval_respond(
        &host_ws,
        &fx.state,
        &ledger,
        &contracts,
        None,
        None,
        "r2".into(),
        respond(approval_id),
    )
    .await;
    assert!(run.await.unwrap().success);
    drop(host_ws);
    crate::peers::host_tools::set_host_route(&peers_root(&fx), "news", 0, Arc::new(|_, _| false));
    assert_eq!(host.await.unwrap().len(), 1);
}

#[tokio::test]
async fn should_wait_for_the_apps_confirmation_sheet_instead_of_timing_out() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let (ws, mut rx) = ws_connection_for_test(32);
    register(
        &fx,
        &ws,
        &token,
        json!({
            "tools": [{
                "name": "news.share",
                "description": "Share a story with a contact.",
                "input_schema": {"type": "object", "required": ["story"]},
                "risk": "destructive",
                "confirm": "app",
            }, mail_send()],
            "call_timeout_ms": 100,
            "approval_ttl_secs": 30,
        }),
    )
    .unwrap();
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
    let registry = Arc::new(turn_registry(&fx, &context_key, "turn-1").await);
    let contracts = Arc::new(UiProtocolContractStores::default());

    // confirm=app, person present: the sheet takes longer than the call
    // timeout; the kernel keeps waiting and sends no cancel.
    let run = {
        let registry = registry.clone();
        let approver = app_approver(&fx, &context_key, &contracts, &TurnId::new());
        tokio::spawn(
            octos_agent::tools::TOOL_APPROVAL_CTX.scope(approver, async move {
                registry
                    .execute_with_context(&call_ctx("c1"), "news_share", &json!({"story": "hn-1"}))
                    .await
                    .unwrap()
            }),
        )
    };
    let call = next_frame(&mut rx, "peer/tool/call").await;
    assert_eq!(call["confirm_required"], true);
    assert!(call["timeout_ms"].as_u64().unwrap() >= 30_000);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    answer(
        &fx,
        &token,
        call["call_id"].as_str().unwrap(),
        json!({"ok": true, "data": {"shared": 1}}),
    )
    .expect("the answer after the sheet is accepted");
    assert!(run.await.unwrap().success);

    // An approved confirm=host call: the host acknowledges it is still asking
    // the person, which extends the wait past the call timeout.
    let run = {
        let registry = registry.clone();
        let approver = app_approver(&fx, &context_key, &contracts, &TurnId::new());
        tokio::spawn(
            octos_agent::tools::TOOL_APPROVAL_CTX.scope(approver, async move {
                registry
                    .execute_with_context(&call_ctx("c2"), "mail_send", &json!({"draft_id": "d-1"}))
                    .await
                    .unwrap()
            }),
        )
    };
    let pending = wait_for_pending(&contracts, &context_key).await;
    contracts
        .approvals
        .respond_with_context(ApprovalRespondParams::new(
            context_key.clone(),
            pending[0].approval_id.clone(),
            ApprovalDecision::Approve,
        ))
        .unwrap();
    let call = next_frame(&mut rx, "peer/tool/call").await;
    let call_id = call["call_id"].as_str().unwrap().to_owned();
    let ack = answer(
        &fx,
        &token,
        &call_id,
        json!({"status": "awaiting_confirmation"}),
    )
    .unwrap();
    assert_eq!(ack["awaiting_confirmation"], true);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    answer(
        &fx,
        &token,
        &call_id,
        json!({"ok": true, "data": {"sent": true}}),
    )
    .unwrap();
    assert!(run.await.unwrap().success);
    while let Ok(frame) = rx.try_recv() {
        assert_ne!(frame_json(frame)["method"], "peer/tool/cancel");
    }
}

#[tokio::test]
async fn should_never_answer_or_remember_a_host_tool_approval_by_scope() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (ws, rx) = ws_connection_for_test(32);
    register(&fx, &ws, &token, json!({ "tools": [mail_send()] })).unwrap();
    let host = spawn_fake_host(
        &fx,
        token.clone(),
        rx,
        |_| json!({ "ok": true, "data": {} }),
    );
    let registry = Arc::new(turn_registry(&fx, &key, "turn-1").await);
    let contracts = Arc::new(UiProtocolContractStores::default());
    let turn = TurnId::new();

    // A remembered session-wide scope for this tool exists already...
    contracts.scopes.record(
        &key,
        ApprovalScopeKind::from_scope_str("approve_for_session"),
        match_key_for(
            ApprovalScopeKind::from_scope_str("approve_for_session"),
            "mail_send",
            &turn,
        ),
        ApprovalDecision::Approve,
    );
    // ...yet the call still asks the person.
    let run = {
        let registry = registry.clone();
        let approver = app_approver(&fx, &key, &contracts, &turn);
        tokio::spawn(
            octos_agent::tools::TOOL_APPROVAL_CTX.scope(approver, async move {
                registry
                    .execute_with_context(&call_ctx("c1"), "mail_send", &json!({"draft_id": "d-1"}))
                    .await
                    .unwrap()
            }),
        )
    };
    let pending = wait_for_pending(&contracts, &key).await;

    // Answering with a scope decides this call only and records no scope.
    let mut params = ApprovalRespondParams::new(
        key.clone(),
        pending[0].approval_id.clone(),
        ApprovalDecision::Approve,
    );
    params.approval_scope = Some("approve_for_tool".into());
    let fresh = UiProtocolContractStores::default();
    let outcome = contracts
        .approvals
        .respond_with_context(params.clone())
        .unwrap();
    assert!(outcome.context.as_ref().unwrap().once_only);
    assert!(!record_approval_scope(
        &fresh,
        &key,
        params.approval_scope.as_deref(),
        outcome.context.as_ref(),
        ApprovalDecision::Approve,
    ));
    assert!(
        fresh
            .scopes
            .lookup(&key, "mail_send", &TurnId::new())
            .is_none()
    );
    assert!(run.await.unwrap().success);

    // An ordinary approval still records its scope.
    let ordinary = crate::contracts::approvals::RespondedApprovalContext {
        tool_name: "shell".into(),
        turn_id: TurnId::new(),
        once_only: false,
    };
    assert!(record_approval_scope(
        &fresh,
        &key,
        Some("approve_for_tool"),
        Some(&ordinary),
        ApprovalDecision::Approve,
    ));

    drop(ws);
    crate::peers::host_tools::set_host_route(&peers_root(&fx), "news", 0, Arc::new(|_, _| false));
    assert_eq!(host.await.unwrap().len(), 1);
}

#[tokio::test]
async fn should_give_no_tools_to_a_session_on_a_foreign_base_key() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let (ws, _rx) = ws_connection_for_test(8);
    register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [news_list(), mail_send()], "generic_tools": ["read_file"] }),
    )
    .unwrap();
    let _ = raw_peer_context_open(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_CONTEXT_OPEN,
            json!({"session_id": fx.system, "peer": "news", "context_id": "ui-1",
                   "host_token": token}),
        ),
        None,
    )
    .unwrap();
    let foreign_base = SessionKey::with_profile_topic("dev", "api", "intruder", "x");
    for topic in ["peerctx-news.ui-1", "peer-news"] {
        let key = SessionKey(format!("{}#{topic}", foreign_base.base_key()));
        let resolved = resolve_session_host_tools(&peers_root(&fx), &key);
        assert!(
            matches!(
                resolved,
                crate::peers::host_tools::SessionHostTools::FailClosed { .. }
            ),
            "{topic}: {resolved:?}"
        );
        let mut registry = fx.runtime.tool_specs.snapshot_excluding(&[]);
        assert!(!registry.tool_names().is_empty());
        apply_session_host_tools(
            &mut registry,
            &resolved,
            &peers_root(&fx),
            &key,
            "t",
            Some(ws.connection_id.0),
        );
        assert!(registry.tool_names().is_empty(), "{topic}");
    }
    // The owner's own sessions still get the set.
    let own = SessionKey(format!("{}#peerctx-news.ui-1", fx.system.base_key()));
    assert!(matches!(
        resolve_session_host_tools(&peers_root(&fx), &own),
        crate::peers::host_tools::SessionHostTools::Enforced { .. }
    ));
}

#[tokio::test]
async fn should_refuse_generic_tools_that_escape_the_set() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let (ws, _rx) = ws_connection_for_test(8);
    for escape in [
        "spawn",
        "spawn_agent",
        "delegate",
        "run_pipeline",
        "peer_handoff",
        "peer_send_input",
        "goal_dispatch",
        "manage_skills",
        "message",
        "send_file",
        "shell",
        "bash",
        "exec_command",
        "write_stdin",
        "git",
        "write_file",
        "edit_file",
        "diff_edit",
        "apply_patch",
        "browser",
        "web_fetch",
        "deep_crawl",
        "monitor_delete",
        "check_background_tasks",
        "read_task_output",
        "synthesize_research",
        "view_image",
        "recall",
        "no_such_tool",
    ] {
        let err = register(
            &fx,
            &ws,
            &token,
            json!({ "tools": [news_list()], "generic_tools": [escape] }),
        )
        .expect_err(escape);
        assert_eq!(err.data.unwrap()["kind"], "peer_tools_invalid", "{escape}");
    }
    let err = register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [{"name": "news.list", "description": "x",
                           "input_schema": {"type": "object",
                                            "properties": {"n": {"type": "whole"}}},
                           "risk": "read"}] }),
    )
    .expect_err("bad schema type");
    assert!(
        err.message.contains("not a JSON Schema type"),
        "{}",
        err.message
    );

    // Boolean subschemas and additionalProperties are honoured.
    register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [{"name": "news.list", "description": "x",
                           "input_schema": {"type": "object",
                                            "properties": {"any": true},
                                            "additionalProperties": false},
                           "risk": "read"}],
                "generic_tools": ["read_file", "deep_search", "memory_search"] }),
    )
    .expect("boolean subschemas are valid");
    let err = register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [{"name": "news.list", "description": "x",
                           "input_schema": {"type": "object",
                                            "additionalProperties": {"type": 7}},
                           "risk": "read"}] }),
    )
    .expect_err("bad additionalProperties");
    assert!(
        err.message.contains("additionalProperties"),
        "{}",
        err.message
    );

    // A stale set holding a now-forbidden name is still stripped per turn.
    let key = peer_key(&fx);
    let path = peers_root(&fx).join("news/host_tools.json");
    let mut stored: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    stored["generic_tools"] = json!(["shell", "read_file"]);
    std::fs::write(&path, stored.to_string()).unwrap();
    assert_eq!(
        sorted_names(&turn_registry(&fx, &key, "t").await),
        ["news_list", "read_file"]
    );
}

#[tokio::test]
async fn should_give_no_tools_to_another_connection_on_the_hosts_base_key() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let (host_ws, _host_rx) = ws_connection_for_test(32);
    register(
        &fx,
        &host_ws,
        &token,
        json!({ "tools": [news_list(), mail_send()] }),
    )
    .unwrap();
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

    // Another client of the profile opens the same topic on the host's base
    // key and drives a turn on its own connection.
    let (spoof_ws, mut spoof_rx) = ws_connection_for_test(32);
    for key in [context_key.clone(), peer_key(&fx)] {
        let registry = turn_registry_on(&fx, &key, "turn-x", spoof_ws.connection_id.0).await;
        assert!(registry.tool_names().is_empty(), "{key:?}");

        // It cannot call the destructive tool, so no approval is ever raised
        // for it to receive or answer.
        let contracts = Arc::new(UiProtocolContractStores::default());
        let approver: Arc<dyn octos_agent::ToolApprovalRequester> =
            Arc::new(UiProtocolApprovalRequester {
                ws: spoof_ws.clone(),
                ledger: Arc::new(UiProtocolLedger::new(64)),
                contracts: contracts.clone(),
                state: fx.state.clone(),
                peers_root: peers_root(&fx),
                session_id: key.clone(),
                turn_id: TurnId::new(),
                features: ConnectionUiFeatures::default(),
            });
        let forced = octos_agent::tools::TOOL_APPROVAL_CTX
            .scope(
                approver,
                registry.execute_with_context(
                    &call_ctx("c1"),
                    "mail_send",
                    &json!({"draft_id": "d"}),
                ),
            )
            .await;
        assert!(forced.is_err(), "unknown tool");
        assert!(contracts.approvals.pending_for_session(&key).is_empty());
    }
    assert!(
        spoof_rx.try_recv().is_err(),
        "nothing reached the spoofing connection"
    );

    // The host's own connection still gets the set; once it closes, nobody does.
    let registry = turn_registry_on(&fx, &context_key, "turn-h", host_ws.connection_id.0).await;
    assert_eq!(sorted_names(&registry), ["mail_send", "news_list"]);
    crate::peers::host_tools::drop_routes_for_connection(host_ws.connection_id.0);
    let registry = turn_registry_on(&fx, &context_key, "turn-h2", host_ws.connection_id.0).await;
    assert!(registry.tool_names().is_empty());
}

#[tokio::test]
async fn should_refuse_an_awaiting_confirmation_ack_for_a_call_that_is_not_gated() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (ws, mut rx) = ws_connection_for_test(32);
    register(&fx, &ws, &token, json!({ "tools": [news_topics_set()] })).unwrap();
    let registry = Arc::new(turn_registry(&fx, &key, "turn-1").await);
    let run = {
        let registry = registry.clone();
        tokio::spawn(async move {
            registry
                .execute_with_context(&call_ctx("c1"), "news_topics_set", &json!({"topics": []}))
                .await
                .unwrap()
        })
    };
    let call = next_frame(&mut rx, "peer/tool/call").await;
    let call_id = call["call_id"].as_str().unwrap().to_owned();
    let err = answer(
        &fx,
        &token,
        &call_id,
        json!({"status": "awaiting_confirmation"}),
    )
    .expect_err("an act call is not acknowledged");
    assert_eq!(err.data.unwrap()["kind"], "peer_tool_ack_not_gated");
    answer(&fx, &token, &call_id, json!({"ok": true, "data": {}})).unwrap();
    assert!(run.await.unwrap().success);
}

/// Round-4 review regression: a runtime cached for `<host base>#peer-news`
/// BEFORE `peer/prepare` bound the topic must never be reused afterwards —
/// it carries the profile's memory, the profile workspace and unclamped
/// host filesystem access.
#[tokio::test]
async fn should_rebuild_a_session_runtime_cached_before_the_peer_was_bound() {
    let fx = fixture().await;
    let key = peer_key(&fx);
    let cache = &fx.state.session_cache;
    let epoch = cache.session_generation(&key);
    let stale = cache
        .get_or_init_with_permissions(
            &fx.runtime,
            key.clone(),
            None,
            octos_agent::EffectivePermissions::danger_full_access(),
            epoch,
        )
        .await
        .expect("an ordinary session before the peer exists");
    assert!(stale.permissions.filesystem_scope.is_host());
    assert!(stale.memory.namespace.is_none());

    let token = prepare_news(&fx).await;
    let (ws, _rx) = ws_connection_for_test(8);
    register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [news_list()], "generic_tools": ["read_file", "memory_search"] }),
    )
    .unwrap();

    let epoch = cache.session_generation(&key);
    let fresh = cache
        .get_or_init_with_permissions(
            &fx.runtime,
            key.clone(),
            None,
            octos_agent::EffectivePermissions::danger_full_access(),
            epoch,
        )
        .await
        .expect("the bound session");
    assert!(
        !Arc::ptr_eq(&stale, &fresh),
        "the stale runtime is not reused"
    );
    assert!(!fresh.permissions.filesystem_scope.is_host(), "clamped");
    assert!(fresh.agent.session_scope().is_some(), "scoped");
    assert_eq!(fresh.memory.namespace.as_deref(), Some("app/news/acct-1"));
    assert_eq!(
        dunce::canonicalize(&fresh.workspace_root).unwrap(),
        dunce::canonicalize(fx.apps.join("news")).unwrap()
    );

    // The host's turn roster is built on the rebuilt runtime.
    let mut registry = fresh.tools.snapshot_excluding(&[]);
    let resolved = resolve_session_host_tools(&peers_root(&fx), &key);
    apply_session_host_tools(
        &mut registry,
        &resolved,
        &peers_root(&fx),
        &key,
        "t",
        Some(ws.connection_id.0),
    );
    assert_eq!(
        sorted_names(&registry),
        ["memory_search", "news_list", "read_file"]
    );

    // A context topic cached before `peer/context/open` is dropped by the
    // bind-time invalidation as well.
    let ctx_key = SessionKey(format!("{}#peerctx-news.ui-9", fx.system.base_key()));
    let epoch = cache.session_generation(&ctx_key);
    // Never opened: it cannot even bootstrap.
    assert!(
        cache
            .get_or_init_with_permissions(
                &fx.runtime,
                ctx_key.clone(),
                None,
                octos_agent::EffectivePermissions::workspace_write(),
                epoch,
            )
            .await
            .is_err()
    );
    invalidate_bound_topics(&fx.state, std::iter::once(&json!({"topic": "peer-news"}))).await;
    let epoch = cache.session_generation(&key);
    let again = cache
        .get_or_init_with_permissions(
            &fx.runtime,
            key.clone(),
            None,
            octos_agent::EffectivePermissions::workspace_write(),
            epoch,
        )
        .await
        .unwrap();
    assert!(
        !Arc::ptr_eq(&fresh, &again),
        "bind-time invalidation drops the entry"
    );
}

// ---------------------------------------------------------------------------
// End to end through `turn/start` → `run_standalone_turn`
// ---------------------------------------------------------------------------

/// A scripted model: on its first call it asks for `tool` with `args`, then
/// ends the turn. Records the tool roster it was offered on every call and
/// the tool result it was given.
struct ScriptedHostToolLlm {
    tool: String,
    args: Value,
    calls: std::sync::atomic::AtomicUsize,
    rosters: std::sync::Mutex<Vec<Vec<String>>>,
    tool_results: std::sync::Mutex<Vec<String>>,
}

impl ScriptedHostToolLlm {
    fn new(tool: &str, args: Value) -> Arc<Self> {
        Arc::new(Self {
            tool: tool.to_owned(),
            args,
            calls: Default::default(),
            rosters: Default::default(),
            tool_results: Default::default(),
        })
    }
}

#[async_trait::async_trait]
impl octos_llm::LlmProvider for ScriptedHostToolLlm {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> eyre::Result<octos_llm::ChatResponse> {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut roster: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
        roster.sort();
        self.rosters.lock().unwrap().push(roster);
        for message in messages {
            if message.role == MessageRole::Tool {
                self.tool_results
                    .lock()
                    .unwrap()
                    .push(message.content.clone());
            }
        }
        let usage = octos_llm::TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        };
        if call == 0 {
            Ok(octos_llm::ChatResponse {
                content: None,
                reasoning_content: None,
                tool_calls: vec![octos_core::ToolCall {
                    id: "call_1".into(),
                    name: self.tool.clone(),
                    arguments: self.args.clone(),
                    metadata: None,
                }],
                stop_reason: octos_llm::StopReason::ToolUse,
                usage,
                provider_index: None,
            })
        } else {
            Ok(octos_llm::ChatResponse {
                content: Some("done".into()),
                reasoning_content: None,
                tool_calls: Vec::new(),
                stop_reason: octos_llm::StopReason::EndTurn,
                usage,
                provider_index: None,
            })
        }
    }

    fn model_id(&self) -> &str {
        "scripted-host-tool"
    }

    fn provider_name(&self) -> &str {
        "stub"
    }
}

/// The e2e fixture: a profile runtime whose model is `llm`, a staged News
/// peer, and the host's connection with the tool set registered on it.
async fn e2e_fixture(llm: Arc<dyn octos_llm::LlmProvider>, tools: Value) -> E2e {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(data_dir.join("memory")).unwrap();
    // The profile's (the system agent's) private memory: no app turn, and no
    // foreign turn on an app session, may see it.
    std::fs::write(
        data_dir.join("memory/MEMORY.md"),
        "- PROFILE-PRIVATE-FACT: the owner's bank PIN hint\n",
    )
    .unwrap();
    let runtime = {
        let base = crate::runtime::ProfileRuntime::bootstrap(
            &crate::profiles::UserProfile {
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
                                api_key_env: Some("HOST_TOOLS_E2E_KEY".to_string()),
                                api_type: None,
                            }),
                            ..Default::default()
                        }),
                        fallbacks: Vec::new(),
                    }),
                    env_vars: [("HOST_TOOLS_E2E_KEY".to_string(), "k".to_string())]
                        .into_iter()
                        .collect(),
                    ..Default::default()
                },
                created_at: Utc::now(),
                updated_at: Utc::now(),
            },
            &data_dir,
            None,
            crate::runtime::BootstrapRole::Serve,
        )
        .await
        .expect("bootstrap");
        // Swap in the scripted model.
        let Ok(mut runtime) = Arc::try_unwrap(base) else {
            panic!("the freshly bootstrapped runtime has one owner");
        };
        runtime.llm = llm.clone();
        Arc::new(runtime)
    };
    let mut state = AppState::empty_for_tests();
    state.profiles.insert("dev".to_string(), runtime.clone());
    state.sessions = Some(Arc::new(tokio::sync::Mutex::new(
        octos_bus::SessionManager::open(&data_dir).unwrap(),
    )));
    let state = Arc::new(state);
    let apps = tmp.path().join("apps");
    std::fs::create_dir_all(apps.join("news")).unwrap();
    let system = SessionKey::with_profile_topic("dev", "api", "host", "system");
    let prepared = raw_peer_prepare(
        &state,
        &rpc(
            APPUI_METHOD_PEER_PREPARE,
            json!({
                "brief": "You are the News app's assistant.",
                "names": ["News"],
                "cwd": apps.join("news").to_string_lossy(),
                "session_id": system,
                "memory_namespace": "app/news/acct-1",
                "resume": true,
            }),
        ),
        None,
    )
    .await
    .expect("prepare");
    let token = prepared["host_token"].as_str().unwrap().to_owned();
    let (ws, rx) = ws_connection_for_test(256);
    let mut params = json!({"session_id": system, "peer": "news", "host_token": token});
    for (key, value) in tools.as_object().unwrap() {
        params[key] = value.clone();
    }
    raw_peer_tools_register(
        &ws,
        &state,
        &rpc(APPUI_METHOD_PEER_TOOLS_REGISTER, params),
        None,
    )
    .expect("register");
    E2e {
        _tmp: tmp,
        state,
        data_dir,
        system,
        token,
        ws,
        rx: Some(rx),
    }
}

struct E2e {
    _tmp: tempfile::TempDir,
    state: Arc<AppState>,
    data_dir: PathBuf,
    system: SessionKey,
    token: String,
    ws: WsConnection,
    rx: Option<mpsc::Receiver<WsMessage>>,
}

/// Drive one `turn/start` on `session` from the host connection. The fake
/// host answers `peer/tool/call` with `{"items": ["hn-1"]}` and approves any
/// `approval/requested`. Returns the tool calls the host saw and the
/// approvals it was asked.
async fn e2e_turn(
    e: &mut E2e,
    session: &SessionKey,
    llm: &ScriptedHostToolLlm,
) -> (Vec<Value>, Vec<Value>) {
    let ledger = Arc::new(UiProtocolLedger::new(256));
    let contracts = Arc::new(UiProtocolContractStores::default());
    let active_turns: SharedActiveTurns = Arc::new(TokioMutex::new(HashMap::new()));
    let connection_turns: SharedConnectionTurns = Arc::new(TokioMutex::new(HashMap::new()));
    let mut rx = e.rx.take().unwrap();
    let state = e.state.clone();
    let system = e.system.clone();
    let token = e.token.clone();
    let connection = e.ws.connection_id.0;
    let host_contracts = contracts.clone();
    let host_session = session.clone();
    let host = tokio::spawn(async move {
        let mut calls = Vec::new();
        let mut approvals = Vec::new();
        while let Some(message) = rx.recv().await {
            let WsMessage::Text(text) = message else {
                continue;
            };
            let frame: Value = serde_json::from_str(text.as_str()).unwrap();
            match frame["method"].as_str() {
                Some("peer/tool/call") => {
                    let params = frame["params"].clone();
                    raw_peer_tool_result(
                        connection,
                        &state,
                        &rpc(
                            APPUI_METHOD_PEER_TOOL_RESULT,
                            json!({
                                "session_id": system, "peer": "news", "host_token": token,
                                "call_id": params["call_id"], "ok": true,
                                "data": {"items": ["hn-1"]},
                            }),
                        ),
                        None,
                    )
                    .expect("result accepted");
                    calls.push(params);
                }
                Some("approval/requested") => {
                    let params = frame["params"].clone();
                    let approval_id: ApprovalId =
                        serde_json::from_value(params["approval_id"].clone()).unwrap();
                    host_contracts
                        .approvals
                        .respond_with_context(ApprovalRespondParams::new(
                            host_session.clone(),
                            approval_id,
                            ApprovalDecision::Approve,
                        ))
                        .expect("the person approves in the app");
                    approvals.push(params);
                }
                _ => {}
            }
            if frame["method"] == "__stop__" {
                break;
            }
        }
        (calls, approvals)
    });
    handle_turn_start(
        &e.ws,
        &e.state,
        &ledger,
        &contracts,
        &active_turns,
        &connection_turns,
        None,
        None,
        ConnectionUiFeatures::stdio_defaults(),
        "start-1".into(),
        TurnStartParams {
            session_id: session.clone(),
            turn_id: TurnId::new(),
            input: vec![InputItem::Text {
                text: "what's new?".into(),
            }],
            media: Vec::new(),
            topic: None,
            rewrite_for: None,
            reasoning_effort: None,
            tool_context: None,
            live_video: false,
        },
    )
    .await;
    for _ in 0..500 {
        if llm.calls.load(std::sync::atomic::Ordering::SeqCst) >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        llm.calls.load(std::sync::atomic::Ordering::SeqCst) >= 2,
        "the turn ran to its second model call"
    );
    // Let the turn finish, then stop the fake host.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let _ = send_raw_notification_ephemeral(&e.ws, "__stop__", json!({}));
    tokio::time::timeout(std::time::Duration::from_secs(5), host)
        .await
        .expect("fake host stops")
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_route_a_real_turns_app_tool_call_to_the_host_end_to_end() {
    let llm = ScriptedHostToolLlm::new("news_list", json!({"topic": "rust"}));
    let mut e = e2e_fixture(
        llm.clone(),
        json!({ "tools": [news_list()], "generic_tools": ["read_file"] }),
    )
    .await;
    let session = SessionKey(format!("{}#peer-news", e.system.base_key()));
    let (calls, approvals) = e2e_turn(&mut e, &session, &llm).await;

    // The model was offered exactly the registered set.
    assert_eq!(llm.rosters.lock().unwrap()[0], ["news_list", "read_file"]);
    // Its tool call reached the host, and the host's data reached the model.
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["name"], "news.list");
    assert_eq!(calls[0]["args"], json!({"topic": "rust"}));
    assert_eq!(calls[0]["session_id"], json!(session));
    assert!(approvals.is_empty(), "a read tool needs no approval");
    assert!(
        llm.tool_results
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.contains("hn-1")),
        "the host's result is the tool result"
    );
    let audit = std::fs::read_to_string(e.data_dir.join("peers/news/tool_audit.jsonl")).unwrap();
    assert!(audit.contains("\"decision\":\"allowed\"") && audit.contains("\"outcome\":\"ok\""));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_ask_the_person_before_a_real_turns_destructive_call_end_to_end() {
    let llm = ScriptedHostToolLlm::new("mail_send", json!({"draft_id": "d-1"}));
    let mut e = e2e_fixture(llm.clone(), json!({ "tools": [mail_send()] })).await;
    let session = SessionKey(format!("{}#peer-news", e.system.base_key()));
    let (calls, approvals) = e2e_turn(&mut e, &session, &llm).await;

    assert_eq!(llm.rosters.lock().unwrap()[0], ["mail_send"]);
    assert_eq!(approvals.len(), 1, "one approval, on the host connection");
    assert!(
        approvals[0]["body"]
            .as_str()
            .unwrap_or_default()
            .contains("d-1")
            || approvals[0].to_string().contains("d-1"),
        "the approval carries the exact arguments: {}",
        approvals[0]
    );
    assert_eq!(calls.len(), 1, "the approved call reached the host");
    assert_eq!(calls[0]["confirm_required"], false);
}

#[tokio::test]
async fn should_refuse_to_fork_an_app_peer_session() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
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
    // Even the host's own connection cannot fork: a fork drops the topic and
    // with it the binding, so there is no binding-preserving fork.
    let (host_ws, mut rx) = ws_connection_for_test(8);
    register(&fx, &host_ws, &token, json!({ "tools": [news_list()] })).unwrap();
    for session in [peer_key(&fx), context_key] {
        handle_session_fork(
            &host_ws,
            &fx.state,
            None,
            None,
            "f1".into(),
            octos_core::ui_protocol::SessionForkParams {
                session_id: session.clone(),
                new_chat_id: "copy".into(),
                copy_messages: None,
            },
        )
        .await;
        let frame = frame_json(rx.recv().await.unwrap());
        assert_eq!(
            frame["error"]["data"]["kind"], "app_peer_fork_refused",
            "{session:?}"
        );
    }
    // An ordinary session is not affected by the refusal (it proceeds to the
    // ordinary checks).
    handle_session_fork(
        &host_ws,
        &fx.state,
        None,
        None,
        "f2".into(),
        octos_core::ui_protocol::SessionForkParams {
            session_id: SessionKey::with_profile_topic("dev", "api", "host", "notes"),
            new_chat_id: "copy".into(),
            copy_messages: None,
        },
    )
    .await;
    let frame = frame_json(rx.recv().await.unwrap());
    assert_ne!(frame["error"]["data"]["kind"], "app_peer_fork_refused");
}

#[tokio::test]
async fn should_accept_a_tool_result_only_from_the_connection_the_call_was_sent_to() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (ws, mut rx) = ws_connection_for_test(32);
    register(&fx, &ws, &token, json!({ "tools": [news_list()] })).unwrap();
    let registry = Arc::new(turn_registry(&fx, &key, "turn-1").await);
    let run = {
        let registry = registry.clone();
        tokio::spawn(async move {
            registry
                .execute_with_context(&call_ctx("c1"), "news_list", &json!({}))
                .await
                .unwrap()
        })
    };
    let call = next_frame(&mut rx, "peer/tool/call").await;
    let body = json!({
        "session_id": fx.system, "peer": "news", "host_token": token,
        "call_id": call["call_id"], "ok": true, "data": {"items": ["forged"]},
    });
    // Another connection holding the token cannot answer the call.
    let (other, _other_rx) = ws_connection_for_test(8);
    let err = raw_peer_tool_result(
        other.connection_id.0,
        &fx.state,
        &rpc(APPUI_METHOD_PEER_TOOL_RESULT, body.clone()),
        None,
    )
    .expect_err("wrong connection");
    assert_eq!(
        err.data.unwrap()["kind"],
        "peer_tool_result_wrong_connection"
    );
    // The connection it was sent to can.
    let mut good = body;
    good["data"] = json!({"items": ["hn-1"]});
    raw_peer_tool_result(
        ws.connection_id.0,
        &fx.state,
        &rpc(APPUI_METHOD_PEER_TOOL_RESULT, good),
        None,
    )
    .unwrap();
    let result = run.await.unwrap();
    assert!(result.output.contains("hn-1") && !result.output.contains("forged"));
}

#[tokio::test]
async fn should_charge_a_request_contexts_turns_and_tools_to_its_peers_budget() {
    let fx = fixture().await;
    let prepared = raw_peer_prepare(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_PREPARE,
            json!({
                "brief": "News.", "names": ["News"],
                "cwd": fx.apps.join("news").to_string_lossy(),
                "session_id": fx.system, "memory_namespace": "app/news/acct-1",
                "resume": true, "token_budget": 100,
            }),
        ),
        None,
    )
    .await
    .expect("prepare with a budget");
    let token = prepared["host_token"].as_str().unwrap().to_owned();
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
    assert_eq!(crate::peers::budget_peer_slug(&context_key), Some("news"));

    // A finished context turn is charged to the peer, not skipped.
    write_peer_result_if_peer_session(
        &fx.state,
        &context_key,
        &TurnId::new(),
        octos_core::ui_protocol::TurnTerminalOutcome::Completed,
        "done",
        150,
        None,
    );
    let status = crate::peers::peer_token_budget_status(&peers_root(&fx), "news")
        .unwrap()
        .expect("budgeted");
    assert_eq!(status.used, 150);
    assert!(
        !peers_root(&fx).join("news/result.md").exists(),
        "a context writes no peer blackboard result"
    );

    // With the budget spent, app tool calls stop, from any session.
    let (ws, _rx) = ws_connection_for_test(8);
    register(&fx, &ws, &token, json!({ "tools": [news_list()] })).unwrap();
    let registry = turn_registry(&fx, &context_key, "turn-2").await;
    let result = registry
        .execute_with_context(&call_ctx("c1"), "news_list", &json!({}))
        .await
        .unwrap();
    assert!(
        result.output.contains("budget_exhausted"),
        "{}",
        result.output
    );
}

/// Records every request's full text (system prompt and messages) and ends
/// the turn at once.
#[derive(Default)]
struct RecordingLlm {
    requests: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl octos_llm::LlmProvider for RecordingLlm {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> eyre::Result<octos_llm::ChatResponse> {
        let text = messages
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n---\n");
        self.requests.lock().unwrap().push(text);
        Ok(octos_llm::ChatResponse {
            content: Some("ok".into()),
            reasoning_content: None,
            tool_calls: Vec::new(),
            stop_reason: octos_llm::StopReason::EndTurn,
            usage: octos_llm::TokenUsage {
                input_tokens: 1,
                output_tokens: 1,
                ..Default::default()
            },
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "recording"
    }

    fn provider_name(&self) -> &str {
        "stub"
    }
}

/// Run one turn on `session` driven by `ws` (or, with `continuation`, as a
/// kernel-internal continuation) and return the request text the model got.
async fn recorded_turn(
    e: &E2e,
    llm: &RecordingLlm,
    ws: &WsConnection,
    session: &SessionKey,
    continuation: bool,
) -> String {
    let before = llm.requests.lock().unwrap().len();
    let params = TurnStartParams {
        session_id: session.clone(),
        turn_id: TurnId::new(),
        input: vec![InputItem::Text {
            text: "what do you know about me?".into(),
        }],
        media: Vec::new(),
        topic: None,
        rewrite_for: None,
        reasoning_effort: None,
        tool_context: None,
        live_video: false,
    };
    let ledger = Arc::new(UiProtocolLedger::new(256));
    let contracts = Arc::new(UiProtocolContractStores::default());
    // Held until the turn has reached the model: dropping the active-turn
    // entry closes its interrupt channel.
    let active_turns: SharedActiveTurns = Arc::new(TokioMutex::new(HashMap::new()));
    let connection_turns: SharedConnectionTurns = Arc::new(TokioMutex::new(HashMap::new()));
    if continuation {
        let (_interrupt_tx, interrupt_rx) = mpsc::channel(1);
        run_standalone_turn(
            ws.clone(),
            e.state.clone(),
            ledger,
            contracts,
            ConnectionUiFeatures::stdio_defaults(),
            params,
            "what do you know about me?".into(),
            None,
            None,
            Arc::new(TokioMutex::new(TurnState::Active)),
            interrupt_rx,
            None,
            true,
            None,
            None,
            None,
            false,
            None,
            false,
            None,
        )
        .await;
    } else {
        handle_turn_start(
            ws,
            &e.state,
            &ledger,
            &contracts,
            &active_turns,
            &connection_turns,
            None,
            None,
            ConnectionUiFeatures::stdio_defaults(),
            "start".into(),
            params,
        )
        .await;
    }
    for _ in 0..500 {
        if llm.requests.lock().unwrap().len() > before {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let requests = llm.requests.lock().unwrap();
    assert!(
        requests.len() > before,
        "the turn on {session:?} (continuation: {continuation}) reached the model"
    );
    requests[before].clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_give_app_memory_only_to_the_host_connections_turns() {
    let llm = Arc::new(RecordingLlm::default());
    let e = e2e_fixture(llm.clone(), json!({ "tools": [news_list()] })).await;
    let peer = SessionKey(format!("{}#peer-news", e.system.base_key()));
    let opened = raw_peer_context_open(
        &e.state,
        &rpc(
            APPUI_METHOD_PEER_CONTEXT_OPEN,
            json!({"session_id": e.system, "peer": "news", "context_id": "ui-1",
                   "host_token": e.token}),
        ),
        None,
    )
    .unwrap();
    let context: SessionKey = serde_json::from_value(opened["session_id"].clone()).unwrap();

    // The app's private memory, in the peer's and the context's namespaces.
    let dev = e.state.profiles.get("dev").unwrap().clone();
    for (key, fact) in [(&peer, "NEWS-PRIVATE-FACT"), (&context, "CTX-PRIVATE-FACT")] {
        let runtime = e
            .state
            .session_cache
            .get_or_init(&dev, key.clone(), None)
            .await
            .unwrap();
        runtime
            .memory
            .memory_store
            .write_long_term(&format!("- {fact}: follows rust news\n"))
            .await
            .unwrap();
    }

    // The host's own turns carry the app's memory (and never the profile's).
    let mut e = e;
    // Drain the host connection like a live client.
    let mut host_rx = e.rx.take().unwrap();
    tokio::spawn(async move { while host_rx.recv().await.is_some() {} });
    let host_peer = recorded_turn(&e, &llm, &e.ws, &peer, false).await;
    assert!(host_peer.contains("NEWS-PRIVATE-FACT"), "{host_peer}");
    assert!(!host_peer.contains("PROFILE-PRIVATE-FACT"));
    let host_ctx = recorded_turn(&e, &llm, &e.ws, &context, false).await;
    assert!(host_ctx.contains("CTX-PRIVATE-FACT"), "{host_ctx}");

    // Another connection of the profile, on the same sessions: no app memory
    // and no profile memory.
    let (foreign, mut foreign_rx) = ws_connection_for_test(256);
    tokio::spawn(async move { while foreign_rx.recv().await.is_some() {} });
    for session in [&peer, &context] {
        let text = recorded_turn(&e, &llm, &foreign, session, false).await;
        for fact in [
            "NEWS-PRIVATE-FACT",
            "CTX-PRIVATE-FACT",
            "PROFILE-PRIVATE-FACT",
        ] {
            assert!(!text.contains(fact), "{session:?} leaked {fact}: {text}");
        }
        assert!(text.contains("not the app's host"), "{text}");
    }

    // A kernel-internal continuation, even on the host's connection: none.
    let continuation = recorded_turn(&e, &llm, &e.ws, &peer, true).await;
    for fact in [
        "NEWS-PRIVATE-FACT",
        "CTX-PRIVATE-FACT",
        "PROFILE-PRIVATE-FACT",
    ] {
        assert!(!continuation.contains(fact), "continuation leaked {fact}");
    }
}
