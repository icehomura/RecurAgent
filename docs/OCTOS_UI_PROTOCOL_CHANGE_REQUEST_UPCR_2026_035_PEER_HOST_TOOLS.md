# Octos UI Protocol Change Request: Host-Registered Tools per App Peer

## Header

- Request id: `UPCR-2026-035`
- Date: 2026-09-27
- Target protocol: `octos-ui/v1alpha1`
- Status: implemented
- Scope: two additive raw AppUI methods, `peer/tools/register` and
  `peer/tool/result`; two additive server notifications, `peer/tool/call`
  and `peer/tool/cancel`; per-turn enforcement of a host-owned app peer's
  tool list and tool risk levels
- Builds on: UPCR-2026-034 (host-owned app peers, host token, request
  contexts)
- Origin: OctoSense ADR 0002 "Event-driven app agents", sections 4 ("Apps
  expose their own tools") and 12 ("What an autonomous app agent may do");
  OctoSense issue #61 (News M2)

## Problem

A host-owned app peer (UPCR-2026-034) runs with the profile's ordinary tool
roster: shell, files, web, spawn, and so on. ADR 0002 wants each app's agent
to work through **its app's own tools** — typed operations implemented where
the capability lives (the app's host service), with a risk level each — plus
the few generic tools the app names, and nothing else. The kernel had no way
to learn an app's tools, to route a call back to the host, or to hold an
outward or destructive call for the person.

## Contract

Discovery: a server that lists `peer/tools/register` in
`config/capabilities/list` `supported_methods` implements this whole UPCR;
`peer/tool/call` and `peer/tool/cancel` are in `supported_notifications`.
Both methods are raw-surface methods (a session-ingress connection cannot
call them), profile scoped, and authorized like UPCR-2026-034 control calls:
the caller names the peer's originator `session_id` and presents the peer's
`host_token`.

### `peer/tools/register`

```
{session_id, peer, host_token, profile_id?,
 tools?: [ToolDecl], generic_tools?: [string], if_version?: u64,
 call_timeout_ms?, approval_ttl_secs?, max_result_bytes?}
→ {slug, profile_id, version, previous_version,
   tools: [{name, model_name, risk, background, outward, confirm}],
   generic_tools, call_timeout_ms, approval_ttl_secs, max_result_bytes,
   applies: "next_turn"}
```

`ToolDecl` is one entry of the app bundle's `tools.json`: `{name,
description, input_schema, output_schema?, risk, background?, outward?,
confirm?, shareable?}`. Unknown fields are refused, so a misspelled flag
never silently weakens a tool:

| Field | Meaning |
| --- | --- |
| `name` | `<app>.<tool>`: 2–4 `.`-separated segments of `[a-z][a-z0-9_]{0,31}`. The model sees it with `.` replaced by `_` (`news.list` → `news_list`; providers refuse `.` in tool names). A registration whose model names collide with each other or with an allowed generic tool is refused. |
| `input_schema` | JSON Schema object (`"type": "object"`). ≤ 16 KiB. Checked structurally against the meta-schema's shapes: `type` names JSON Schema types, `properties` is an object of schemas, `required` is an array of strings, `items`, `additionalProperties`, `enum` and `anyOf`/`oneOf`/`allOf` are well formed (boolean subschemas allowed), nesting ≤ 10. |
| `output_schema` | Optional, same rules. Stored and echoed; not enforced by the kernel. |
| `risk` | `read`, `act` or `destructive`. |
| `background` | May run in a turn with no interactive client. Default `false`. |
| `outward` | Reaches past the app (send, post, share, buy). Gated like `destructive`. |
| `confirm` | `host` (default) or `app`: who confirms a gated call with the person. Independent of `risk`. |
| `shareable` | App Hub metadata; accepted, not acted on yet (see follow-ups). |

`generic_tools` names kernel tools the app may use, from an **allowlist** of
peer-safe tools: reading and searching the app's workspace (`read_file`,
`list_dir`, `glob`, `grep`, all fenced to the session scope), research
(`web_search`, `deep_search`), the app's memory namespace (`memory_search`,
`memory_load`, `recall_memory`, `save_memory`, `record_memory_use`) and
content generation (`mofa_make`, `mofa_describe_content_type`). Deliberately
not on it: `synthesize_research` (its model-chosen `research_dir` is limited
only to the profile data dir, so it could read the person's memory and other
apps' files), `view_image` (it resolves upload handles, bare names and the
global upload directory without the session scope) and `recall` (not
registered in serve). Any other name is refused
(`peer_tools_invalid`) and also stripped at every turn: shell and exec tools,
file writes and patches, messaging and channel tools (`message`,
`send_file`, …), browsers and raw fetches (`browser`, `web_fetch`,
`deep_crawl`), child agents and pipelines (`spawn`, `delegate`,
`run_pipeline`, which build registries of their own with the built-in
tools), schedulers and monitors, background-task readers, other sessions'
tools (`peer_*`, `goal_*`) and admin tools. Wider tiers (shell, files) would
be an explicit opt-in of a later UPCR. A generic tool the kernel does not
offer in a turn is simply absent.

Limits: 64 app tools, 32 generic tools, 2 KiB per description. Options are
clamped, and a host may raise the defaults up to the maximum:
`call_timeout_ms` 1–300 000 (default 30 000), `approval_ttl_secs` 1–604 800
(default 3 600), `max_result_bytes` 1–1 048 576 (default 262 144).

The set **replaces** the previous one whole; `version` increments on every
registration (the first is 1). With `if_version`, a registration whose
expected version is not the current one is refused
(`peer_tools_version_conflict`, `data.current_version`). Registrations of one
peer are serialized; different peers never wait on each other. The set is
durable (`peers/<slug>/host_tools.json`) and applies from the next turn
start. The registering connection becomes the peer's **tool host**: every
later `peer/tool/call` goes to it (a later registration from another
connection moves the route; a route whose connection is found closed is
dropped).

Typed `data.kind`: `peer_host_token_mismatch`, `peer_originator_mismatch`,
`peer_not_found`, `peer_not_host_bound`, `peer_closed`, `peer_tools_invalid`,
`peer_tools_version_conflict`.

### `peer/tool/call` (server → host notification)

```
{peer, session_id, context_id, turn_id, call_id, tool_call_id, args_digest,
 name, args, risk, confirm_required, timeout_ms, tools_version}
```

`session_id` / `context_id` identify the calling session (the peer's own, or
one of its request contexts): the caller's identity for the host service.
`name` is the declared name (`news.list`). `confirm_required` is true when
the app must confirm the call with the person itself (`confirm: app`, person
present); it is false when the kernel already holds the person's approval,
so the person is never asked twice. `timeout_ms` is how long the kernel will
wait. The notification is ephemeral: if the host connection is gone the call
fails with `host_unavailable`; it is never replayed.

**Host obligations (MUST).** A host:

- MUST execute a call at most once per `(session_id, turn_id, tool_call_id,
  args_digest)`, answering a repeat with the first result;
- MUST NOT execute a call after it received `peer/tool/cancel` for its
  `call_id`, nor after `timeout_ms` has passed without an
  `awaiting_confirmation` acknowledgement;
- MUST send `status: "awaiting_confirmation"` before showing its own
  confirmation sheet for a gated call whose `confirm_required` is false
  (for `confirm_required: true` the kernel already waits the approval TTL).

### `peer/tool/result`

```
{session_id, peer, host_token, profile_id?, call_id,
 ok?, data?, error?: {kind?, message} | string,
 status?: "awaiting_confirmation"}
→ {call_id, accepted: true, result_too_large?: {bytes, max},
   awaiting_confirmation?: true}
```

`status: "awaiting_confirmation"` is not a result: for a gated call it
extends the kernel's wait to the approval TTL (counted from the call), and
the host answers again later. `data` above `max_result_bytes` (serialized) is
not given to the model: the call ends with `result_too_large`. An error
`kind` must match `[a-z0-9_]{1,32}` and reaches the model as `host:<kind>`
(anything else becomes `host:error`), so a host can never pose as a kernel
outcome; the message is capped at 4 KiB. A result for a call that finished,
timed out or was cancelled is refused (`peer_tool_call_not_found`) and, when
the kernel still remembers the call (one hour, 1 024 calls), audited as
`late_result`.

### `peer/tool/cancel` (server → host notification)

`{call_id, reason}`, `reason` = `timeout` (no result within the wait) or
`cancelled` (the turn was interrupted while the call was in flight). After
it the host MUST NOT execute the call.

## Enforcement

For a session whose topic is `peer-<slug>` of a host-owned peer with a
registered set, or any `peerctx-<slug>.<context>` of it, every turn start:

- **Caller identity.** Neither the topic nor the base key is a credential:
  any client of the profile can open `<base>#peerctx-<slug>.<id>`, and the
  host's base key can be listed. A turn gets the peer's set only when BOTH
  hold:
  - the session is on the base key of the peer's recorded originator, and
  - the turn is driven by the connection that registered the set (the one
    that presented the host token and is the peer's tool host).

  Any other turn on a peer or context topic, and any turn with a malformed
  context topic, gets **no tools at all**. When the host's connection closes
  its routes are dropped, and the peer's turns get no tools until the host
  registers again.
  **Hosts MUST drive the peer's turns (`turn/start` on the peer and its
  request contexts) on the connection that registered the set.**
- **Kernel-internal continuations get no tools.** A turn the kernel starts
  itself on a peer session (a `peer_send_input` injection, a background
  result, a goal continuation) is nobody's turn: it gets no tools, whichever
  connection it happens to run on. Runs in the person's absence are the
  host's: it starts them with `turn/start` on its own connection.
- **Approvals stay with the host.** An approval raised by a host-routed
  call is tagged with the peer. Its `approval/requested` (and the matching
  `approval/decided`, `approval/cancelled`) is written to the session's
  ledger as usual but is filtered out of every other connection's live
  forwarding, `session/open` replay, pending-approval list and
  `session/hydrate`; `approval/respond` for it from any other connection is
  refused (`peer_host_connection_only`). The tag is held in memory; after a
  kernel restart the pending approvals are gone anyway (their waiters were),
  and only the historical events remain in the ledger.
- **Turn controls stay with the host.** `turn/steer` and `turn/interrupt`
  (including a voice turn's `supersedes_turn_id`) on the session of a peer
  with a registered set are accepted only from the peer's host connection
  (`peer_host_connection_only`).
- **No host filesystem access.** A host-bound app session never runs with
  `Host` filesystem permissions (`danger_full_access`, e.g. a Solo profile
  with `--danger-full-access` or `permission/profile/set`): the kernel
  clamps them to workspace access (keeping the approval policy), and such a
  session always carries its workspace scope; if the scope cannot be built,
  the session does not start.
- **No stale runtimes.** A session runtime records the app binding it was
  built for. The runtime cache re-checks it on every lookup and rebuilds a
  runtime whose binding changed — e.g. one cached for `<base>#peer-<slug>`
  before `peer/prepare` bound the topic, which would otherwise keep the
  profile's memory and workspace and unclamped permissions. `peer/prepare`
  and `peer/context/open` also drop every runtime cached for the bound topic
  at once.
- **Visibility.** The turn's tool registry is cut down to the allowed generic
  tools and then gets one routed tool per declared app tool. Nothing else is
  advertised, and a call to any other name is refused by the registry
  (`unknown tool`). This runs after the profile tool policy and envelope, so
  it cannot be widened by them. An empty registration means no tools, never
  the default roster. A set file that exists but cannot be read fails closed:
  no tools at all.
- **Unchanged without a registration.** A host-owned peer that never
  registered keeps today's roster, so existing hosts keep working.
- **Arguments.** Must be an object carrying every `required` property of the
  input schema and at most 64 KiB serialized; the host validates the rest.
- **Person present or absent.** A call is *attended* when it comes from an
  open request context of the peer (one of the app's interactive clients,
  the app's own conversation, UPCR-2026-034) and its turn carries an
  approval bridge (every AppUI `turn/start` does). A call from the peer's
  own session (the app agent's background runs, the system agent's
  requests) or from a turn with no bridge is *unattended*.
- **Risk.** A tool is *gated* when it is `destructive` or marked `outward`.
  - `read` and `act` run. A tool not marked `background` runs only attended
    (`not_background`).
  - Gated, `confirm: host`: an explicit approval first, attended or not,
    through the turn's existing approval bridge: the same
    `approval/requested` → `approval/respond` path every tool uses, raised
    on the calling session (the peer's or the request context's — the owning
    app's conversation, never the system agent's), carrying the exact
    arguments. Declined → error result (`denied`); no answer within
    `approval_ttl_secs` → error (`expired`) and the parked approval is
    cancelled. The host is not called in either case. UPCR-2026-034's rule
    holds: the system agent cannot answer these approvals through
    `peer_respond`; the person answers them in the app.
  - Gated, `confirm: app`, attended: the call goes to the host at once with
    `confirm_required: true` and no kernel approval — the app's own
    confirmation sheet is the only prompt (e.g. a messaging app's
    `send_message`).
  - Gated, `confirm: app`, unattended: exactly like `confirm: host` — an
    approval request in the app's conversation; an approved call reaches the
    host with `confirm_required: false`.
  - A turn with no approval bridge never runs a gated call that needs an
    approval: error (`approval_unavailable`), host not called.
  - Kernel approvals are **once-only**: they cover exactly this call and
    its arguments. A remembered approval scope (`approve_for_tool`,
    `approve_for_session`, `approve_for_turn`) never answers one, and
    `approval/respond` records no scope from one (the decision applies to
    this call only). Otherwise a single "always" would approve every later
    call of the tool on the session, whatever its arguments and whoever
    started the run. The other approval bridges honour the flag as well: the
    `octos chat` requester neither auto-resolves a once-only request from a
    session "always" nor records one from it, and the approved-tool replay
    path refuses it.
  - The person-absent approvals of `confirm: app` and `confirm: host` tools
    are raised on the peer's own session (`peer-<slug>`); the host surfaces
    them in the app's conversation (for native modules, OctoSense's
    `crates/app-peers` broker does).
- **One execution per occurrence.** Every non-`read` call is claimed before
  any approval or host call under
  `<session>/<turn>/<tool_call_id>/<argument digest>` (the `peer_send_input`
  occurrence shape plus a SHA-256 of the arguments, whose object keys are
  sorted, so key order does not matter). A re-dispatch of the same call is
  refused (`duplicate`): it neither asks again nor reaches the host again. A
  provider that reuses tool-call ids (`call_1`) with other arguments is a
  different occurrence. `read` calls may repeat. Claims are kept 24 h and
  only expired claims are evicted: when 4 096 unexpired claims are held, a
  new non-`read` call is refused (`busy`, host_busy) rather than forgetting
  a claim.
- **No retry after an unknown outcome.** A non-`read` call whose outcome is
  unknown — it timed out, or its turn was interrupted while the host was
  working on it — marks `(session, tool, argument digest)` for 24 h. The same
  call in that session, in any later turn and under any tool-call id, is not
  sent unless the person approves it through an approval whose text says the
  earlier outcome is unknown (`approved_after_unknown`, which clears the
  mark); without an approval channel it is refused (`outcome_unknown_before`).
- **Routing and waiting.** At most 16 calls in flight per peer
  (`host_busy`). A call waits `call_timeout_ms`; a `confirm_required` call
  waits the approval TTL instead (the app's sheet may take as long as an
  approval would), and a gated call's wait extends to the approval TTL on an
  `awaiting_confirmation` acknowledgement (an acknowledgement of a call that
  is not gated is refused, `peer_tool_ack_not_gated`). A call waiting on the
  person holds one of the peer's 16 slots for as long as `approval_ttl_secs`
  (up to 7 days); hosts that confirm slowly should keep the TTL short.
  `peer/tool/result` takes the call out of the pending set and hands its
  result to the waiter under one lock, and the waiter at its deadline takes
  the call out under the same lock before looking for a result: either the
  host's answer wins and is delivered (never reported unknown while the
  host was told `accepted`), or the deadline wins and the answer is refused
  and audited as late. When the deadline wins the kernel sends
  `peer/tool/cancel`, and:
  - a `read` call ends with `timeout`;
  - any other call ends with `outcome_unknown`: the model is told the app may
    or may not have acted and must not retry, but check with a read tool or
    ask the person. The audit outcome is `unknown`.
- **Audit.** Every call, including refused ones, appends one JSON line to
  `peers/<slug>/tool_audit.jsonl`: `ts, peer, context_id, session_id,
  turn_id, tools_version, tool, tool_call_id, risk, decision, outcome,
  duration_ms, args_bytes, result_bytes`. `decision` is one of `allowed`,
  `approved`, `approved_after_unknown`, `app_confirms`, `denied`, `expired`,
  `approval_unavailable`, `duplicate`, `busy`, `outcome_unknown_before`,
  `not_background`, `invalid_args`, or `late_result` (a host
  answer after the kernel stopped waiting, with its `call_id`); `outcome` is
  `ok`, `error:<kind>`, `unknown` or `not_called`. Arguments and results
  themselves are not logged. The file is capped at 16 MiB: at the cap one
  `audit_full` marker is written and later rows are dropped; the host owns
  rotation.

## One declaration source: `tools.json`

The app bundle's `tools.json` (checked and pinned by App Hub at admission) is
the ONE declaration of an app's tools, for native modules and script apps
alike; its entries are the `tools` of `peer/tools/register` as they are.
Whoever owns the host-owned app peer registers them: for a native module
that is OctoSense's `crates/app-peers` broker, which creates the peer with
`peer/prepare`, holds its host token, registers the pinned `tools.json`
when it opens (or resumes) the peer and after every App Hub update, and
executes each `peer/tool/call` with the calling session's identity. A module
never declares its tools a second way.

## Non-goals and follow-ups

- **Shareable tools / other callers.** Tools of other apps, granted and
  marked `shareable`, and calls from the system agent are not part of this
  change; a peer's set only offers its own tools.
- **Presence beyond request contexts.** Attended means "an open request
  context with an approval bridge". A host that wants a finer signal (the
  app window focused, the screen on) would need a per-turn flag; not in this
  change.
- **Full JSON Schema validation** of arguments and of results against
  `output_schema` stays with the host; the kernel checks shape, `required`
  and size.
- **Durable host routing across restarts.** The route is in memory; after a
  kernel or host restart the host re-registers (the set itself is durable, so
  enforcement never lapses in between — the peer's turns get no tools).
- **A durable approval park for unattended runs.** Background runs woken by
  events (News M3) run through the host's AppUI connection and get the
  normal approval bridge. A runner with no client at all refuses gated tools
  today; parking those for a later client is a follow-up.
- **Audit rotation.** `tool_audit.jsonl` stops at 16 MiB; rotating it is the
  host's, as for transcripts.
- **Paths that do not build the turn registry here** are not filtered by the
  set: `skill/action/invoke` on a peer session (client-driven),
  `review/start` (its own review agent), `octos chat`, and the gateway's
  in-process peer inbox (a `peer_send_input` delivered to a gateway actor).
  Host-owned peers are serve peers and are not driven through the gateway;
  refusing host-owned peers on that path explicitly is a follow-up.
- **Host-chosen bindings (UPCR-2026-034).** The peer's `cwd` is validated
  like a session open but not restricted further (a host could bind `$HOME`
  or the profile dir), and the memory namespace is only syntax-checked, not
  tied to an app id. Restricting both is a follow-up.
- **Schema enforcement.** The kernel checks only an object shape and
  `required`; `additionalProperties`, types and formats are the host's to
  enforce.
- **Reconnects.** In-flight calls follow the route: a host that reconnects
  and registers again with the same token receives later calls and can
  still answer earlier ones.
- **End-to-end turn test.** The enforcement is exercised on the registry a
  turn uses (`apply_session_host_tools` after the session's own roster); a
  test that drives `run_standalone_turn` with a scripted model is a
  follow-up.

## Tests

- `peer_host_tool` unit tests (octos-agent, 11):
  `should_run_read_and_act_tools_without_approval`,
  `should_run_destructive_only_after_an_explicit_approve_with_the_exact_arguments`
  (the request is once-only),
  `should_never_call_the_host_when_approval_is_declined_expired_or_unavailable`,
  `should_gate_an_outward_act_tool_like_destructive`,
  `should_raise_one_approval_per_occurrence` (and a reused id with other
  arguments is a new call),
  `should_digest_arguments_independently_of_key_order`,
  `should_send_an_act_call_once_but_let_reads_repeat`,
  `should_report_an_unanswered_act_call_as_unknown_not_failed` (no resend
  without an approval stating the unknown outcome; approved → resent),
  `should_let_the_app_confirm_when_the_person_is_present`,
  `should_ask_for_a_kernel_approval_when_an_app_confirmed_tool_runs_without_the_person`,
  `should_refuse_foreground_tools_unattended_and_bad_arguments`
- `agent::execution` (octos-agent, 1): `should_never_auto_approve_a_once_only_request`
- `peers::host_tools` unit tests (5):
  `should_validate_names_schemas_and_collisions`,
  `should_accept_a_tools_json_entry_and_refuse_unknown_fields`,
  `should_evict_only_expired_claims_and_refuse_when_full`,
  `should_deliver_a_result_that_lands_at_the_deadline` (paused time),
  `should_deliver_a_result_taken_before_the_deadline_but_delivered_after_it`
  (an injected pause between taking the call and delivering it; fails on
  the previous ordering)
- `peer_host_tools_tests` (octos-cli, real profile runtime and sessions, 21):
  `should_advertise_and_dispatch_the_peer_tool_methods`,
  `should_refuse_a_registration_without_the_host_token`,
  `should_offer_exactly_the_registered_tools_and_refuse_an_unlisted_one`,
  `should_route_an_app_tool_call_to_the_host_and_back`,
  `should_time_out_and_cancel_a_call_the_host_never_answers` (late result
  audited),
  `should_run_a_destructive_tool_only_after_the_persons_approval` (includes
  the system agent's `peer_respond` being refused),
  `should_not_run_a_declined_or_expired_destructive_call_nor_ask_twice`,
  `should_replace_the_tool_set_atomically_and_refuse_a_stale_version`,
  `should_let_the_app_confirm_when_the_person_is_in_the_app_and_ask_otherwise`,
  `should_report_an_unanswered_act_call_as_unknown_and_never_resend_it`
  (also in a later turn),
  `should_treat_a_call_interrupted_while_the_host_worked_as_unknown`,
  `should_wait_for_the_apps_confirmation_sheet_instead_of_timing_out`,
  `should_never_answer_or_remember_a_host_tool_approval_by_scope`,
  `should_give_no_tools_to_a_session_on_a_foreign_base_key`,
  `should_give_no_tools_to_another_connection_on_the_hosts_base_key`,
  `should_give_no_tools_to_a_kernel_internal_continuation`,
  `should_keep_a_host_tool_approval_and_turn_controls_on_the_host_connection`
  (live forwarding filtered, `approval/respond` and `turn/interrupt` /
  `turn/steer` refused from another connection, the host answers),
  `should_clamp_host_filesystem_access_for_a_bound_app_session`,
  `should_rebuild_a_session_runtime_cached_before_the_peer_was_bound`
  (fails without the cache re-check),
  `should_refuse_generic_tools_that_escape_the_set` (the allowlist, schema
  shapes, and per-turn stripping of a stale set),
  `should_refuse_an_awaiting_confirmation_ack_for_a_call_that_is_not_gated`
- `spec_section6_catalog_lists_every_advertised_method`
