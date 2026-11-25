# Plugin runtime: isolated QuickJS VMs

The embedded Magpie plugin runs inside the application, with one dedicated OS
thread and an independent QuickJS runtime per active plugin. It never runs in
NSH's UI VM. There is no plugin subprocess, downloaded runtime, runtime workspace,
filesystem module loader, installation scan, market, import action, or migration
of old plugin installations.

## Module map

| Previous responsibility | Current implementation |
| --- | --- |
| TypeScript evaluation in an external executable | `plugin_compile.rs` (build time), `vm/runtime.rs` (runtime) |
| `definition.rs`, temporary SDK/workspace, cleanup | `vm/collect.rs`: fresh in-memory collector VM |
| `builtin.rs` source materialization | generated static module table; embedded manifest/icon |
| `catalog.rs` path discovery | embedded manifest, module-id lookup, existing descriptor validation |
| child stdin/stdout and stderr readers | `worker.rs` session orchestration; VM channel reader |
| capability frames and response multiplexer | `vm/natives.rs`, `vm/host_module.rs`, `sdk/host.ts` |
| runtime global objects | `vm/globals.rs`, `vm/timers.rs` |
| runtime filesystem resolver | `vm/loader.rs`: table-only `Resolver` / `Loader` |
| resource permissions, requests, streams | `host_calls.rs` (unchanged network policy) |
| protocol adaptation and normalized request projection | existing `adapters/` and `wire.rs` |

## Compilation and module names

`build.rs` still generates the Cursor protobuf bindings. It also compiles
`plugins/build-in/magpie/**/*.ts` and `src/plugins/sdk/**/*.ts` using oxc 0.159:
Parser → SemanticBuilder with enum evaluation → Transformer with default target
(no syntax downleveling) → Codegen. Diagnostics fail the build. Relative imports,
exports and literal dynamic imports have their `.ts` suffix rewritten in the AST;
ordinary string data is not rewritten. JS is emitted under `OUT_DIR/plugins`, and
a generated Rust table uses `include_str!` for every emitted module. No bytecode,
source maps or original TypeScript source modules are embedded in the application.

The loader maps `cursor-byok-next:plugin`, `:provider`, `:model`, `:resource`,
and `:host` to `sdk/*.js`. Magpie's manifest entry is `magpie/main.js`.
Only table entries resolve, including dynamic imports; unknown bare specifiers,
`node:*`, filesystem paths, external URLs and root traversal fail closed.
The SDK's type-only modules still compile into valid empty ESM modules.

oxc erases TypeScript syntax; it is **not** a full TypeScript type checker.
`cargo build` rejects parser/semantic/transform diagnostics, while `nsh check`
provides the separate TypeScript type-check gate. A type mismatch that is valid
syntax need not fail oxc. Tests explicitly preserve this distinction.

## VM and native capability boundary

The app pins `quickjs-jit = 0.14.2`, imported as `rquickjs`, with default features
off and explicit `std` / `loader`. It does not request `jit-abi`. NSH's generated
host may enable additional features on that same package through Cargo feature
unification; that does not install its UI modules into a plugin context.

Each VM owns a custom allocator with a 64 MiB ceiling (including aligned backing
allocation headers), a 512 KiB JS stack ceiling, and a 5-second thread-CPU budget
for each entry into JS. A microtask drain shares one budget across all its jobs,
so recursively scheduling tiny jobs cannot evade the limit. Unix uses
`CLOCK_THREAD_CPUTIME_ID`; Windows uses `GetThreadTimes`, not elapsed HTTP time.
A stop or CPU interrupt is fatal even if script code attempts to catch it. A heap
refusal also discards the generation. No interrupted engine state is reused.
OOM can surface as an engine exception or an unavailable exception object; the
contract is bounded memory, safe failure and no host crash, not an unconditional
promise that the exhausted JS heap can allocate a new `RangeError`.

Module evaluation returns a Promise. Import failures, top-level throws and TLA
rejections are checked, their exception value consumed, and only a safe host error
is exposed. Module initialization also has a 10-second wall-time ceiling for
unsettled TLA. The collector and worker use distinct runtimes and module registries.

The only native capability bridge accepts invocation ids and JSON strings:

| SDK operation | Rust destination |
| --- | --- |
| fetch | `HostContext::call("network.fetch")` |
| streamOpen | `HostContext::call("network.stream.open")` |
| streamRead | `HostContext::call("network.stream.read")` |
| streamClose | `HostContext::call("network.stream.close")` |
| protocolStream | `HostContext::call("protocol.stream")` → existing adapters |
| readRequest | synchronous `HostContext::read_request` → existing `wire::llm_request` |

The author's `input.readRequest(): Promise<LlmRequest>` API remains unchanged:
the native read is synchronous, and the SDK caches its parsed result in one
Promise. Request bodies do not enter invocation frames. Protocol adapter events
go directly to the invocation's Rust stream, not through JS.

A native first checks the live invocation, then spawns actual I/O on the captured
Tokio runtime handle. It never calls `block_on`. Neither JS values nor Promise
resolvers cross threads. Owned JSON/error/status completion data returns through
a separate HostDone queue; only the VM thread resolves/rejects Promises.
Per-invocation concurrency is eight, including completed-but-unsettled calls;
the ninth immediately rejects with `too many concurrent host calls`.

`host_calls.rs` remains the policy authority: manifest network hosts and resource
origin, credential-bearing URL rejection, no redirects, existing request/response
limits, cancellation and exactly-once CallRecorder ownership. Stream ids are
bound to their owning invocation; at most 16 streams per invocation remain open.
Closing a stream cancels its child token, not its entire invocation. Invocation
completion/cancellation revokes its remaining capabilities and stream readers.
HTTP error status survives the bridge; arbitrary upstream response text does not.
Catalog failures retain old cached models, with only host-observed status shown.

## Transport and shutdown

Host → VM: complete JSON `request` or `cancel` frames. VM → host: complete JSON
`result` or `event` frames. Field names and ordering remain stable. Legacy
`host_call` samples can still be parsed for diagnostics, but received production
frames are rejected and never dispatched to the host.

The request queue holds 32 frames. VM output holds at most 4096 frames, each
limited to 16 MiB; a saturated/oversized output is fatal instead of blocking the
VM inside a native send. Each invocation's Rust event queue holds 4096 items,
and each plugin accepts at most 16 concurrent invocations. Native completion
results consume none of those session slots. The SDK retains trailing JSONL newline
serialization for production request/result interoperability, although the queue
already preserves complete frame boundaries.

The VM checks stop, receives a session frame with a bounded wait, synchronously
dispatches it, settles native completions, drains microtasks, and runs due timers.
A maximum 5 ms idle wait prevents a HostDone arriving without a request/timer from
being stranded in a different queue. No active HTTP wait consumes JS CPU budget.

`stop()` sets the atomic stop flag, revokes pending invocations and joins the
thread through Tokio's blocking pool. Drop requests stop and schedules the join
without blocking an async executor worker. The 60-second invocation timeout kills
the entire generation; the next connection creates a new independent runtime.
Old generations have their own completion queues, so late results cannot settle
Promises in a replacement VM. No runtime-directory cleanup thread remains.

## Global objects and verified engine assumptions

The minimal URL object delegates parsing to Rust `url` and exposes href, protocol,
username, password, hostname and origin. Rust origin normalization and permission
checks remain authoritative. AbortController/Signal support an aborted flag and
abort listeners, with idempotent abort. Timers are bounded to 1024 registered
callbacks per VM; clearTimeout and clearInterval remove handles. Console methods
log with the `plugin` tracing target.

Engine probes cover JSON, Promise, queueMicrotask, WeakRef, atob/btoa,
Symbol.asyncIterator and undefined `import.meta.url`. Async generators remain
used by the real SDK stream reader. No ambient fetch, WebSocket, filesystem,
Node process, require, structuredClone or NSH module is provided.

## Requirements and test evidence

| Invariant | Tests / gate |
| --- | --- |
| no external runtime or workspace | catalog child-process test with empty PATH and temporary HOME; installed-source shadow test |
| compiler preserves modern JS, erases TS and rewrites only module sources | `tests/plugin_compile.rs` |
| parser/transform diagnostics fail closed | compiler malformed-source test and build script errors |
| isolated module registries | VM namespace and loader static/dynamic import probes |
| required globals and absent ambient capabilities | `engine_intrinsics_and_capability_absence`, URL/abort/timers probe |
| CPU and recursive microtask budgets | VM infinite-loop and recursive microtask tests |
| bounded heap; fatal generation can be replaced | heap exhaustion and CPU/new-VM probes |
| request/event/result shape | protocol/session golden tests and unchanged LAST_INVOKE_FRAME body-size regression |
| native concurrency and network refusal | ninth-call rejection; origin/permission tests; forged legacy host_call never reaches a listening fake gateway |
| synchronous native request with unchanged author API | SDK request projection count and concurrent/cancelled invocation tests |
| descriptor/live catalog behavior | full Magpie fake-gateway integration, including deduplication/media/IDs/503 fallback/redaction/resource changes/corrupt storage |
| protocol model behavior and recorder semantics | unchanged adapters, file/image, HTTP 401, completion and cancellation record tests |
| VM restart | stop/reconnect and fatal CPU/heap → automatic generation rebuild regressions |
| cancellation revokes native I/O and marks Signal | cancellation signal / rejected Promise / active task count zero regression |
| per-invocation stream ownership, 16-stream limit, EOF cleanup | network stream ownership/limit/EOF task-drain regression |
| initialization errors visible without private causes | collector failure snapshot regression (`vm_failed`) |

The PRD contains contradictory instructions to both keep and delete `collect.ts`.
This implementation keeps it as the compiled collector entry: the independent VM
and descriptor validation require that path. The old runtime-specific SDK config
is removed; the app tsconfig retains SDK mappings, includes and exclusions.
The literal search `rg -i bun` also matches the runner name `ubuntu`; removal of
the external runtime is checked using word/identifier-aware matches, not that
substring. Historical documents can still name the previous runtime.

## Validation boundaries

Run fmt, strict clippy, workspace tests, `nsh check`, `nsh build`, `nsh test`, and
the repository release-script checks. The CLI must match the checked-out NSH
version (currently 0.6.1). Check both root and generated-host dependency graphs
for one quickjs-jit-sys version.

Only temporary stores and local fake gateways are permitted in automated tests.
Actual Cursor takeover and real Magpie text/thinking/tool/usage/cancellation need
separate authorized acceptance on a user's setup. Linux test evidence is not
macOS/Windows build or appearance evidence; those platform packages remain CI
validation, and this document does not claim they were run locally.

### Local validation record

Linux validation uses the repository's NSH 0.6.1 CLI and temporary fake gateways.
The strict clippy Rust checks pass; Cargo's separate manifest diagnostic still
reports the existing `nsh-base` dependency as apparently unused (it is consumed by
generated NSH code). The UI tests deliberately exercise an unavailable AppKit
appearance change and log that expected failure. The macOS-only visual test is
skipped on Linux; it is not counted as a visual acceptance pass.

The old global `LAST_INVOKE_FRAME` test hook could be overwritten by concurrent
workers. Its data now belongs to each worker, preserving the frame-size/body
regression without depending on serial test execution.
