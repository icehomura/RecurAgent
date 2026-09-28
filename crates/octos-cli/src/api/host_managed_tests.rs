//! `octos serve --host-managed` (UPCR-2026-036): the router driven over a real
//! loopback listener with connect info, as `serve` wires it.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::http::StatusCode;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use super::{HostManaged, MIN_TOKEN_LEN, wait_for_eof};
use crate::api::{AppState, build_router};

const HOST: &str = "host-token-0123456789abcdef0123456789abcdef";
const EXTERNAL: &str = "external-token-0123456789abcdef0123456789ab";
const WEB: &str = "https://web.example";

struct Server {
    addr: SocketAddr,
    state: Arc<AppState>,
    handle: tokio::task::JoinHandle<()>,
    _audit: tempfile::TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn serve(external: bool) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let host_managed = HostManaged::new(
        HOST.to_owned(),
        external.then(|| EXTERNAL.to_owned()),
        addr.port(),
    )
    .unwrap();
    let audit = tempfile::tempdir().unwrap();
    let state = Arc::new(AppState {
        admin_audit_store: Some(Arc::new(
            crate::admin_audit_store::AdminAuditStore::open(audit.path()).unwrap(),
        )),
        auth_token: Some(HOST.to_owned()),
        appui_allowed_origins: vec![WEB.to_owned()],
        host_managed: Some(Arc::new(host_managed)),
        ..AppState::empty_for_tests()
    });
    let app = build_router(state.clone());
    let handle = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });
    tokio::task::yield_now().await;
    Server {
        addr,
        state,
        handle,
        _audit: audit,
    }
}

async fn status(
    server: &Server,
    method: reqwest::Method,
    path: &str,
    token: Option<&str>,
) -> StatusCode {
    let mut request =
        reqwest::Client::new().request(method, format!("http://{}{path}", server.addr));
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    request.send().await.unwrap().status()
}

/// A raw HTTP/1.1 GET with an arbitrary `Host`, returning the status line.
async fn raw_get(addr: SocketAddr, path: &str, host: &str) -> String {
    let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    tcp.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    let mut response = String::new();
    tcp.read_to_string(&mut response).await.unwrap();
    response.lines().next().unwrap_or_default().to_owned()
}

async fn ws(
    addr: SocketAddr,
    configure: impl FnOnce(&mut axum::http::Request<()>),
    query: &str,
) -> Result<axum::http::Response<Option<Vec<u8>>>, u16> {
    let mut request = format!("ws://{addr}/api/ui-protocol/ws{query}")
        .into_client_request()
        .unwrap();
    configure(&mut request);
    match tokio_tungstenite::connect_async(request).await {
        Ok((_, response)) => Ok(response),
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            Err(response.status().as_u16())
        }
        Err(error) => panic!("unexpected WebSocket error: {error}"),
    }
}

fn bearer(token: &str) -> impl FnOnce(&mut axum::http::Request<()>) + '_ {
    move |request| {
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
    }
}

#[test]
fn should_refuse_weak_equal_or_unsafe_host_managed_tokens() {
    assert!(HostManaged::new("short".into(), None, 1).is_err());
    assert!(HostManaged::new(HOST.into(), Some(HOST.into()), 1).is_err());
    assert!(HostManaged::new(HOST.into(), Some(format!("{EXTERNAL} x")), 1).is_err());
    assert!(HostManaged::new(format!("{}\n", "a".repeat(MIN_TOKEN_LEN)), None, 1).is_err());
    assert!(HostManaged::new(HOST.into(), None, 0).is_err());
    let host_managed = HostManaged::new(HOST.into(), Some(EXTERNAL.into()), 1).unwrap();
    let debug = format!("{host_managed:?}");
    assert!(
        !debug.contains(HOST) && !debug.contains(EXTERNAL),
        "{debug}"
    );
}

#[test]
fn should_resolve_only_the_two_configured_tokens() {
    use crate::api::router::AuthIdentity;
    use crate::user_store::UserRole;
    let host_managed = HostManaged::new(HOST.into(), Some(EXTERNAL.into()), 1).unwrap();
    assert!(matches!(
        host_managed.resolve(HOST),
        Some(AuthIdentity::Admin)
    ));
    assert!(matches!(
        host_managed.resolve(EXTERNAL),
        Some(AuthIdentity::User { ref id, role: UserRole::User }) if id == "_main"
    ));
    assert!(host_managed.resolve("").is_none());
    assert!(host_managed.resolve(&HOST[..HOST.len() - 1]).is_none());
    let host_only = HostManaged::new(HOST.into(), None, 1).unwrap();
    assert!(host_only.resolve(EXTERNAL).is_none());
}

#[tokio::test]
async fn should_reach_admin_routes_only_with_the_host_token() {
    let server = serve(true).await;
    for (method, path) in [
        (reqwest::Method::GET, "/api/admin/overview"),
        (reqwest::Method::POST, "/api/admin/stop-all"),
        (reqwest::Method::POST, "/api/admin/token/rotate"),
        (reqwest::Method::GET, "/api/admin/token/status"),
        (reqwest::Method::POST, "/api/admin/host/pairing"),
    ] {
        assert_eq!(
            status(&server, method.clone(), path, Some(EXTERNAL)).await,
            StatusCode::UNAUTHORIZED,
            "external token on {path}"
        );
        assert_eq!(
            status(&server, method, path, None).await,
            StatusCode::UNAUTHORIZED,
            "no token on {path}"
        );
    }
    assert_eq!(
        status(
            &server,
            reqwest::Method::POST,
            "/api/admin/host/pairing",
            Some(HOST)
        )
        .await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn should_confine_the_external_token_to_the_ui_protocol_socket() {
    let server = serve(true).await;
    for path in [
        "/api/my/profile",
        "/api/files/list",
        "/metrics",
        "/api/events/harness",
    ] {
        let status = status(&server, reqwest::Method::GET, path, Some(EXTERNAL)).await;
        assert!(
            status == StatusCode::FORBIDDEN || status == StatusCode::UNAUTHORIZED,
            "external token on {path}: {status}"
        );
    }
    assert!(ws(server.addr, bearer(EXTERNAL), "").await.is_ok());
    assert!(ws(server.addr, bearer(HOST), "").await.is_ok());
    assert_eq!(ws(server.addr, |_| {}, "").await.unwrap_err(), 401);
    assert_eq!(
        ws(server.addr, bearer("not-a-token"), "")
            .await
            .unwrap_err(),
        401
    );
    // A loopback hop proves nothing on a host-managed server.
    let spoof = |request: &mut axum::http::Request<()>| {
        request
            .headers_mut()
            .insert("x-profile-id", "_main".parse().unwrap());
    };
    assert_eq!(ws(server.addr, spoof, "").await.unwrap_err(), 401);
    // The server stop is never offered here: the host owns the lifecycle.
    assert!(!crate::api::ui_protocol_transport::supports_server_shutdown(&server.state));
}

#[tokio::test]
async fn should_refuse_external_clients_when_the_host_enabled_none() {
    let server = serve(false).await;
    assert_eq!(
        ws(server.addr, bearer(EXTERNAL), "").await.unwrap_err(),
        401
    );
    assert!(ws(server.addr, bearer(HOST), "").await.is_ok());
    assert_eq!(
        status(
            &server,
            reqwest::Method::POST,
            "/api/admin/host/pairing",
            Some(HOST)
        )
        .await,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn should_reject_a_request_whose_host_header_does_not_name_the_listener() {
    let server = serve(true).await;
    let port = server.addr.port();
    for host in [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("LOCALHOST:{port}"),
        format!("[::1]:{port}"),
    ] {
        assert!(
            raw_get(server.addr, "/health", &host)
                .await
                .contains(" 200 "),
            "{host}"
        );
    }
    for host in [
        format!("rebind.example:{port}"),
        "127.0.0.1".to_owned(),
        format!("127.0.0.1:{}", port.wrapping_add(1)),
        format!("localhost.:{port}"),
    ] {
        assert!(
            raw_get(server.addr, "/health", &host)
                .await
                .contains(" 421 "),
            "{host}"
        );
    }
}

#[tokio::test]
async fn should_admit_only_configured_browser_origins() {
    let server = serve(true).await;
    let with_origin = |origin: &'static str| {
        move |request: &mut axum::http::Request<()>| {
            bearer(EXTERNAL)(request);
            request
                .headers_mut()
                .insert("origin", origin.parse().unwrap());
        }
    };
    assert!(ws(server.addr, with_origin(WEB), "").await.is_ok());
    for origin in [
        "http://localhost:5173",
        "http://localhost:3000",
        "https://app.ominix.io",
        "https://evil.example",
    ] {
        assert_eq!(
            ws(server.addr, with_origin(origin), "").await.unwrap_err(),
            403,
            "{origin}"
        );
    }
    let browser_without_origin = |request: &mut axum::http::Request<()>| {
        bearer(EXTERNAL)(request);
        request
            .headers_mut()
            .insert("sec-fetch-mode", "websocket".parse().unwrap());
    };
    assert_eq!(
        ws(server.addr, browser_without_origin, "")
            .await
            .unwrap_err(),
        403
    );
}

#[tokio::test]
async fn should_accept_a_bearer_subprotocol_without_echoing_it() {
    let server = serve(true).await;
    let offer = |request: &mut axum::http::Request<()>| {
        request.headers_mut().insert(
            "sec-websocket-protocol",
            format!("octos-ui, octos.bearer.{EXTERNAL}")
                .parse()
                .unwrap(),
        );
    };
    let response = ws(server.addr, offer, "").await.unwrap();
    assert_eq!(
        response.headers()["sec-websocket-protocol"],
        "octos-ui",
        "the server selects octos-ui and never echoes the bearer entry"
    );
    // `?token=` still works for older clients.
    assert!(
        ws(server.addr, |_| {}, &format!("?token={EXTERNAL}"))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn should_pair_only_while_the_host_enables_it_and_hand_out_the_external_token() {
    let server = serve(true).await;
    let base = format!("http://{}", server.addr);
    let client = reqwest::Client::new();
    // Off by default: no code was minted or printed at startup.
    assert_eq!(
        client
            .get(format!("{base}/pair/info"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let enabled: serde_json::Value = client
        .post(format!("{base}/api/admin/host/pairing"))
        .bearer_auth(HOST)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let code = enabled["code"].as_str().unwrap().to_owned();
    assert_eq!(code.len(), 8);
    assert_eq!(enabled["expires_in_secs"], 300);
    assert!(!enabled.to_string().contains(EXTERNAL));
    let info = client
        .get(format!("{base}/pair/info"))
        .send()
        .await
        .unwrap();
    assert_eq!(info.status(), StatusCode::OK);
    let claim = || async {
        client
            .post(format!("{base}/pair/claim"))
            .header("content-type", "application/json")
            .body(serde_json::json!({ "code": code }).to_string())
            .send()
            .await
            .unwrap()
    };
    let claimed: serde_json::Value = claim().await.json().await.unwrap();
    assert_eq!(
        claimed["token"], EXTERNAL,
        "pairing hands out the external token"
    );
    assert_ne!(claimed["token"], HOST);
    assert_eq!(
        claim().await.status(),
        StatusCode::BAD_REQUEST,
        "a code is single use"
    );
    assert_eq!(
        client
            .delete(format!("{base}/api/admin/host/pairing"))
            .bearer_auth(HOST)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        client
            .get(format!("{base}/pair/info"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[test]
fn should_stop_waiting_at_end_of_input() {
    wait_for_eof(std::io::Cursor::new(b"ignored bytes".to_vec()));
    wait_for_eof(std::io::empty());
}

#[cfg(unix)]
#[test]
fn should_adopt_only_a_loopback_tcp_listener_descriptor() {
    use std::os::fd::{IntoRawFd, OwnedFd};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let fd = OwnedFd::from(listener).into_raw_fd();
    let adopted = super::adopt_listener_fd(fd).unwrap();
    assert_eq!(adopted.local_addr().unwrap(), addr);
    let flags = rustix::io::fcntl_getfd(&adopted).unwrap();
    assert!(flags.contains(rustix::io::FdFlags::CLOEXEC));

    let wildcard = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    assert!(super::adopt_listener_fd(OwnedFd::from(wildcard).into_raw_fd()).is_err());
    let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    assert!(super::adopt_listener_fd(OwnedFd::from(udp).into_raw_fd()).is_err());
    assert!(
        super::adopt_listener_fd(1).is_err(),
        "stdio is never adopted"
    );
}

#[test]
fn should_allow_external_clients_only_the_allowlisted_methods() {
    use super::{EXTERNAL_ALLOWED_METHODS, EXTERNAL_METHOD_DENIED, external_gate};
    use serde_json::json;
    let supported = crate::api::ui_protocol_transport::ui_protocol_server_supported_methods();
    for method in EXTERNAL_ALLOWED_METHODS {
        assert!(
            supported.contains(method),
            "{method} is not a dispatched method"
        );
    }
    let opened: std::collections::HashSet<String> = ["_main:api:web".to_owned()].into();
    let params = json!({"session_id": "_main:api:web"});
    // Every dispatched method is refused unless it is on the allowlist.
    let mut sensitive = supported.clone();
    sensitive.extend([
        "session/list",
        "session/delete",
        "session/title.set",
        "memory/ingest",
        "memory/search",
        "content/delete",
        "session/files.list",
        "session/workspace.get",
    ]);
    for method in sensitive {
        let allowed = external_gate(method, &params, &opened).is_ok();
        assert_eq!(
            allowed,
            EXTERNAL_ALLOWED_METHODS.contains(&method),
            "{method}"
        );
        if !allowed {
            let error = external_gate(method, &params, &opened).unwrap_err();
            assert_eq!(
                error.data.unwrap()["kind"],
                json!(EXTERNAL_METHOD_DENIED),
                "{method}"
            );
        }
    }
    for method in [
        "profile/llm/upsert",
        "profile/sub_providers/upsert",
        "profile/skills/install",
        "profile/skills/remove",
        "snapshot/restore",
        "skill/action/invoke",
        "session/fork",
        "peer/prepare",
        "peer/context/open",
        "server/shutdown",
        "profile/local/create",
    ] {
        assert!(external_gate(method, &params, &opened).is_err(), "{method}");
    }
}

#[test]
fn should_refuse_external_calls_on_host_owned_peer_sessions() {
    use super::{
        EXTERNAL_SESSION_NOT_OPENED, HOST_OWNED_PEER_ANSWER_DENIED, HOST_OWNED_PEER_SESSION_DENIED,
        external_gate,
    };
    use serde_json::json;
    let peer = "_main:api:octosense#peer-rinx";
    let ctx = "_main:api:octosense#peerctx-rinx.app-a";
    let opened: std::collections::HashSet<String> = [peer.to_owned(), ctx.to_owned()].into();
    let kind = |r: Result<(), octos_core::ui_protocol::RpcError>| {
        r.unwrap_err().data.unwrap()["kind"].clone()
    };
    for session in [peer, ctx] {
        for method in [
            "session/open",
            "turn/start",
            "turn/steer",
            "session/messages_page",
            "session/hydrate",
        ] {
            assert_eq!(
                kind(external_gate(
                    method,
                    &json!({"session_id": session}),
                    &opened
                )),
                json!(HOST_OWNED_PEER_SESSION_DENIED),
                "{method}"
            );
        }
        assert_eq!(
            kind(external_gate(
                "approval/respond",
                &json!({"session_id": session}),
                &opened
            )),
            json!(HOST_OWNED_PEER_ANSWER_DENIED)
        );
    }
    // Answers only on a session this connection opened.
    assert_eq!(
        kind(external_gate(
            "approval/respond",
            &json!({"session_id": "_main:api:other"}),
            &opened
        )),
        json!(EXTERNAL_SESSION_NOT_OPENED)
    );
    let mine: std::collections::HashSet<String> = ["_main:api:octosense#system".to_owned()].into();
    assert!(
        external_gate(
            "approval/respond",
            &json!({"session_id": "_main:api:octosense#system"}),
            &mine
        )
        .is_ok()
    );
    assert!(
        external_gate(
            "turn/start",
            &json!({"session_id": "_main:api:octosense#system"}),
            &mine
        )
        .is_ok()
    );
}

#[test]
fn should_give_external_turns_no_code_admin_or_peer_tools() {
    use super::external_turn_tool_allowed;
    for tool in [
        "shell",
        "bash",
        "exec_command",
        "spawn",
        "peer_respond",
        "peer_send_input",
        "admin_update_octos",
        "manage_skills",
        "browser",
        "git",
        "deep_crawl",
        // Default-deny: MCP wrappers and plugin tools, whatever their names.
        "mcp_filesystem_read",
        "github__create_issue",
        "weather",
        "run_python",
    ] {
        assert!(!external_turn_tool_allowed(tool), "{tool}");
    }
    for tool in [
        "read_file",
        "write_file",
        "edit_file",
        "web_search",
        "web_fetch",
        "ask_user_question",
        "recall",
    ] {
        assert!(external_turn_tool_allowed(tool), "{tool}");
    }
}

#[test]
fn should_confine_external_calls_to_the_main_profile_at_any_depth() {
    use super::{EXTERNAL_PROFILE_DENIED, HOST_OWNED_PEER_SESSION_DENIED, external_gate};
    use serde_json::json;
    let none = std::collections::HashSet::new();
    let kind = |r: Result<(), octos_core::ui_protocol::RpcError>| {
        r.unwrap_err().data.unwrap()["kind"].clone()
    };
    assert_eq!(
        kind(external_gate(
            "session/status/read",
            &json!({"profile_id": "dev"}),
            &none
        )),
        json!(EXTERNAL_PROFILE_DENIED)
    );
    assert_eq!(
        kind(external_gate(
            "session/open",
            &json!({"session_id": "dev:api:x"}),
            &none
        )),
        json!(EXTERNAL_PROFILE_DENIED)
    );
    assert!(
        external_gate(
            "session/status/read",
            &json!({"profile_id": "_main"}),
            &none
        )
        .is_ok()
    );
    // A peer session nested in an object or array is found too.
    assert_eq!(
        kind(external_gate(
            "turn/start",
            &json!({"session_id": "_main:api:web", "context": {"target": {"session_id": "_main:api:octosense#peer-rinx"}}}),
            &none
        )),
        json!(HOST_OWNED_PEER_SESSION_DENIED)
    );
    assert_eq!(
        kind(external_gate(
            "turn/start",
            &json!({"sessions": ["_main:api:octosense#peerctx-rinx.a"]}),
            &none
        )),
        json!(HOST_OWNED_PEER_SESSION_DENIED)
    );
}

#[test]
fn should_keep_pairing_off_when_the_enable_cannot_be_audited() {
    let host_managed = HostManaged::new(HOST.into(), Some(EXTERNAL.into()), 1).unwrap();
    let response =
        super::enable_pairing_audited(&host_managed, |_| Err(eyre::eyre!("audit store down")));
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        host_managed.pairing().is_none(),
        "an unaudited code is never live"
    );
    let response = super::enable_pairing_audited(&host_managed, |_| Ok(()));
    assert_eq!(response.status(), StatusCode::OK);
    assert!(host_managed.pairing().is_some());
}
