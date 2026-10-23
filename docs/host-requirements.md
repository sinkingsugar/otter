# What a Production Host Needs: Gaps and Decisions

Status: proposal, 2026-10-10. Every **Proposal** below is a recommendation awaiting Giovanni's decision, except where a section records a **Decision**; nothing here is implemented. H1, H3, H4 and H6 were accepted on 2026-10-11; their implementation (Track A1) waits for the performance gates in [performance-gates.md](performance-gates.md). When a decision is taken, record it in its section (and in the document that owns the design, for example `values-and-types.md` for a new value), and move the work into a plan with milestones and gates as the golden path and the metaprogramming plan do.

The golden path and M8–M10 made the language and engine sound. This document lists what is still missing for a real application to run on 2.0, and the design questions that must be settled before the missing pieces are built. Several of them shape every module, so they come before any module port; otherwise each module invents its own handle, lifetime and threading rules.

## 1. Evidence

The list was derived from a read-only survey of a large production 1.x host: an application with a native (non-Rust) UI that embeds 1.x as a framework. It is a good model because it uses most of what a serious host needs. Aggregate numbers, so that the scale of each gap stays visible:

- **Scripts:** 312 files, about 32,000 lines (about 26,000 outside tests), 180 entry scripts, 171 of them started by name from host code.
- **Shards used:** 401 distinct, 14,730 call sites. 53 of them exist in 2.0 by name (13 percent of names, 54 percent of calls, almost all core basics). 10 more (1,500 calls) are replaced by 2.0 constructs (`Set`, `Ref`, `Do`, `And`/`Or`, `Sequence`/`Table`, `ForEach`/`Map`, `Detach`/`Step`). 338 (5,296 calls) are missing.
- **2.0 `check`:** 1 of 312 files passes as written. After a mechanical rewrite of parameter labels and the removed assignment operators, still 1, with 1,304 unknown-shard errors (163 distinct), 559 unsupported forms (`@define`, `@template`, `@if`, vector constructors, wire options) and the removed `Do` and `Set`.
- **Language forms:** `@include` 360, `@define` 292, `@template` 91, `@type` 183, `@if` 99, `@wire` 289, `#( )` 52, `@env` 18, `@platform` 28, vector and color constructors 93, implicit loop variables (`$0`…) 65; assignment operators `>=` 796, `>` 618, `>>` 236; `Global: true` 150; `Await` 209, `Detach` 37, `Spawn` 16, `Step` 12, `Branch` 7, `Expect`/`Wait` on wires about 50, `Produce`/`Consume` 26, `Events.*` 26; runtime parsing and composition (`Shards.Read`, `Shards.Distill`, `WireComposer`, a host-side checker) at about 20 sites, used for model-written tools and UI.
- **Host shards:** 169 shards written in the host language (68 UI views and modifiers, 37 3D scene shards, 16 embedded browser, 11 sign-in flows, plus PDF, calendar, scripting engine and system integrations). 26 files suspend from inside an activation with a blocking poll loop. The host binds 176 externals (host-owned variables visible to scripts) and polls them every tick to stream progress into the UI.
- **1.x modules linked:** about 40. Used in production: sqlite (with the cr-sqlite replication extension, FTS5 and sqlite-vec), http (client with streaming, and a server), llm (llama.cpp; whisper.cpp only in tests), fs, json, imaging, events/channels, crypto hashes, ssh, local shell, fileops, network (WebSocket), markdown, pdf, spreadsheet, brotli, bigint, random, candle (only UMAP in production), and an external Rust module wrapping a memory-indexing crate. Enabled but unused in production: anim, crdts, csv, os, reflection, run, snappy, struct, geo, audio.

The surveys' working files are not part of this repository; the numbers above are what they established.

## 2. Decisions for the core

### H1. Object values

**Need.** Every non-trivial module passes host resources through scripts: model and context handles, database connections, network and shell sessions, streams, workbooks, tensors, UI view nodes, scene entities, browser views, scripting contexts, memory indexes. 1.x has ref-counted object values with a registered type. 2.0's `Var` has none (deferred in `values-and-types.md` "until a ported script needs them"; this is that script). `prototype-shard-contract.md` already sketches the answer: objects are `Arc<dyn Object>` with a type id.

**Options.** (a) A `Var::Object` variant: typed, ref-counted, owned by values. (b) No variant; scripts pass string or integer keys into host services. Keys lose typing (compose cannot tell a model from a session), leak when a script forgets to release, and make every module invent its own registry.

**Proposal.** (a). `Var::Object(ManuallyDrop<Arc<dyn Object>>)` (a fat pointer, fits the 32-byte `Var`), dropped and cloned out of line like the other storage variants so the VM's code does not change (verify with hot-asm and the A/B suite).
- `trait Object: Any + Send + Sync` with a downcast accessor (`Var::as_object::<T>()`).
- Object types are registered in the type registry by name (`Llm.Model`, `Db.Connection`), so `Type::Object(id)` composes, prints and checks like any type; `Any` admits them.
- Equality and hashing are by identity; printing is the type name and an id. No literal syntax: compose-time evaluation refuses to produce an object, so an object can never land in `Compiled` (the rule that device- and connection-bound resources are resolved at run time).
- Objects live in values only, so a mesh variable, a sequence or a table can hold one; dropping the last value releases it.

**Open.** Thread affinity: `Var` is `Send + Sync` today, and async operations move owned values to the runtime's threads. A UI object that must only be touched on the main thread either stays `Send + Sync` by contract (the host asserts the thread on use, as the 1.x host already does) or 2.0 adds a non-`Send` object class, which would make `Var` non-`Send` and is not worth it. Proposal: the former.

**Decision (2026-10-11):** accepted as proposed. Objects stay `Send + Sync`; a host object with thread affinity asserts its thread on use.

### H2. Images, audio and the other deferred values

**Need.** Images appear in the imaging module, in LLM vision input and in host shards (decode, crop, resize, write, screenshots). Audio buffers in the audio module and speech recognition. Integer-keyed tables in one real script (numeric row order). Integer vectors and colors as `@i2`/`@f3`/`@color` constructors (93 sites).

**Proposal.**
- Image: an object type (`Image`, H1) carrying width, height, channels and pixel bytes, not a `Var` variant. It never needs VM operations, and an object keeps `Var` small.
- Audio: the same, deferred until the audio module is ported.
- Integer-keyed tables: not added. The one script rewrites to a sequence of rows; keys stay strings ("tables have string keys" is a settled 2.0 rule).
- Integer vectors: deferred until a ported script needs arithmetic on them; the constructors go to the language (H12).

### H3. Services

**Need.** Database connections per database, the model cache, the HTTP client pool, the async runtime and shell sessions are process- or mesh-wide resources that shards look up at activation. `otter-io`'s HTTP client cache is process-wide only because "the prototype has no mesh services yet" (its module header). 1.x uses per-mesh `AnyStorage`.

**Proposal.** A typed service map on the mesh: the host inserts services before running (`mesh.services_mut().insert(db)`), and shards fetch them at activation through `LeafCtx` (`ctx.service::<Db>()`), never in compose. A module provides a default service when the host inserts none. Objects (H1) are what scripts hold; services are what owns and pools the underlying resources (a `Db.Connection` object refers to its database in the `Db` service).

**Decision (2026-10-11):** accepted as proposed.

### H4. Cleanup on every exit

**Need.** A transaction must commit or roll back, and a UI container must dismiss, on every exit of its body: success, error, `Stop`, `Restart`, `Return` passing through, and cancellation. `ControlShard::State` is reset to `Default` on those exits and documented as holding no resource, and `resume` is not called when `Stop`, `Restart` or `Return` passes through.

**Options.** (a) An `exit(compiled, state, ctx, reason)` callback called on every exit that skips `resume`. (b) Allow guard values in `State`: the reset drops them, and their `Drop` does the cleanup (queueing a rollback to the database service, closing a dialog through the host).

**Proposal.** (b), because the reset already happens on every exit. Specify and test that it does, cancellation included, then relax the rule: `State` may hold guards whose `Drop` releases or schedules release and never blocks. (a) remains available if a cleanup ever needs `ctx`.

**Decision (2026-10-11):** accepted as proposed.

### H5. Concurrency: work off the scheduler, joining, detaching

**Need.** 1.x's `Await` moves an arbitrary sub-flow onto worker threads (209 sites: queries, JSON, files, model calls, image work). Scripts also start wires and wait for their results (`Expect`, `Wait`), detach long-running ones, step a wire manually (`Step`), and run child meshes (`Branch`). 2.0 has `AsyncShard` and a `Spawn` that passes its input through and returns no handle.

**Proposal.**
- **No generic `Await`.** Work that blocks becomes an async shard whose operation runs on `otter_io::runtime::spawn_blocking` (it exists) or as a task. Moving arbitrary script code to another thread contradicts the single-threaded engine and the frame model. Each 1.x `Await` site is rewritten to call the async form of its work (most wrap one query, one parse, one decode).
- **Instances as values.** `Spawn` outputs an instance handle (an object, H1). `Wire.Wait(handle)` suspends until the instance finishes and outputs its result or fails with its error; `Wire.Stop(handle)` cancels it.
- **Structured by default.** A spawned instance belongs to its spawner and is cancelled when the spawner ends. `detach: true` gives it to the mesh. This replaces `Detach` and most of `Branch`. A separate child mesh is not needed until a host shows a case that instance groups cannot express.
- **`Step`** (advance another wire by one iteration from this one): not provided; the survey's uses are UI ticking, which the host drives.
- **Channels** (`Produce`/`Consume`) and events (`Events.Send/Receive/Update`): a mesh service with async receive (H3), not a core construct.

**Decision (2026-10-11):** accepted in this form, after a review of 1.x's wire shards (`shards/modules/core/wires.cpp`) and Astra's design review:
- 1.x's names: `Spawn(wire)` outputs a handle (a `Wire.Instance` object, H1), replacing today's pass-through; `Wait(w)` (the handle as a parameter, as 1.x's `Wire` parameter) with `timeout:` (seconds; expiring is an error), output `Any`; `Peek(w)` (non-blocking: `none` while the child runs, fails if it failed); `IsRunning(w)`; `Stop(w)` stops that instance, and `Stop` alone stays "stop this wire".
- `Wait` on a child that ended `Stopped` or `Cancelled` fails with a distinct error for each; a failed child's error passes through; a completed `none` stays a value. `Stop(w)` is an idempotent request; `Wait` acknowledges the child's cleanup, not quiescence of external work (H6).
- A spawned instance belongs to its spawner instance (not its iteration); `detach: true` gives it to the mesh. When the spawner ends, for any reason, its whole tree is cancelled in the same pass, children queued in the same activation included; finished children are pruned, so a looped spawner does not accumulate them.
- Handles are shareable values: several waiters, waiters that drop, a handle valid after the mesh retires the record, and across a reload it keeps naming the original instance (never retargets).
- Implementation: a shared completion cell per spawned instance (outcome, cancel request, waiters); `Wait` as an async shard; the mesh honors cancel requests before its wake filtering.
- **Implemented** (branch `concurrency`): `otter_core::wire_instance` (`Completion`, the `Wire.Instance` object, which every `Catalog` lists). `Wait` is a core suspending shard like `Pause` (its registration is its resume point, removed when the wait ends or its state is dropped), not an async shard: it needs the running instance's completion to refuse waiting on itself. A request (`Stop(w)`, an owner's end, the host through a handle) marks the cell and counts it on the mesh. One made between ticks ends its target before the next tick runs anything; one made during a tick takes effect at its end, after every running instance had its activation, down the whole tree, and a target that completed in that tick ends as the request says (`Stopped`, `Cancelled`). Nothing is checked per activation: the cost is two checks per tick. A reload that ends an owner ends its children before it returns. `Stop(w)` ends the target `Stopped`; an owner's end, the host and reloads end it `Cancelled`. A timed `Wait` is resumed every tick. Hosts get a handle with `Mesh::handle(id)`.
- Deviations from 1.x: structured ownership by default (1.x `Joint: false` lets children outlive spawners); handles never dangle or retarget (1.x `Spawn` returns a weak pointer into a recycled pool); stopped and cancelled children are distinguishable (1.x keeps the last output across a stop); spawned wires see the mesh's variables and their input, no deep-copied captured variables. Not ported: `Detach` (`detach:`), `Step`, `SwitchTo`, `Suspend`/`Resume`, `Branch`, `WhenDone`, `DoMany`/`TryMany`/`Expand`.
- Gate: fan out, collect and cancel a three-generation tree under both wake modes; spawn-then-exit before the child starts; detached survival; a parked async child cancelled; several and dropped waiters; every ending (`Stopped`, `Cancelled`, failure, a cleanup panic); bounded bookkeeping for looped parents; retained and restarted reloads with handles held in mesh variables; mesh destruction.

### H6. Cancellation is not quiescence

**Need.** Cancelling an instance drops its pending future and fires its token; a worker running blocking code finishes later. A model context, connection or host object must not be freed or reused while a worker still uses it.

**Proposal.** Rule for module authors: an operation owns `Arc`s of every resource it touches, so releasing the instance cannot free a resource in use; resources with exclusive use (a chat context) are leased from their service and returned when the worker finishes, not when the instance is cancelled. Long native calls get a cancellation bridge where the library has one (llama.cpp's abort callback, `sqlite3_interrupt`). Document it in `embedding.md` and test it per module.

**Decision (2026-10-11):** accepted as proposed.

### H7. Exchanging values with the host per instance

**Need.** The host starts many instances of one wire concurrently (one per user session) and needs per-instance inputs, outputs and progress while they run (streamed text, token counts, the current UI tree). 1.x binds host memory into each wire as externals and the host polls it every tick. 2.0 has spawn input, outcomes, and mesh-global variables exchanged by value.

**Proposal.** No bindings into frames. Inputs go in the spawn input (a table). Results come out as the outcome. Progress goes through objects the host passes in the input: a `Cell` (a host-readable value with a version counter that the script sets through `Cell.Set`) and a `Channel` (H5). The host keeps the same objects and reads them after each tick. A host that wants shared, mesh-wide state keeps using typed mesh variables.

### H8. Compiling code while the program runs

**Need.** Model-written tools and UI are checked, compiled and run while the application runs, with a list of forbidden shards and shared definitions; widgets compile script text they receive; a checker runs inside the app. `metaprogramming.md` §5 leaves runtime `eval` out of its plan and calls `Mesh.Spawn(code)` "a separate decision for later"; `golden-path.md` §12 lists runtime `eval` and scoped type registries as out of scope. This is that decision.

**Proposal.**
- **Host side first:** compile source into a running mesh (`Session`/`Program` already compile; add "compile into this mesh with this catalog subset", then spawn) and check source with diagnostics. The catalog subset is the forbidden list, expressed through effects and names.
- **Script side:** `Code.Compile(source)` (async, outputs a definition handle or fails with diagnostics) and `Spawn` on that handle, with the same restrictions and compose-time budgets (`EvalLimits`).
- **Memory:** a long-running application compiling generated code cannot keep a type registry and compose cache that never free. Scoped registries (types and shapes owned by a compilation unit and freed with it) and a bounded compose cache become prerequisites. Both are already listed as open in `current-state.md`.

**Status (2026-10-16): the host side, minimal, is done** as `otter_lang::Tool` ([embedding.md §6](embedding.md#6-tools)): load a script with a catalog (the catalog is the forbidden list), get its definition (name, description, JSON Schema parameters) and inferred effects, call it with JSON arguments, host settings, a working directory, services, a cancellation flag and a time limit, on a fresh mesh per call; `otter tool` drives it from the command line. Each tool owns its program and compiled call wire, but compiles into the process-wide type registry, so the memory item above still stands, as do compiling into an already running mesh, the script-side `Code.Compile`, and the soak gate in §7.

### H9. Platform and configuration branching

**Need.** Scripts select code by platform and build configuration (`@if` 99, `@platform` 28, `@env` 18), including code that uses shards which do not exist on other platforms. One script calls a host shard inside `#( )` to compute a path, which 2.0's compose-time rules correctly refuse.

**Proposal.** Host-declared constants: the host declares named constants when it loads a program (`platform: "ios"`, `debug: false`), and they are compose inputs like any other, so determinism holds. `@if(condition {then} {else})` selects code at load, before compose, so the branch not taken may name shards the catalog lacks. `condition` is a compose-time expression over constants. `@env` stays removed: the host passes what it means. Paths and other host facts are constants or run-time calls, never compose-time reads of the host.

### H10. The C API and host-language shards

**Need.** A non-Rust host must load, check, run and exchange values with programs, and implement shards in its own language: 169 in the surveyed host. `prototype-shard-contract.md` §0 plans "a small C API for running programs", rules out a C ABI for implementing shards, and expects binary extensions "behind the Rust trait, for example as an adapter shard type".

**Proposal.**
- **`shards-capi` crate** (cbindgen header, static library) for running programs:
  - values, types and tables;
  - catalog creation;
  - session load with a host `Files` callback;
  - check with diagnostics as JSON;
  - spawn, tick, cancel and outcomes;
  - mesh variables, cells (H7) and services;
  - a log sink.

  Every entry point catches panics.
- **Foreign shards through one adapter `ShardType`** inside the Rust contract, as §0 allows. A descriptor is built at run time (name, parameters, input and output types, effects; leaked to `'static`, since descriptors live for the process). Compose, activate and cleanup go through a function table with a user pointer. `Compiled` holds the host's compiled pointer, `Send + Sync` by the host's contract.
- **Async foreign shards.** `start` returns a pending operation; the host completes it from any thread with a value or an error, which wakes the instance (the waker is already an atomic flag). This replaces suspending inside an activation, which the stackless engine cannot do.
- **Foreign control shards** (a host shard that runs nested flows): later, when a host needs one; two shards in the survey do.
- **Thread affinity.** The mesh ticks on the host's chosen thread; foreign shards activate on it. A UI host ticks on its main thread.
- Requires `ShardType` registration at run time: descriptors are `const` today.

### H11. Packaging, targets and startup

**Need.** Static libraries for Apple platforms (device and simulator, packaged as an xcframework), and later Android; CI coverage for them. 1.x hosts ship a precompiled binary form of their scripts; 2.0 compiles source when it loads.

**Proposal.**
- **Packaging:** cargo builds per target plus `xcodebuild -create-xcframework`, with a CI job that builds them (the pinned toolchain has prebuilt std for every Apple target a current host ships; 32-bit-pointer watchOS would need `build-std`). Hosts copy the block-alignment flag (`embedding.md` §1).
- **Startup:** measure compile time for a large program at startup before designing a binary form; if it matters, cache lowered definitions keyed by source hash, not a new serialization of the AST.

### H12. Language additions

- **Iteration.** `$0` is removed and there is no shipped `ForEach`, `Map`, `Filter` or `Reduce`; today users write them as functions with flow parameters. They are among the most common operations in real scripts (230 `ForEach` sites). Proposal: core shards with an explicitly named element binding, flattened into the VM like `Repeat`. The binding syntax is the decision. **Decision (2026-10-23):** `seq | ForEach(x i {...})`, names positional and optional, value first (`tbl | ForEach(v k {...})`), the body's input the element; with paths on the pipeline value (`| .a.b`), dynamic and lenient (`?`) paths, and destructuring (`= {a b}`, `= [a _ c]`) in the same milestone. Contract: [iteration-and-access.md](iteration-and-access.md).
- **Macros.** `@define` and `@template` (383 sites) map to constants, functions, flow parameters, and M10 macros for the rest. Real hosts are a good source of M10's cases.
- **Constructors** for vectors and colors (`@f3`, `@i2`, `@color`): small frontend work.
- **`@include` options** (`Once: true` at almost every site): accept and ignore `once`, or report it with a fix-it, since 2.0 always includes once.

### H13. Migrating 1.x scripts

**Need.** Every 1.x codebase fails `check` on syntax alone (thousands of errors per large codebase, nearly all mechanical).

**Proposal.** `otter migrate`: rewrites parameter labels, the assignment operators, `And`/`Or`, sequence and table forms and known renames, and leaves each semantic change as a diagnostic at its site:
- `Do` on caller scope becomes a function with explicit arguments;
- `Set ... Global: true` becomes mesh state;
- persistence across iterations of a looped wire becomes `Keep`;
- `Await` (H5);
- `$0`.

Translation stops where meaning changes; it does not guess.

### H14. Observability

Diagnostics are structured; at run time a host cannot see much. Proposal: per-instance tracing hooks (activation and suspension events, compiled out by a feature), a per-wire timing profile, and an instance inspector over `instance_locals` and the frame stack. Lower priority than the rest; noted so it is not lost.

## 3. Catalog breadth

No design decisions, mostly volume. The core shards real scripts use and 2.0 lacks, by group:

- **Strings:** `Join`, `Split`, `Trim`, `Contains`, `Starts`, `Ends`, `Replace`, `ToLower`/`ToUpper`, `CodePoints`, `FromCodePoints`, `CleanUTF8`.
- **Regex:** `Match`, `Search`, `Replace`. **Decision:** the dialect. 1.x uses C++ `std::regex` (ECMAScript); Rust's `regex` crate has no lookaround or backreferences. Proposal: `regex`, with the differences documented and checked against the patterns ported scripts use.
- **Sequences:** `AppendTo`, `PrependTo`, `Slice`, `Extend`, `Clear`, `PopFront`, `DropFront`, `Reverse`, `IndexOf`, `Remove`, `Sort`.
- **Types and checks:** `IsString`, `IsTable`, `IsSeq`, `IsInt`, `IsFloat`, `IsBool`, `IsBytes`, `IsTrue`/`IsFalse`, `ToAny`, `ToAnyTable`, `Expect*` sequence forms.
- **Control and errors:** `Fail`, `Pass`, `Cond`, `WhenNot`, `Msg` (logging with a level, not events).
- **Encoding and identity:** base64 both ways, hex to bytes, bytes and ints, `NanoID`, UUID, `Hash.XXH3-64/128`, `Hash.Blake2-128`.
- **Time:** epoch seconds and milliseconds, formatting.
- **Assertions:** `Assert.Is`, `Assert.IsNot` (640 asserts in the surveyed tests).
- **Parameter parity on existing names:** `Get(default:)`, `Http.Get`'s headers, bytes, retry and full-response options, the `Match` and `Once` options the scripts use.
- **JSON** (`ToJson`, `FromJson`). **Decision:** the mapping of ints against floats, bytes, none and non-string keys.

**Done (2026-10-16), the text-tool subset:** `String.Join`, `String.Split`, `String.Trim`, `String.Contains`, `String.Starts`, `String.Ends`, `String.Replace`, `String.ToLower`/`String.ToUpper`, `String.EncodeURI`/`String.DecodeURI`; `Slice` (strings by character, sequences, bytes), `Sort`, `Bytes.Join`; `IsString`, `IsTable`, `IsSeq`, `IsInt`, `IsFloat`, `IsBool`, `IsBytes`; `Fail`; `ToBase64`/`FromBase64`, `ToLEB128`/`FromLEB128`; `Date.Parse`/`Date.Format` (UTC, a `strftime` subset); `Math.Mod`, `Math.Max`, `Math.Min`; `Http.Get`'s `headers`, `bytes`, `full-response` and `retry` (with `backoff`); `ToJson`/`FromJson`; the fs module's core (§4). **JSON decided:** `null` is none; a number without a fraction or exponent that fits in 64 bits is an Int, any other a Float, and a Float is always written with a fraction or exponent (`2.0`), so text reads back as the same value; numbers out of the f64 range, NaN and infinities are errors; objects are tables (string keys, as JSON's; key order not kept, a repeated key keeps its last value); float vectors are written as arrays; byte strings and objects have no JSON form (encode bytes with `ToBase64`); nesting is limited (128, 32 on ESP-IDF). The codec is hand-written in `otter-core` (`json.rs`), not serde_json: the core has no dependencies and runs on ESP-IDF and WASI, the codec reads straight into values with no intermediate tree, and the Int/Float rule and the depth limit would need custom code on serde_json anyway.

**Done (2026-10-16), in `otter-text`:** `Regex.Match`, `Regex.Search`, `Regex.Replace`; `Hash.XXH3-64`/`-128`, `Hash.Blake2-128` (and `Hash.Blake2-256`, `Hash.Sha2-256`, `Hash.Sha1`, `Hash.Md5`, `Hash.Blake3`); `NanoID`, `UUID` (version 4; 1.x has no version 7). **Regex decided:** the Rust `regex` dialect, as proposed: linear time, patterns size-limited, constant patterns compiled at compose; the differences from 1.x are in current-state.md. The crate holds the text shards that need third-party crates, so the core keeps no dependencies.

**Done (2026-10-16), the rest of the core's part:** `String.CodePoints`, `String.FromCodePoints`, `String.CleanUTF8`; `AppendTo`, `PrependTo` (sequences, strings and byte strings), `Extend`, `Clear` (sequences and open tables), `PopFront`, `DropFront`, `Reverse` (by character for strings), `IndexOf` (an element or a run, `all:`), `Remove` (a predicate flow per element, a composite through the public `ControlShard` interface); `IsTrue`/`IsFalse`, `ToAny`, `ToAnyTable`, `ExpectIntSeq`, `ExpectFloatSeq`, `ExpectBoolSeq`, `ExpectStringSeq`, `ExpectBytesSeq`, `ExpectTableSeq`, `ExpectFloat2Seq`/`3`/`4`; `Pass`, `Cond`, `WhenNot`, `Msg` (`level:` from trace to error, with a process-wide threshold, `otter_core::log::set_level`); `HexToBytes` (alias `FromHex`), `BytesToInts`, `IntsToBytes`; `Time.Epoch`, `Time.EpochMs` (Ints); `Assert.Is`, `Assert.IsNot`. Shards that change a variable follow `Push`'s rules (declared, mutable, typed at compose) as leaf shards writing their binding, so the VM gained no instruction. `Cond` and `WhenNot` lower into the parent's code like `If` and `When` (`ComposeCtx::flatten`): `WhenNot` is `When` with a jump on true, `Cond` a chain of `If` branches sharing one saved input (one branch is `If`'s code exactly), so neither engine nor VM changed. For `Cond`'s `passthrough:`, a variadic parameter may now be followed by parameters given by name, never required (`check_params`). The new shards with work that is constant or charged by size are on the compose-time list. Regex, hashes, NanoID and UUID (an `otter-text` crate), the fs shards and `Http.Get`'s other options (`otter-io`), and `Maybe` and the date shards are other streams' work.

**The remaining parameter parity:**
- **Done (2026-10-16): `Get(default:)`, `Match(passthrough:)`, `Once(every:)`.** `Get(v default: d)` and `Get(t key: "k" default: d)` output the value (or the table's value at the key) when the default's type admits it, the default otherwise, with the default's type; on a fixed table a literal key must exist. A plain `Get` is still the VM's instruction; one with a default is an ordinary activation (its compiled form says which, `inline::leaf`). The 1.x use for a variable that may not exist is not supported, by definite assignment: the host declares the variable or passes an open table. `Match(passthrough: true)` outputs its input (the engine's `dispatch_once`, out of line). `Once(every: seconds)` runs the action again on a fixed cadence, on the first activation at or after each scheduled time (as 1.x); it is a custom composite whose state keeps that time (`OnceCompiled::Every`), so the engine's `Control::Once` and every frame are unchanged, and it declares the clock effect.
- **`IndexOf(predicate:)`** and `Remove`'s `Join:`/`Unordered:` wait for iteration with a named element (H12).

## 4. Modules

Each depends on the decisions above (objects, services, cleanup, cancellation). Where modules live is itself a decision; proposal: crates in this workspace (`shards-db`, `shards-llm`, …), native-only by default, each behind its own feature.

| Module | Approach | Size |
|---|---|---|
| sqlite | Connection service; `Db.Query` and `Db.RawQuery` async; `Db.Transaction` as a control shard leasing the connection, with H4 guards; the bundled SQLite, cr-sqlite (C) and sqlite-vec built with `cc` against one amalgamation; existing databases as acceptance fixtures (replication, FTS5, vector tables, result types). **First cut done (2026-10-20, `otter-sqlite`):** read-only `Db.Query` on the blocking pool, rows as tables, positional and named parameters, `max-rows`, writes refused three ways, cancellation through `sqlite3_interrupt`, the `Connections` pool service, paths through `FsAccess` with read grants, FTS5 and WAL databases being written by another process; desktop and iOS (rusqlite 0.40, bundled). Open: writes, `Db.RawQuery`, `Db.Transaction`, cr-sqlite, sqlite-vec, typed row shapes | L–XL |
| http | Methods, streaming reads (a stream object, H1) on the shared runtime; a server on hyper, one spawned handler per connection. Done for `Http.Get`: headers, bytes, retry, full response; `Http.Post` with the same options, its input the body (2026-10-20) | L |
| llm | Own `-sys` crate (cmake and bindgen over `llama.h`, `mtmd.h`, and later `whisper.h`, one ggml); models and contexts in a service; load, add-text, add-image, embed and generate as async shards (generation stays one token per activation, as in 1.x, so streaming needs no new mechanism); abort callback for cancellation (H6); Metal on Apple | L (XL with vision, speech and every target) |
| fs | Done, the core a text tool needs (2026-10-16, `otter-io`): reads and writes on the blocking pool, probes, listings, directories, copy, rename, remove, leaf path functions, and an `FsAccess` service a host scopes or denies access with ([embedding.md §3](embedding.md#3-the-catalog-and-scripts)). Open: chunked and size-limited reads, write times, `RelativeTo`, `Normalize`, `Absolute`, watching | M |
| json | Done: `ToJson`/`FromJson` in the core, a hand-written codec, mapping per §3 | done |
| imaging | `Image` object (H2), decode, encode, crop, resize on the `image` crate | M |
| events, channels | Mesh service with async receive (H5) | M |
| ssh, local shell | Port the shared session code; sessions as objects; async waits | M |
| network | WebSocket client | M |
| fileops | Grep, walk and rank over reused crates | S–M |
| crypto | The hash subset first | S |
| markdown, pdf, spreadsheet, brotli, bigint, random | Thin wrappers over existing crates | S each |
| candle | Only UMAP is used in production: one pure-Rust shard over float sequences | S |
| audio | Deferred until a production use appears | XL |

## 5. Differences ported scripts will meet

These are deliberate and stay, but each needs a line in the migration notes and a test in the first ported code:
- named calls do not see caller scope;
- persistence is explicit (`Keep`);
- `Once` is for effects;
- stateless functions cannot hold state;
- integer overflow is checked;
- floats print exactly, where 1.x rounded to six digits;
- `Float2` components are f32;
- tables have string keys;
- `Match` must be exhaustive or have `default:`;
- cancellation cannot be caught by `Maybe`;
- `Maybe` without `else` passes its input through, as in 1.x;
- `Spawn` outputs the instance, and owns it unless `detach: true` (1.x passes the input through, and children outlive their spawner);
- parameter labels are lowercase;
- `looped` is the only wire option, and wire `Pure:` has no equivalent.

## 6. Order

The first ordering, written for porting the surveyed host as it is. §7 supersedes it.

1. Decide H1, H3, H4, H5 and H6 together (one value design, one resource and lifetime model), then H7 and H10 on top of them. H8 and H9 can be decided in parallel.
2. Catalog breadth (§3) and the migration tool (H13): no dependency on the decisions, can start at any time, and turn real scripts into compose errors that name the true gaps.
3. Objects and services in the core, with cleanup and cancellation tests; then the C API's smallest end-to-end: a host registers one leaf and one async foreign shard, and ticks, cancels and exchanges values on two Apple targets.
4. sqlite, against copies of real databases; then http with streaming; then llm.
5. A vertical slice in a real host: one complete user-facing flow (a streamed request, a tool call, persistence, cancellation, UI progress through cells).
6. The rest of the modules, runtime compilation (H8), observability.

## 7. Priorities (2026-10-11)

The surveyed host is not being ported as it is. It is being replaced by **an agent-orchestrator host**: a Rust node that runs on every machine (iOS included, in-process), with native apps and a CLI as clients. Agents and tools are written in Otter; the store, sync, providers and UI are not. That host embeds 2.0 the way `embedding.md` already describes. So the C API and foreign shards (H10), the 1.x migration tool (H13), most of the modules and the UI concerns drop out, while objects, services, concurrency and runtime compilation become the critical path. The host's first milestones (its store, chat with attachments, a CLI, external agent workers, the iOS client) need nothing new from 2.0 except an iOS build, so both sides can proceed in parallel. Its native agent loop is the first point that needs the work below.

**P0. Rename (done 2026-10-11).** The project is Otter: crates `otter-core`, `otter-lang`, `otter-io`, `otter-cli`, the `otter` command, the repository. The language kept the name Shards until 2026-10-17, when it was renamed Otter too; script files are `.otr` (preferred), `.otter` or `.shs` (still accepted).

**Track A: the core decisions, in order** (the host's native loop needs A1 to A3). The strict performance gates ([performance-gates.md](performance-gates.md)) land first, so A1 is measured against them:
1. **Objects and services** (H1, H3), specified together with guard cleanup (H4) and the cancellation rules (H6). Gate: a host object (a fake session) created, used and released by scripts; a service looked up at activation; release on success, error, `Stop`, `Return` and cancellation; hot-asm unchanged, or the difference explained; the ESP32 suites still fit.
2. **Concurrency** (H5): `Spawn` outputs a handle; `Wait`, `Peek`, `IsRunning` and `Stop(w)`; spawned instances owned by their spawner unless detached; channels as a service. Gate: an orchestration script that fans out, waits on and cancels workers, with cancellation reaching every child.
3. **Runtime compilation** (H8), host side first: compile into a running mesh with a catalog subset, and check with diagnostics; scoped type registries and a bounded compose cache (both already open items); then a script-side `Code.Compile`. Gate: a soak compiling and dropping generated tools with memory flat.
4. **Iteration** (H12): `ForEach`, `Map`, `Filter`, `Reduce` with a named element, flattened into the VM.

**M10** (code as data, `metaprogramming.md` §4) is not on this host's critical path. It runs after A1 and A2, or in parallel with them when there is capacity; its three cases are supplied.

**Host needs met on the way (2026-10-11):** log lines streamed to the host as they are written, each with its instance (`otter_core::log::Sink::stream`, `Session::set_log_sink`); defaults for script arguments (`@const(name value ignore-redefined: true)`, the argument read as the default's type). Iteration with a named element (A4 below) is needed by a host's scripts too.

**Track B: catalog breadth** (§3), at any time and in parallel: strings, regex (dialect decided first), JSON (mapping decided first), fs, hashes, time, the `Is*` family, `Fail`/`Pass`/`Msg`, and parameter parity (`Get(default:)`). The text-tool subset comes first.

**Track C: modules, after A1:** local shell and SSH (one shared session layer), HTTP extensions (methods, headers, bytes, streaming reads), llm embeddings first (own `-sys` crate over llama.cpp), and the memory-index shards over a service.

**Track D: targets, early and cheap:** a CI job that builds `otter-core`, `otter-lang` and `otter-io` as a static library for iOS device and simulator (H11), since the host's node runs in-process on the phone.

**Later:** observability (H14) once A2 lands; platform branching (H9) when a host needs it.

**Deferred** until a host needs them: the C API and foreign shards (H10), the 1.x migration tool (H13), image and audio values (H2), sqlite writes, transactions and extensions (the host uses SQLite in Rust as a derived index; its tools needed to read an existing database, a chat app's store with FTS5 indexes in WAL mode, on desktop and iOS, which the read-only first cut of 2026-10-20 covers: §4), imaging, spreadsheet, pdf, network, candle, audio.
