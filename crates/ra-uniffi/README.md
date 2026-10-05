# ra-uniffi

Idiomatic **Python / Swift / Kotlin** bindings for embedding RecurAgent, generated
from a single Rust definition by [uniffi](https://mozilla.github.io/uniffi-rs/)
(v0.29).

It is a thin wrapper over the **native core** in
[`ra-ffi`](../ra-ffi) (`ra_ffi::RaRuntime`). All the real work —
provider construction, the agent loop, the embedder, and the **hardened
credential path** (single key resolution + pinning + secret-scrubbing of error
text) — lives in that one core, shared with the C-ABI. This crate only adds
idiomatic type marshalling, so there is exactly one place to audit for
credential handling.

## Surface

| Rust | Foreign |
|---|---|
| `Config` (record) | dict / data class of provider, model, key, cwd, … |
| `Brief` (record) | `{ prompt, max_iterations? }` |
| `TaskResult` / `TokenUsage` (records) | outputs |
| `RaError` (error enum) | exception (`Config`/`Provider`/`Run`/`Embed`/`NoEmbedder`/`Incomplete`/`Memory`) |
| `Runtime` (object) | `new(config)`, `run_task(brief)`, `embed(text)`, `memory_upsert(json)`, `memory_search(json)`, `memory_load(id)`, `memory_stats()` |
| `embedding_model_status(data_dir)` / `embedding_model_ensure(data_dir, download)` (free functions) | provision the default embedding model without a `Runtime` (JSON strings; same contracts as the C-ABI's `ra_embedding_model_*`) |

Methods are **synchronous**: the async agent loop is driven by a `block_on`
inside the core, so callers see plain blocking calls — call them from a normal
(non-async) thread. `Runtime` is `Send + Sync` and reference-counted, so it may
be shared across threads (concurrent calls contend on one internal executor).

## Build

```bash
cargo build -p ra-uniffi                      # cdylib + staticlib + rlib, with the
                                                 # in-process GGUF embedder (default
                                                 # feature `embed-llama`; needs cmake)
cargo build -p ra-uniffi --no-default-features  # pure Rust, keyword-only memory
```

## Generating bindings

Bindings are generated from the **built library** (proc-macro metadata, no
UDL) via the in-crate `uniffi-bindgen` binary — so the exact uniffi version the
crate compiled against is the one that generates:

```bash
cargo build -p ra-uniffi
# Use libra_uniffi.so on Linux.
cargo run -p ra-uniffi --bin uniffi-bindgen -- generate \
    --library target/debug/libra_uniffi.dylib \
    --language python --no-format \
    --out-dir crates/ra-uniffi/bindings/python

# Deterministic post-gen tidy so `git diff --check` stays clean (the generator
# emits some trailing whitespace + a trailing blank line):
perl -i -pe  's/[ \t]+$//'  crates/ra-uniffi/bindings/python/ra.py
perl -i -0pe 's/\n+\z/\n/'  crates/ra-uniffi/bindings/python/ra.py
```

Swift and Kotlin generate identically — swap `--language swift` (emits
`ra.swift` + a `.modulemap`) or `--language kotlin` (emits `ra.kt`). The
committed Python bindings live in [`bindings/python/ra.py`](bindings/python/).

## Python example

Put `ra.py` on the `PYTHONPATH` and the compiled library
(`libra_uniffi.dylib`/`.so`) where it can be loaded (uniffi looks it up by
name), then:

```python
from ra import Runtime, Config, Brief

rt = Runtime(Config(provider="openai", model="gpt-4o-mini", api_key="sk-..."))
print(rt.run_task(Brief(prompt="Reply OK")).output)
```

Optional `Config` fields default sensibly (`api_key_env`, `base_url`,
`api_type`, `cwd`, `allow_shell=False`, `max_iterations`,
`embedding_model_path`, `data_dir`, `recall_dimension`,
`embedding_auto_download`), so only `provider` and `model` (plus a
credential) are required. Set `api_type="anthropic"` (or `"responses"`) to
drive a `provider="custom"` Anthropic-compatible endpoint onto the right
protocol. Set `data_dir` to keep the episode + Recall memory stores — and the
default embedding model — on disk across runtimes (otherwise they live in a
scratch dir removed on drop).

Errors surface as an `RaError` exception; a failed provider build or run
carries a scrubbed message, and `embed` without an embedder raises
`RaError.NoEmbedder`.

### The default embedding model

An `embed-llama` build (the default) embeds with EmbeddingGemma-300M
(Q8_0 GGUF, 334 MB, [Gemma Terms of Use](https://ai.google.dev/gemma/terms)),
kept at `<data_dir>/models/embeddinggemma-300M-Q8_0.gguf`. When
`embedding_model_path` is unset, `Runtime(...)` loads it if it is there;
otherwise it downloads it first — **blocking the constructor** — unless
`embedding_auto_download=False` or `ra_NO_MODEL_DOWNLOAD=1` is set, in
which case the runtime is keyword-only (`embed` raises `NoEmbedder`; memory
search still works, BM25-only). The full resolution rules are in the
[`ra-ffi` README](../ra-ffi/README.md#the-default-embedding-model).
To own the download (first-run screen, Wi-Fi policy), provision before
constructing a runtime, with the two free functions:

```python
import json
from ra import embedding_model_status, embedding_model_ensure, RaError

status = json.loads(embedding_model_status("/data/ra"))
# {"path", "present", "bytes", "complete", "url", "license_url", "sha256"}
if not status["complete"]:
    try:
        path = json.loads(embedding_model_ensure("/data/ra", download=True))["path"]
    except RaError.Embed as error:   # download disabled/vetoed, or failed to verify
        ...
rt = Runtime(Config(provider="openai", model="gpt-4o-mini", api_key="sk-...",
                    data_dir="/data/ra", embedding_auto_download=False))
```

### Recall memory

The four `memory_*` methods take and return JSON strings with exactly the
contracts of the C-ABI's `ra_memory_*` functions (documented in the
[`ra-ffi` README](../ra-ffi/README.md#memory-the-recall-index)); a
failure raises `RaError.Memory` (e.g. `no such record`). No embedder is
needed — the index is BM25-only until one is configured:

```python
import json
rt.memory_upsert(json.dumps({"records": [
    {"id": "doc:mail:42", "kind": "document", "source": "mail",
     "timestamp": "2026-09-01T10:00:00Z", "title": "Dentist appointment",
     "abstract": "Sunrise Dental on the 24th", "fingerprint": "h42"}]}))
hits = json.loads(rt.memory_search(json.dumps({"query": "dentist", "limit": 5})))["hits"]
record = json.loads(rt.memory_load(hits[0]["id"]))["record"]
stats = json.loads(rt.memory_stats())
```

A provider `max_tokens` stop raises `RaError.Incomplete`, **not** a successful
`TaskResult`. Its `partial` field contains the actual output, accumulated token
usage, and iterations. The diagnostic string is fixed and short; the partial
is lossless task payload and is not passed through error redaction or the
600-byte diagnostic cap. Treat it as unfinished output, not as a final answer.

```python
from ra import RaError

try:
    result = rt.run_task(Brief(prompt="Explain the design"))
except RaError.Incomplete as error:
    unfinished_output = error.partial.output
    consumed_tokens = error.partial.tokens
    # Display/store as incomplete; do not report a successful final answer.
```

The error variant is appended, preserving existing variant ordinals. Regenerate
bindings together with the library to consume the new structured error; the
committed Python binding is generated from this version's library metadata.

Offline ABI regression (real C exports and generated Python, localhost fixture
with a fake key only; both incomplete and successful controls):

```bash
cargo build -p ra-ffi -p ra-uniffi
python3 crates/ra-uniffi/tests/incomplete_bindings.py --library-dir target/debug
```

## Credentials & safety

Credential resolution, pinning, and error-text scrubbing are inherited verbatim
from `ra-ffi` — see that crate's README for the full contract (an explicitly
supplied `api_key`/`api_key_env` wins over the `ra auth login` store; the key
is resolved once and pinned; do not pass a raw key beginning with `keychain:`).
As there, `tracing` debug/trace logging of provider error bodies is the host's
responsibility.

> Scratch cleanup: the core keeps a small episodic-memory scratch dir under the
> OS temp dir. It is owned by an RAII guard in the shared `ra-ffi` core that
> removes it when the last runtime is dropped (the guard is the final struct
> field, so it runs after the episodic store releases its redb lock). Both the
> C-ABI's `ra_runtime_free` and a native/uniffi drop reclaim it identically,
> so a long-lived host does not accumulate scratch dirs.

## Runtime contract checks

Run `./scripts/milestone-ci.sh oup-runtime` to build the native libraries,
compare generated Python bindings, compile the C header contract, and exercise
actual C/Python success and incomplete-result calls against a localhost fixture.
The same suite checks real chat, ACP and OUP subprocesses.
