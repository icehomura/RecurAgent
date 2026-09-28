//! `octos serve --host-managed`: a loopback server owned by an embedding host.
//!
//! An app shell (for example an OctoSense phone or desktop shell) runs ONE
//! `octos serve` for its own native clients and may let the person attach an
//! external client (a web client or a terminal UI) to the same agent runtime.
//! Loopback is not an authentication boundary on a phone (any installed app
//! can connect to 127.0.0.1) or on a multi-user computer, so this mode keeps
//! the bearer token mandatory and distinguishes two credentials:
//!
//! | Credential | Source | Identity | Reaches |
//! | --- | --- | --- | --- |
//! | host token | `OCTOS_AUTH_TOKEN` (env only) | [`AuthIdentity::Admin`] | every route, as a normal admin token |
//! | external token | `OCTOS_HOST_EXTERNAL_TOKEN` (env, optional) | `User { _main, role: User }` | `/api/ui-protocol/ws` only |
//!
//! Neither token is written anywhere, returned by any route (except the
//! external token, once, to a successful pairing claim the host enabled), or
//! logged. Both env names look secret to
//! [`octos_core::env_hygiene::is_secret_env_name`], so tool, hook and MCP
//! subprocesses never inherit them.
//!
//! What else the mode changes (see `docs/HOST_MANAGED_SERVE.md`):
//!
//! - no solo login, no trusted-proxy `X-Profile-Id`, no hashed admin-token
//!   store, no `OCTOS_TEST_TOKEN`, no OTP sessions: only the two tokens;
//! - the `Host` header must name the bound loopback listener, which blocks
//!   DNS rebinding;
//! - the browser `Origin` allowlist is only the configured origins;
//! - an external identity cannot answer approvals or questions of the
//!   host-owned app-peer sessions (`peer-…`, `peerctx-…`);
//! - pairing is off until the host asks for a code;
//! - `server/shutdown` is never offered: the host owns the lifecycle, and the
//!   process stops when its stdin reaches EOF (the host exited or closed it).

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use octos_core::{MAIN_PROFILE_ID, SessionKey};

use super::AppState;
use super::pairing::PairingState;
use super::router::{AuthIdentity, constant_time_eq};
use crate::user_store::UserRole;

/// The env var that carries the host token. `--auth-token` and the config
/// file's `auth_token` are refused in this mode: argv is visible to every
/// local process, and a file outlives the host.
pub const HOST_TOKEN_ENV: &str = "OCTOS_AUTH_TOKEN";

/// The env var that carries the optional external-client token.
pub const EXTERNAL_TOKEN_ENV: &str = "OCTOS_HOST_EXTERNAL_TOKEN";

/// Tokens shorter than this are refused (128 bits of hex).
pub const MIN_TOKEN_LEN: usize = 32;

/// The only route an external identity may use.
pub const EXTERNAL_ROUTE: &str = "/api/ui-protocol/ws";

/// `data.kind` of a refused external answer to a host-owned peer.
pub const HOST_OWNED_PEER_ANSWER_DENIED: &str = "host_owned_peer_answer_denied";

/// `data.kind` of a refused external call to the host-owned app-peer control
/// plane.
pub const HOST_OWNED_PEER_CONTROL_DENIED: &str = "host_owned_peer_control_denied";

/// Whether an external connection may make this raw call. Host-owned app
/// peers (UPCR-2026-034) are the host's: an external client may neither
/// create, resume nor bind one (`peer/prepare` with `memory_namespace`,
/// `resume` or `host_token`), nor change its lane or open or close its
/// request contexts. Their originator check trusts a self-reported
/// `session_id`, and a namespace may nest with a legitimate app's (the
/// #2556 residuals), so this mode keeps the whole control plane host-only.
/// Ordinary peers stay available.
pub fn external_may_call(method: &str, params: Option<&serde_json::Value>) -> bool {
    match method {
        "peer/model/set" | "peer/context/open" | "peer/context/close" => false,
        "peer/prepare" => !params.is_some_and(|params| {
            ["memory_namespace", "resume", "host_token"]
                .iter()
                .any(|field| params.get(field).is_some_and(|value| !value.is_null()))
        }),
        _ => true,
    }
}

/// The refusal for [`external_may_call`].
pub fn peer_control_denied(method: &str) -> octos_core::ui_protocol::RpcError {
    octos_core::ui_protocol::RpcError::permission_denied(format!(
        "{method}: host-owned app peers are managed by the host, not external clients"
    ))
    .with_data(serde_json::json!({ "kind": HOST_OWNED_PEER_CONTROL_DENIED }))
}

/// Host-managed authentication and lifecycle state (`AppState::host_managed`).
pub struct HostManaged {
    host_token: String,
    external_token: Option<String>,
    /// Lower-case `Host` values that name this listener.
    allowed_hosts: Vec<String>,
    server_origin: String,
    /// The pairing code the host asked for, if any. Pairing is off (the
    /// `/pair/*` routes answer 404) while this is `None`.
    pairing: Mutex<Option<Arc<PairingState>>>,
}

impl std::fmt::Debug for HostManaged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostManaged")
            .field("external_access", &self.external_token.is_some())
            .field("allowed_hosts", &self.allowed_hosts)
            .finish_non_exhaustive()
    }
}

fn validate_token(name: &str, token: &str) -> eyre::Result<()> {
    eyre::ensure!(
        token.len() >= MIN_TOKEN_LEN,
        "{name} must be at least {MIN_TOKEN_LEN} characters for --host-managed"
    );
    // Header- and subprotocol-safe: RFC 7230 `tchar`s only, so the token can
    // ride in `Authorization`, in `Sec-WebSocket-Protocol` and in a query.
    eyre::ensure!(
        token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)),
        "{name} may contain only letters, digits and !#$%&'*+-.^_`|~"
    );
    Ok(())
}

impl HostManaged {
    /// Validate both credentials for a listener on `127.0.0.1:<port>`.
    pub fn new(
        host_token: String,
        external_token: Option<String>,
        port: u16,
    ) -> eyre::Result<Self> {
        validate_token(HOST_TOKEN_ENV, &host_token)?;
        let external_token = external_token.filter(|token| !token.is_empty());
        if let Some(external) = &external_token {
            validate_token(EXTERNAL_TOKEN_ENV, external)?;
            eyre::ensure!(
                !constant_time_eq(external.as_bytes(), host_token.as_bytes()),
                "{EXTERNAL_TOKEN_ENV} must differ from {HOST_TOKEN_ENV}"
            );
        }
        eyre::ensure!(port != 0, "--host-managed needs the bound port");
        Ok(Self {
            host_token,
            external_token,
            allowed_hosts: vec![
                format!("127.0.0.1:{port}"),
                format!("localhost:{port}"),
                format!("[::1]:{port}"),
            ],
            server_origin: format!("http://127.0.0.1:{port}"),
            pairing: Mutex::new(None),
        })
    }

    /// Resolve a bearer token. Only the two configured tokens authenticate.
    pub fn resolve(&self, token: &str) -> Option<AuthIdentity> {
        if token.is_empty() {
            return None;
        }
        if constant_time_eq(token.as_bytes(), self.host_token.as_bytes()) {
            return Some(AuthIdentity::Admin);
        }
        match &self.external_token {
            Some(external) if constant_time_eq(token.as_bytes(), external.as_bytes()) => {
                Some(external_identity())
            }
            _ => None,
        }
    }

    /// Whether the request's `Host` names this listener.
    pub fn host_allowed(&self, headers: &HeaderMap, authority: Option<&str>) -> bool {
        let host = headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .or(authority);
        host.is_some_and(|host| {
            let host = host.trim().to_ascii_lowercase();
            self.allowed_hosts.contains(&host)
        })
    }

    /// The pairing state the host enabled, if any.
    pub fn pairing(&self) -> Option<Arc<PairingState>> {
        self.pairing
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Mint a fresh single-use, five-minute code for the external token. A
    /// previous code is replaced. `None` when external access is disabled.
    pub fn enable_pairing(&self) -> Option<Arc<PairingState>> {
        let token = self.external_token.clone()?;
        let pairing = Arc::new(PairingState::mint(self.server_origin.clone(), Some(token)));
        *self
            .pairing
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(pairing.clone());
        Some(pairing)
    }

    /// Turn pairing off again.
    pub fn disable_pairing(&self) {
        *self
            .pairing
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
}

/// The identity an external token authenticates as.
pub fn external_identity() -> AuthIdentity {
    AuthIdentity::User {
        id: MAIN_PROFILE_ID.to_owned(),
        role: UserRole::User,
    }
}

/// Whether `identity` on a host-managed server is an external client. The
/// host token is the only admin credential there, so everything else is.
pub fn is_external(state: &AppState, identity: Option<&AuthIdentity>) -> bool {
    state.host_managed.is_some() && !matches!(identity, Some(AuthIdentity::Admin))
}

/// A host-owned app-peer session (`peer-<slug>` or `peerctx-<slug>.<ctx>`),
/// whose approvals and questions only the host (the person, in the app's own
/// UI) answers. See UPCR-2026-034 "Approvals belong to the person".
pub fn is_peer_session(session_id: &SessionKey) -> bool {
    session_id.topic().is_some_and(|topic| {
        topic.starts_with("peer-")
            || topic.starts_with(crate::peers::app_binding::PEER_CONTEXT_TOPIC_PREFIX)
    })
}

/// The refusal an external answer to a host-owned peer gets.
pub fn peer_answer_denied(what: &str) -> octos_core::ui_protocol::RpcError {
    octos_core::ui_protocol::RpcError::permission_denied(format!(
        "an external client cannot answer a host-owned app peer's {what}; answer it in the app"
    ))
    .with_data(serde_json::json!({ "kind": HOST_OWNED_PEER_ANSWER_DENIED }))
}

/// Outermost guard: refuse a request whose `Host` does not name the loopback
/// listener (DNS rebinding). A no-op unless host-managed.
pub(crate) async fn host_header_guard(
    State(state): State<Arc<AppState>>,
    req: axum::http::Request<axum::body::Body>,
    next: Next,
) -> Response {
    if let Some(host_managed) = &state.host_managed {
        let authority = req.uri().authority().map(|a| a.as_str().to_owned());
        if !host_managed.host_allowed(req.headers(), authority.as_deref()) {
            tracing::warn!(
                target: "octos::api::host_managed",
                "rejected request with a Host header that does not name the loopback listener"
            );
            return (StatusCode::MISDIRECTED_REQUEST, "unexpected Host").into_response();
        }
    }
    next.run(req).await
}

/// `POST /api/admin/host/pairing` (host token only): mint a one-time code a
/// loopback web client exchanges for the EXTERNAL token at `/pair/claim`.
/// The host calls this only while its pairing UI is open.
pub(crate) async fn enable_pairing(
    State(state): State<Arc<AppState>>,
    identity: Option<axum::Extension<AuthIdentity>>,
) -> Response {
    let Some(host_managed) = &state.host_managed else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(pairing) = host_managed.enable_pairing() else {
        return (
            StatusCode::CONFLICT,
            axum::Json(serde_json::json!({ "error": { "kind": "external_access_disabled" } })),
        )
            .into_response();
    };
    // Audited without the code: who enabled pairing, and for how long.
    if let Err(error) = super::admin_audit::record_admin_action(
        &state,
        identity.as_ref().map(|identity| &identity.0),
        "host.pairing.enable",
        "external",
        None,
        Some(serde_json::json!({ "expires_in_secs": super::pairing::PAIR_CODE_TTL.as_secs() })),
    ) {
        host_managed.disable_pairing();
        tracing::error!(%error, "could not audit the pairing; pairing stays off");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    axum::Json(serde_json::json!({
        "code": pairing.printed_code(),
        "server_origin": pairing.server_origin(),
        "expires_in_secs": super::pairing::PAIR_CODE_TTL.as_secs(),
    }))
    .into_response()
}

/// `DELETE /api/admin/host/pairing` (host token only): pairing off.
pub(crate) async fn disable_pairing(
    State(state): State<Arc<AppState>>,
    identity: Option<axum::Extension<AuthIdentity>>,
) -> Response {
    let Some(host_managed) = &state.host_managed else {
        return StatusCode::NOT_FOUND.into_response();
    };
    host_managed.disable_pairing();
    // Off regardless; a failed audit write is logged, not undone.
    if let Err(error) = super::admin_audit::record_admin_action(
        &state,
        identity.as_ref().map(|identity| &identity.0),
        "host.pairing.disable",
        "external",
        None,
        None,
    ) {
        tracing::error!(%error, "could not audit turning pairing off");
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Stop the server when stdin reaches EOF: the host exited or closed its end.
/// Bytes read are ignored. Runs on a plain thread (stdin reads block).
pub(crate) fn spawn_stdin_eof_watcher(stop: Arc<tokio::sync::watch::Sender<bool>>) {
    let spawned = std::thread::Builder::new()
        .name("octos-host-stdin".into())
        .spawn(move || {
            wait_for_eof(std::io::stdin().lock());
            tracing::info!(
                target: "octos::api::host_managed",
                "host closed stdin; stopping"
            );
            stop.send_replace(true);
        });
    if let Err(error) = spawned {
        tracing::error!(%error, "could not watch stdin; stopping instead of running unowned");
        // Fail closed: an unwatched host-managed server could outlive its host.
        std::process::exit(1);
    }
}

fn wait_for_eof(mut input: impl std::io::Read) {
    let mut buf = [0u8; 256];
    loop {
        match input.read(&mut buf) {
            Ok(0) => return,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// Linux/Android: ask the kernel for SIGTERM when the parent dies (SIGTERM
/// takes the same graceful path as `stop`).
///
/// Orphan posture on every platform: stdin EOF is the lifeline. When the
/// host process ends for any reason (exit, crash, SIGKILL, Windows
/// TerminateProcess), the OS closes its end of the pipe and the server
/// stops; `tests/serve_host_managed.rs` SIGKILLs the pipe's holder to prove
/// it. What EOF cannot see is a host whose pipe end outlives it (inherited by
/// another process it spawned). Linux/Android add this parent-death signal
/// for that case; elsewhere the host must not leak the write end (spawn it
/// close-on-exec, as Rust's `std::process` does). The signal follows the
/// parent THREAD that spawned us, so hosts spawn from a long-lived thread.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn bind_to_parent() -> eyre::Result<()> {
    use eyre::WrapErr;
    let parent = rustix::process::getppid();
    rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::TERM))
        .wrap_err("could not set the parent-death signal")?;
    // The parent may have died before the signal was armed.
    eyre::ensure!(
        rustix::process::getppid() == parent,
        "the host exited during startup"
    );
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(crate) fn bind_to_parent() -> eyre::Result<()> {
    Ok(())
}

/// Adopt the listening socket the host passed as descriptor `fd`, so the host
/// keeps the port across server restarts (nobody else can take it while no
/// server runs; connections queue in the backlog). The socket must be TCP and
/// bound to 127.0.0.1; it is made close-on-exec and non-blocking.
#[cfg(unix)]
#[allow(unsafe_code)]
pub(crate) fn adopt_listener_fd(fd: i32) -> eyre::Result<std::net::TcpListener> {
    use eyre::WrapErr;
    use std::os::fd::{FromRawFd, OwnedFd};
    eyre::ensure!(
        fd > 2,
        "--listen-fd must name an inherited descriptor other than stdin, stdout or stderr"
    );
    // Validate the number names an open descriptor before taking ownership.
    rustix::io::fcntl_getfd(
        // SAFETY: the borrow ends with this call; fcntl(F_GETFD) on a closed
        // number fails with EBADF instead of touching memory.
        unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) },
    )
    .wrap_err("--listen-fd does not name an open descriptor")?;
    // SAFETY: the descriptor is open (checked above) and was handed to this
    // process by its parent for this exact purpose; nothing else in the
    // process refers to it, so taking ownership cannot double-close.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    eyre::ensure!(
        rustix::net::sockopt::socket_type(&owned).wrap_err("--listen-fd is not a socket")?
            == rustix::net::SocketType::STREAM,
        "--listen-fd is not a stream socket"
    );
    rustix::io::fcntl_setfd(&owned, rustix::io::FdFlags::CLOEXEC)
        .wrap_err("could not make the inherited listener close-on-exec")?;
    rustix::net::listen(&owned, 1024).wrap_err("the inherited socket cannot listen")?;
    let listener = std::net::TcpListener::from(owned);
    let addr = listener
        .local_addr()
        .wrap_err("the inherited socket has no local address")?;
    eyre::ensure!(
        addr.ip() == std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST) && addr.port() != 0,
        "--listen-fd must be bound to 127.0.0.1 (got {addr})"
    );
    listener
        .set_nonblocking(true)
        .wrap_err("could not make the inherited listener non-blocking")?;
    Ok(listener)
}

#[cfg(not(unix))]
pub(crate) fn adopt_listener_fd(_fd: i32) -> eyre::Result<std::net::TcpListener> {
    eyre::bail!("--listen-fd is supported on Unix only")
}

#[cfg(test)]
#[path = "host_managed_tests.rs"]
mod host_managed_tests;
