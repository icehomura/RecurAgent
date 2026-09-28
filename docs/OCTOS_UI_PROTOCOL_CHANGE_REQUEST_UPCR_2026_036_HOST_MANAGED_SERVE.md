# Octos UI Protocol Change Request: Host-Managed Serve

## Header

- Request id: `UPCR-2026-036`
- Date: 2026-09-28
- Target protocol: `octos-ui/v1alpha1`
- Status: implemented
- Scope: transport authentication (a WebSocket bearer subprotocol) on every
  server; for `octos serve --host-managed` only, the external-client identity,
  its refusal to answer host-owned app peers, and the unavailability of
  `server/shutdown`. No method, event or field is added or removed.
- Origin: OctoSense shells share one kernel between native apps and an
  external web or terminal client (see `docs/HOST_MANAGED_SERVE.md`).

## Problem

An app shell owns one `octos serve` for its native clients and wants the
person to attach a web client or terminal UI to the same runtime. The existing
serve modes do not fit:

1. On a phone every installed app reaches loopback, so solo login and the
   trusted-loopback `X-Profile-Id` path would hand the runtime to any app.
2. One admin token would give an external client everything the host has,
   including the admin API and the ability to answer a host-owned app peer's
   approvals, which UPCR-2026-034 reserves for the person in the app.
3. A browser cannot set `Authorization` on a WebSocket, so the token travels
   in the URL.
4. `server/shutdown` lets any client stop a process the host owns.

## Contract

### WebSocket bearer subprotocol (every server)

A client may authenticate the `/api/ui-protocol/ws` upgrade with

```text
Sec-WebSocket-Protocol: octos-ui, octos.bearer.<token>
```

The token is read from the first `octos.bearer.` entry. It is checked after
`Authorization: Bearer` and before `?token=`/`?_token=`, which remain
supported. When the client offers `octos-ui`, the server selects it in the
handshake response. The bearer entry is never echoed. A client that offers
the bearer entry must also offer `octos-ui`, because browsers fail a handshake
in which the server selects no offered protocol. Clients that offer no
subprotocol are unaffected.

### Host-managed identities

On a host-managed server exactly two tokens authenticate: the host token
(admin) and, when the host configured one, the external token, which resolves
to the user identity `_main` (role user). The external identity may upgrade
`/api/ui-protocol/ws` and use no other route: 403 on REST routes, 401 on
`/api/admin/*`.

### Native clients keep the stdio feature set

`octos_core::ui_protocol::UI_PROTOCOL_STDIO_DEFAULT_FEATURES` lists the
features a `--stdio` connection has without negotiating. A host that moves a
native client from the stdio pipe to the host-managed WebSocket sends exactly
these in `X-Octos-Ui-Features`. This adds no feature: the list names existing
ones, and a test keeps it equal to the stdio defaults.

### Host-owned app peers answer to the person

On a host-managed server, `approval/respond` and `user_question/respond` from
any connection that is not authenticated with the host token, including a
session-ingress connection, are refused when `session_id`'s topic starts with
`peer-` or `peerctx-`:

```json
{"code": -32120, "message": "an external client cannot answer a host-owned app peer's approval; answer it in the app",
 "data": {"kind": "host_owned_peer_answer_denied"}}
```

(`permission_denied`; the message says `question` for `user_question/respond`.)
Nothing is decided, and the prompt stays pending for the host. The external
client still answers prompts on every other session of its profile, such as the
shared system conversation. This extends UPCR-2026-034's "Approvals belong to
the person" from the owning system agent to external clients.

### Host-owned app peers are the host's to manage

On a host-managed server, a connection that is not authenticated with the
host token cannot call `peer/model/set`, `peer/context/open` or
`peer/context/close`, nor `peer/prepare` with `memory_namespace`, `resume`
or `host_token` (non-null). These calls fail with `permission_denied`,
`data.kind: "host_owned_peer_control_denied"`. Ordinary `peer/prepare` and the
other raw methods are unchanged.

### `server/shutdown`

A host-managed server never advertises `server/shutdown` in
`config/capabilities/list`, and answers `server_shutdown_unavailable` to every
caller. It already required solo login (UPCR-2026-032), which host-managed
never enables. The host stops the server by closing its stdin.

## Risk

- The external token grants the full UI Protocol surface of profile `_main`
  (turns, sessions, and raw methods of that profile), which is what attaching
  a client to the person's assistant means. The UPCR-2026-034 session-plane
  caveat applies: hosts must not give raw protocol access to untrusted apps.
  The external token is for a client the person chose to attach.
- **Without the control-plane gate,** an external client could combine
  the two #2556 residuals. `peer/prepare` trusts a self-reported originator
  `session_id`, and a new host-owned peer's namespace is checked only against
  existing peers, not against what the host will later create. An external
  client could therefore mint a host-owned peer that shares memory stores
  with a legitimate app's peer. Under host-managed those calls would ride the
  host's authority context. This UPCR removes that path by keeping the whole
  host-owned control plane host-only. #2556 remains the general fix for
  other deployments.
- A peer topic prefix is the refusal criterion, so ordinary agent-staged
  peers' prompts are also host-only for external clients. Their originator
  answers them through `peer_respond`, which is unchanged.

## Tests

- `should_accept_a_bearer_subprotocol_without_echoing_it`
- `should_resolve_only_the_two_configured_tokens`
- `should_confine_the_external_token_to_the_ui_protocol_socket` (includes
  `supports_server_shutdown` staying false)
- `should_reach_admin_routes_only_with_the_host_token`
- `should_refuse_an_external_answer_to_a_host_owned_peer_approval`
- `should_refuse_an_external_answer_to_a_host_owned_peer_question`
- `should_let_the_host_answer_a_host_owned_peer_approval`
- `should_let_an_external_client_answer_its_own_session_approval`
- `should_admit_only_configured_origins_on_a_host_managed_ws_upgrade`
- `stdio_default_feature_list_matches_the_stdio_defaults`
- `should_keep_the_host_owned_peer_control_plane_from_external_clients`
- `should_refuse_external_calls_to_the_host_owned_peer_control_plane`
- `should_audit_the_pairing_ceremony_without_the_code`
- `tests/serve_host_managed.rs`: stdin EOF, host SIGKILL, inherited listener
  (serial CI step)
