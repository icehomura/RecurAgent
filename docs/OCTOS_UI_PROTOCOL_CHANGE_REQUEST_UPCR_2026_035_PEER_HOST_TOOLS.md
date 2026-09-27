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
| `input_schema` | JSON Schema object (`"type": "object"`). ≤ 16 KiB. |
| `output_schema` | Optional, same rules. Stored and echoed; not enforced by the kernel. |
| `risk` | `read`, `act` or `destructive`. |
| `background` | May run in a turn with no interactive client. Default `false`. |
| `outward` | Reaches past the app (send, post, share, buy). Gated like `destructive`. |
| `confirm` | `host` (default) or `app`: who confirms a gated call with the person. Independent of `risk`. |
| `shareable` | App Hub metadata; accepted, not acted on yet (see follow-ups). |

`generic_tools` names kernel tools the app may use (`deep_search`, `read_file`,
…); names that do not exist in a turn's registry are simply absent. Limits:
64 app tools, 32 generic tools, 2 KiB per description. Options are clamped:
`call_timeout_ms` 1–300 000 (default 30 000), `approval_ttl_secs` 1–604 800
(default 3 600), `max_result_bytes` 1–1 048 576 (default 262 144).

The set **replaces** the previous one whole; `version` increments on every
registration (the first is 1). With `if_version`, a registration whose
expected version is not the current one is refused
(`peer_tools_version_conflict`, `data.current_version`). The set is durable
(`peers/<slug>/host_tools.json`) and applies from the next turn start. The
registering connection becomes the peer's **tool host**: every later
`peer/tool/call` goes to it (a later registration from another connection
moves the route).

Typed `data.kind`: `peer_host_token_mismatch`, `peer_originator_mismatch`,
`peer_not_found`, `peer_not_host_bound`, `peer_closed`, `peer_tools_invalid`,
`peer_tools_version_conflict`.

### `peer/tool/call` (server → host notification)

```
{peer, session_id, context_id, turn_id, call_id, tool_call_id, name, args,
 risk, confirm_required, timeout_ms, tools_version}
```

`session_id` / `context_id` identify the calling session (the peer's own, or
one of its request contexts): the caller's identity for the host service.
`name` is the declared name (`news.list`). `confirm_required` is true when
the app must confirm the call with the person itself (`confirm: app`, person
present); it is false when the kernel already holds the person's approval,
so the person is never asked twice. The
notification is ephemeral: if the host connection is gone the call fails
with `host_unavailable`; it is never replayed.

### `peer/tool/result`

```
{session_id, peer, host_token, profile_id?, call_id, ok,
 data?, error?: {kind?, message} | string}
→ {call_id, accepted: true, result_too_large?: {bytes, max}}
```

`data` above `max_result_bytes` (serialized) is not given to the model: the
call ends with `result_too_large`. An error message is capped at 4 KiB.
A result for a call that finished, timed out or was cancelled is refused
(`peer_tool_call_not_found`).

### `peer/tool/cancel` (server → host notification)

`{call_id, reason}`, `reason` = `timeout` (no result within `timeout_ms`) or
`cancelled` (the turn was interrupted while the call was in flight).

## Enforcement

For a session whose topic is `peer-<slug>` of a host-owned peer with a
registered set, or any `peerctx-<slug>.<context>` of it, every turn start:

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
  - One approval per occurrence: the kernel claims
    `<session>/<turn>/<tool_call_id>` (the `peer_send_input` occurrence
    shape) before raising an approval; a re-dispatch of the same occurrence
    is refused (`duplicate`) instead of asking twice.
- **Routing.** At most 16 calls in flight per peer (`host_busy`); each call
  waits at most `call_timeout_ms` (`timeout`, then `peer/tool/cancel`).
- **Audit.** Every call, including refused ones, appends one JSON line to
  `peers/<slug>/tool_audit.jsonl`: `ts, peer, context_id, session_id,
  turn_id, tools_version, tool, tool_call_id, risk, decision, outcome,
  duration_ms, args_bytes, result_bytes`. `decision` is one of `allowed`,
  `approved`, `app_confirms`, `denied`, `expired`, `approval_unavailable`,
  `duplicate`, `not_background`, `invalid_args`; `outcome` is `ok`,
  `error:<kind>` or `not_called`. Arguments and results themselves are not
  logged.

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
  enforcement never lapses in between — calls fail with `host_unavailable`).
- **A durable approval park for unattended runs.** Background runs woken by
  events (News M3) run through the host's AppUI connection and get the
  normal approval bridge. A runner with no client at all refuses gated tools
  today; parking those for a later client is a follow-up.
- **Audit retention.** `tool_audit.jsonl` grows without rotation; the host
  owns retention of the peer dir, as for transcripts.
- **Skill actions** (`skill/action/invoke`) on a peer session are
  client-driven and are not filtered by the set.

## Tests

- `peer_host_tool` unit tests (octos-agent):
  `should_run_read_and_act_tools_without_approval`,
  `should_run_destructive_only_after_an_explicit_approve_with_the_exact_arguments`,
  `should_never_call_the_host_when_approval_is_declined_expired_or_unavailable`,
  `should_gate_an_outward_act_tool_like_destructive`,
  `should_raise_one_approval_per_occurrence`,
  `should_let_the_app_confirm_when_the_person_is_present`,
  `should_ask_for_a_kernel_approval_when_an_app_confirmed_tool_runs_without_the_person`,
  `should_refuse_foreground_tools_unattended_and_bad_arguments`
- `peers::host_tools` unit tests: `should_validate_names_schemas_and_collisions`,
  `should_accept_a_tools_json_entry_and_refuse_unknown_fields`
- `peer_host_tools_tests` (octos-cli, real profile runtime and sessions):
  `should_advertise_and_dispatch_the_peer_tool_methods`,
  `should_refuse_a_registration_without_the_host_token`,
  `should_offer_exactly_the_registered_tools_and_refuse_an_unlisted_one`,
  `should_route_an_app_tool_call_to_the_host_and_back`,
  `should_time_out_and_cancel_a_call_the_host_never_answers`,
  `should_run_a_destructive_tool_only_after_the_persons_approval` (includes
  the system agent's `peer_respond` being refused),
  `should_not_run_a_declined_or_expired_destructive_call_nor_ask_twice`,
  `should_replace_the_tool_set_atomically_and_refuse_a_stale_version`,
  `should_let_the_app_confirm_when_the_person_is_in_the_app_and_ask_otherwise`
- `spec_section6_catalog_lists_every_advertised_method`
