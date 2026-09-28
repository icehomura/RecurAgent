# Host-managed serve

`octos serve --host-managed` runs the HTTP/WebSocket server as a child of an
embedding host, such as an app shell on a phone or desktop. The host starts
the process, owns its lifecycle, and decides whether an external client (a web
client or a terminal UI the person runs) may attach to the same agent runtime.
It is the serve counterpart of [`octos acp --host-managed`](HOST_MANAGED_ACP.md):
opt-in, fail-closed, and without effect on ordinary `octos serve`.

The protocol-visible parts are specified in
[UPCR-2026-036](OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_036_HOST_MANAGED_SERVE.md).

## Threat model

Loopback is not an authentication boundary. On Android every installed app
can connect to `127.0.0.1`; on a shared computer so can every other local
user; and a web page can reach loopback through the person's browser (DNS
rebinding, cross-site WebSocket). The server therefore requires a bearer token
on every route except the unauthenticated public ones (`/health`,
`/api/version`, `/pair/*`), and it trusts nothing about a connection's origin
by itself.

## Credentials

| Credential | Source | Identity | May use |
| --- | --- | --- | --- |
| Host token | `OCTOS_AUTH_TOKEN` (environment only) | admin | every route, as an admin token does today |
| External token | `OCTOS_HOST_EXTERNAL_TOKEN` (optional) | user `_main`, role user | `GET /api/ui-protocol/ws` only |

- Only these two tokens authenticate. There is no solo login (not even with
  `OCTOS_SOLO_LOGIN`), no trusted-proxy `X-Profile-Id`, no hashed admin-token
  store, no `OCTOS_TEST_TOKEN` and no OTP session.
- `--auth-token` and the config file's `auth_token` are refused: argv is
  visible to every local process and a file outlives the host. Tokens must be
  at least 32 characters of RFC 7230 `tchar`, and the two must differ.
- Neither token is printed, logged or returned by a route. The only exception
  is a successful pairing claim, which returns the external token (see
  below). Both variable names match the secret-name heuristic, so tool, hook
  and MCP subprocesses do not inherit them.
- Without `OCTOS_HOST_EXTERNAL_TOKEN` external clients are disabled. To
  revoke or rotate external access, the host restarts the server without the
  variable or with a new value; open external connections end with the
  process.

An external identity:

- gets 403 on every REST route and 401 on every `/api/admin/*` route
  (`stop-all`, `token/rotate` and the rest included);
- cannot call `server/shutdown`, which a host-managed server never
  advertises or accepts;
- cannot answer `approval/respond` or `user_question/respond` for a
  host-owned app-peer session (`peer-…` or `peerctx-…` topics). The request
  fails with `data.kind: "host_owned_peer_answer_denied"` and the prompt stays
  parked for the person, who answers it in the app through the host. This
  extends UPCR-2026-034's "Approvals belong to the person" to external
  clients;
- cannot touch the host-owned app-peer control plane: `peer/model/set`,
  `peer/context/open`, `peer/context/close`, and `peer/prepare` with
  `memory_namespace`, `resume` or `host_token` fail with
  `data.kind: "host_owned_peer_control_denied"`. Otherwise an external client
  could self-report an originator `session_id` and bind a namespace nesting
  with a legitimate app's (the #2556 residuals). Ordinary peers stay
  available.

## Network guards

- **Bind.** `--host 127.0.0.1` only, and a Local deployment.
- **Host header.** Every request must name the listener:
  `127.0.0.1:<port>`, `localhost:<port>` or `[::1]:<port>`; anything else is
  answered 421 before routing. This blocks DNS rebinding. A tunnel into the
  device must preserve the port number (for example `adb forward tcp:P tcp:P`).
- **Origin.** CORS and the WebSocket upgrade trust only the configured origins
  (`appui.allowed_origins` or `OCTOS_APPUI_ALLOWED_ORIGINS`), without the
  built-in development, ominix or per-tenant origins, and without the
  listener's own loopback origins. A WebSocket upgrade that carries the
  browser-only `Sec-Fetch-*` headers but no `Origin` is refused. A client that
  is not a browser sends neither and proceeds to the token check.
  Origin only protects a browser that holds a token from other web pages.
  It does not authenticate: any local process can send any `Origin`. The
  token is the control.

## Sending the token

In order of preference:

1. `Authorization: Bearer <token>` (native clients).
2. `Sec-WebSocket-Protocol: octos-ui, octos.bearer.<token>` (browsers, which
   cannot set `Authorization` on a WebSocket). The server selects `octos-ui`,
   so the bearer entry is never echoed. Offer both entries: a browser fails a
   handshake in which the server selects none of the offered protocols.
3. `?token=<token>`, kept for compatibility. It is never logged (request
   spans record route templates), but URLs can end up in browser history, so
   prefer 2.

The subprotocol path works on every `octos serve`, not only host-managed.

## Pairing

A host-managed server mints no pairing code at startup and prints none.
While the host shows its pairing UI, it calls:

- `POST /api/admin/host/pairing` (host token): mints an 8-character code
  (Crockford base32) valid for five minutes and for one successful claim, and
  returns `{code, server_origin, expires_in_secs}`. A new call replaces the
  previous code. With external access disabled it answers 409
  `external_access_disabled`.
- `DELETE /api/admin/host/pairing` (host token): turns pairing off.

`/pair/info` and `/pair/claim` keep their contract (loopback only, 404 when
pairing is off). A claim returns the external token, never the host token.
Both host calls are recorded in the admin audit log (`host.pairing.enable`,
`host.pairing.disable`) without the code; if the enable cannot be audited,
pairing stays off and the call fails.

## Lifecycle

- **Stdin.** The host keeps the child's stdin open as a lifeline. Bytes are
  ignored; EOF (the host closed it, exited or crashed) stops the server
  through the normal drain path, the same one SIGTERM takes.
- **Orphans, per platform.** Stdin EOF covers every way the host can end on
  every platform: exit, crash, SIGKILL (macOS, Linux, Android) or
  TerminateProcess (Windows) all close the host's end of the pipe, because the
  OS closes a dead process's descriptors. `tests/serve_host_managed.rs` kills
  the pipe's holder with SIGKILL. EOF cannot see a host whose pipe end
  outlives it, because another process inherited it. On Linux and Android the
  server therefore also asks for SIGTERM when its parent dies
  (`PR_SET_PDEATHSIG`), and refuses to start if the parent is already gone.
  The signal follows the parent thread that spawned the child, so a host
  spawns it from a long-lived thread. On macOS and Windows the host must not
  leak the write end: spawn children close-on-exec, as Rust's
  `std::process` does.
- **Profiles run in process.** As with `--solo`, profiles run inside the
  server; no gateway children are started.
- **Inherited listener (Unix).** With `--listen-fd <FD>` the server serves the
  listening TCP socket the host passed as descriptor `FD` (stream, bound to
  `127.0.0.1`, not stdio) instead of binding `--port`. The host keeps its copy
  across restarts, so no other process can take the port while no server
  runs, and clients that connect in between wait in the backlog. The server
  makes its copy close-on-exec. Without it, a host that restarts the server
  on a fixed port can lose that port to another local process; the host
  should then bind a new port and rotate the external token.

```sh
OCTOS_AUTH_TOKEN=<host> OCTOS_HOST_EXTERNAL_TOKEN=<external> NO_COLOR=1 \
  octos serve --host-managed --host 127.0.0.1 --port 0 --data-dir <dir>
```

The server prints `Listening: http://127.0.0.1:<port>` when it accepts
connections.
