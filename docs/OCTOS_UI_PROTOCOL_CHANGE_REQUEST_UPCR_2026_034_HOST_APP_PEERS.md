# Octos UI Protocol Change Request: Host-Owned App Peers

## Header

- Request id: `UPCR-2026-034`
- Date: 2026-09-27
- Target protocol: `octos-ui/v1alpha1`
- Status: implemented
- Scope: additive `peer/prepare` fields; three additive raw AppUI methods,
  `peer/model/set`, `peer/context/open`, `peer/context/close`; session-level
  enforcement of app bindings and app/account memory namespaces
- Origin: Rinx ADR 0007, "Host-owned Octos app peers and Rinx deployment
  modes" (OctoSense shells host apps such as Rinx on one shared kernel)

## Problem

A host (an OctoSense shell) runs one kernel and one provider profile per user
runtime and launches apps that need the assistant. Each authorized app should
get ONE peer owned by the host's system agent, which the system agent can
talk to for the app's lifetime. The existing peer primitives stop short:

1. `peer/prepare` could not select a configured model lane, although the
   model's `peer_handoff` tool can (`model: "strong"`), and it could only
   refuse a name that already exists — a host reopening an app could not
   resume its peer.
2. A `ProfileRuntime` owns memory, recall and the episode store as well as
   the provider. Peers with different working directories still captured
   into, and were injected from, the same profile memory, so one app's
   history reached every other app and the system agent's private memory
   reached every app.
3. An app that hosts smaller clients of its own (Rinx's mini apps) had no
   way to keep their requests apart except ordinary sessions with a full,
   unrelated agent identity.

## Contract

Discovery: a server that lists `peer/context/open` in
`config/capabilities/list` `supported_methods` implements this whole UPCR.
Older servers silently ignore unknown `peer/prepare` fields, so a host MUST
check discovery (and that the result echoes `memory_namespace`) before
relying on a binding. All methods are raw-surface methods: session-scoped
(session-ingress) connections cannot call them, and every one is profile
scoped like `peer/prepare`.

### `peer/prepare` (additive)

| Field | Meaning |
| --- | --- |
| `model?` | Configured `sub_provider` lane key, as `peer_handoff`'s `model`. Unknown lane: `model_note`, primary model. |
| `memory_namespace?` | Marks a **host-owned app peer**. Requires `session_id` (the owning system-agent session, persisted as the originator), exactly one name, and no `worktree`. `cwd` names the app's host-owned workspace (validated like a session open); without it the kernel provisions `<data_dir>/app-workspaces/<namespace segments>` — the path a remote host uses, since its local paths mean nothing to the kernel. Grammar: 1–8 `/`-separated segments of `[a-z0-9][a-z0-9._-]{0,63}`, at most 200 bytes. |
| `resume?` | With `memory_namespace`: if the named peer exists with the same originator, namespace and workspace, return it (`resumed: true`) instead of refusing the name. A `model` given on resume updates the lane. |

Result entries add `model` (`{lane, provider?, model?}` — the effective
model; `{lane: "primary"}` otherwise; never credentials), `model_note`,
`memory_namespace` and `resumed`. Typed `data.kind`:
`peer_originator_mismatch` (permission denied), `peer_binding_mismatch`,
`peer_closed`.

The binding (`peers/<slug>/host_binding.json`) is written durably BEFORE
`brief.md`, the peer's visibility gate, so a peer is never visible unbound.

### `peer/model/set`

`{session_id, peer, model: string|null, profile_id?}` →
`{slug, profile_id, model, applies: "next_turn"}`. Originator only. Sets or
clears the peer's lane; the lane is read at each turn start, so a change
applies between turns. An unknown lane is refused (`peer_model_unknown`,
with `available`) and nothing changes. The profile default model, the
profile's lanes and all credentials are untouched.

### `peer/context/open`, `peer/context/close`

A **request context** belongs to a host-owned app peer: a separate
transcript, a workspace inside the peer's, and a child memory namespace. It
is not an agent: it cannot hand off peers, has no blackboard entry and runs
on its peer's model lane.

`peer/context/open {session_id, peer, context_id, cwd?, profile_id?}` →
`{session_id, topic, slug, context_id, cwd, memory_namespace, model,
profile_id, created}`. Originator only; `context_id` is
`[a-z0-9][a-z0-9-]{0,63}`. The session key is minted by the kernel:
`<originator base key>#peerctx-<slug>.<context_id>`. `cwd` defaults to
`<peer cwd>/contexts/<context_id>`; an explicit `cwd` must lie strictly
inside the peer's workspace (`peer_context_workspace_escape`). The memory
namespace is `<peer namespace>/ctx-<context_id>`. Idempotent while open.

`peer/context/close {session_id, peer, context_id}` →
`{session_id, slug, context_id, profile_id, closed, was_open, interrupted}`.
Writes the closed marker first, then interrupts the context's in-flight turn
(`turn/error`: "interrupted by peer/context/close"). The transcript and
workspace stay on disk; the host owns retention. A closed id is never
reopened (`peer_context_closed`); hosts mint a new id per client generation.

Other kinds: `peer_not_found`, `peer_not_host_bound`,
`peer_context_not_found`, `peer_context_namespace_too_long`.

### Session enforcement

For a session whose topic is `peer-<slug>` of a host-owned peer, or any
`peerctx-` topic:

- **Workspace**: the session runs in the bound workspace. `session/open`
  without `cwd` gets it; a different `cwd` is refused.
- **Refusal**: a closed peer, a closed context, a never-opened context or a
  malformed `peerctx-` topic cannot bootstrap, and every `turn/start` on a
  cached runtime re-checks the binding (`session_binding_closed` terminal).
- **Memory namespace**: capture (`save_memory`, episodes), retrieval
  (`recall_memory`, `memory_search`, `memory_load`), the automatic memory
  prompt segment and episodic recall use stores rooted at
  `<data_dir>/memory-namespaces/<segments…>`. The namespace sees neither the
  profile's own memory nor another namespace's. `memory_note` (whose notes
  only the profile-level consolidator serves) and `run_pipeline` (which
  captures into the profile's memory) are not offered to namespaced
  sessions, and spawned children inherit the namespaced episode store.
- **Background extraction**: the profile's memory-refresh sweep never reads
  a bound session's transcript.

Ordinary sessions and agent-staged peers are unchanged.

## Non-goals and conservative defaults

- **Permission prompts.** Approvals keep their existing policy: an app
  peer's tool approval is raised like any session's and is answered by the
  person through the host. The system agent's ability to answer a peer's
  ordinary question is not authority to approve a tool; nothing here
  auto-approves.
- **Background work after close.** Closing a request context interrupts its
  work; nothing in this UPCR keeps a context running. A host-owned peer
  survives its app's UI closing (the host owns its lifecycle and may close
  it with the existing `peer_close` path).
- Per-app token/tool budgets beyond the existing `token_budget`, fair
  scheduling across apps, and exposing the namespaces through the
  memory-panel RPCs are not part of this change.

## Risk

- The binding is only as trustworthy as the raw surface: in the single-user
  profile model any client that can call `peer/prepare` can create bindings.
  Hosts must not hand raw OUP to untrusted apps (they broker requests).
- Namespace stores are cached per process by root (one redb open per file).

## Tests

- `should_stage_and_resume_a_host_owned_app_peer_with_a_persisted_system_originator`
- `should_reject_incomplete_host_bindings`
- `should_select_a_configured_model_for_one_peer_without_touching_the_profile_default`
- `should_isolate_app_peer_workspace_and_memory_from_the_system_and_each_other`
- `should_open_isolated_request_contexts_and_refuse_them_after_close`
- `should_provision_a_kernel_workspace_when_the_host_names_none`
- `should_advertise_and_dispatch_the_host_peer_methods`
- `should_never_extract_an_app_bound_session_into_the_profile_memory`
- `peers::app_binding` and `runtime::memory_namespace` unit tests
- `spec_section6_catalog_lists_every_advertised_method`
