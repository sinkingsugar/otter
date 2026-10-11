# Prototype Shard Contract: Decisions Replacing `shards.h`

**Status:** Agreed (2026-10-04), after review by Astra and the maintainer, and implemented in `crates/otter-core`. Later additions (panic boundary, definite initialization, lifecycle rules, wake modes, shared `LeafShard`/`AsyncShard` APIs) are recorded in the relevant sections and in [`stackless-experiment.md`](stackless-experiment.md).
**Scope:** The interface decisions the §5 prototype in [`shards-2-compose-split.md`](shards-2-compose-split.md) needs. In 1.x, all of these are fixed by the C ABI in `include/shards/shards.h`. Each decision below is marked as either:

- **Prototype only:** a choice the prototype can make and throw away.
- **Commitment:** a choice that would shape every future shard and module. These need agreement before the prototype is built on them.

---

## 0. The framing decision: does 2.0 need a C ABI for shards at all?

`shards.h` exists so that C++ and Rust modules can implement shards against one binary interface, with the C++ core as host. In 2.0, Rust is the host, and C++ libraries are called from Rust through binding crates (design doc §3.5). Shard implementations no longer need to cross a language boundary.

**Proposal:** 2.0 shards implement a **Rust trait**. There is no C shard ABI in the core, at least for the prototype. Two kinds of extension need to be kept apart:

- **Source extensions** need no stable binary interface. An external Rust crate can implement the trait and be compiled into a custom runtime without belonging to this workspace. Swift or C++ functionality can also sit behind a Rust shard wrapper.
- **Binary extensions** need a stable foreign-language interface: implementing or loading shards from another language, or from a separately compiled artifact, without rebuilding the runtime. The roadmap's candidate for these is wasm components (roadmap §3.8), not a C ABI.

Embedding 2.0 into a host application (Swift, C, C++) needs a small C API for *running* programs (load, schedule, tick, exchange values), not for *implementing* shards.

Consequence: most of the choices below become ordinary Rust API design, not ABI design. That is the main reason this redesign is tractable.

**Open question A (resolved 2026-10-04, maintainer):** no binary-extension requirement for now. Assume only Rust projects: in-workspace crates, and external Rust crates compiled into a custom runtime (the 1.x `SHARDS_EXTERNAL_MODULE_DIRS` use case, as source extensions). **The requirement is recorded, not dropped:** binary extensions (implementing or loading shards from another language, or as separately compiled plugins) are expected eventually. When they are designed, they must fit behind the Rust trait, e.g. as an adapter shard type that hosts wasm components (roadmap §3.8), rather than reshaping the trait into a C ABI.

## 1. Shard identity and dispatch

| 1.x (`shards.h`) | Proposal |
|---|---|
| `struct Shard` per instance, about 25 function pointers filled in by each `create()`; `refCount`, `id`, `line`/`column`/`file` in the instance header | One static `ShardType` per shard kind (name, help, parameter schema, `compose`). A compiled wire is a graph of nodes, each `{ shard_type, Arc<Compiled>, source location }`. No per-instance header. |
| `inlineShardId` written by compose, so the VM can switch on hot shards | Compose may return a specialized activation (e.g. a different `Compiled` variant or function) chosen at compose time. Nothing is written into shared nodes. |

**Commitment:** per-type static metadata plus shared compiled nodes.
**Implemented optimization (2026-10-05):** compose selects immutable enum
instructions for core Const/Get/Set/Ref/Update/Inc, specialized Add, Take and
non-clearing Push, plus consuming Seq.Make/Table.Make segments. Selection uses
the Rust implementation identity, never a
shard's name. Both schedulers run uninterrupted instruction segments through
the same executor; other shards retain trait-object dispatch. Normal node
instantiation and cleanup still run. See the [VM measurements](vm-execution-benchmarks.md).

## 2. Parameters

| 1.x | Proposal |
|---|---|
| `setParam(index, SHVar*)` and `getParam(index)` on live instances; parameters are mutable at any time; clones are rebuilt by replaying `setParam` | `Params` is a typed, validated value built once by the loader from the AST. It is immutable after construction and is an input to compose (and part of the cache key). |

`getParam` exists today for serialization, the formatter round trip and tooling. In 2.0 those read the AST or the `Params` value instead of asking an instance.

**Immutable parameters can still describe dynamic inputs.** A parameter that refers to a variable (e.g. `Take(waypoint)`, `Repeat(times: n)`) compiles into a **binding** whose value is read during activation (§8). Updating that variable's value does not require recompilation. Changing *which* variable the parameter refers to does. This keeps the useful 1.x behavior of variable-valued parameters.

**Commitment:** parameters are immutable inputs to compose. Changing a parameter means building a new compiled node. For live editing (roadmap §3.5), that is a replacement, not a mutation.

## 3. Types

| 1.x | Proposal |
|---|---|
| `SHTypeInfo`: a C union with raw pointers to nested type arrays. Ownership is spread out: shards own their exposed-type arrays, `wire->outputType` points into a shard (`runtime.cpp` ~L1526). `fixedSize` and `recursiveSelf` are special cases. | `Type` is an **opaque, registry-scoped handle** (cheap to clone and compare) to an immutable, interned description. Structural equality and hashing are defined once, in one place. The registry owns all type data. `TypeCache` in 1.x is the seed of this. |

**Commitment:** opaque handles to interned, immutable type descriptions. This is a precondition for the compose cache, because cache keys contain types and must compare them structurally (design doc §3.2).

**Recursive types (open question B, resolved for now):** unsupported in the prototype. For eventual structural recursion, the direction is explicit recursion binders/references, rather than letting user-visible names determine type identity. That avoids tying type equality to names and avoids ownership cycles in the registry. Recursive equality and canonicalization need their own design, which does not block the non-recursive prototype.

## 4. Compose interface

| 1.x | Proposal |
|---|---|
| `compose(Shard*, SHInstanceData*)`: `SHInstanceData` carries the shard pointer (for write-back), the wire, the input type, `shared` (all visible variables), `onWorkerThread`, `outputTypes` and private context | `compose(&Params, &mut ComposeCtx) -> Result<Composed<Self::Compiled>>`. `ComposeCtx` gives the input type and **records every permitted dependency read**: variables by name (returning their binding and type), host configuration, target features, policy. |
| Exposed and required variables are returned as arrays owned by the shard | `Composed` returns the output type, the variables this node exposes (with types and mutability), and the `Compiled` value, all owned by the compiled graph. |
| `onWorkerThread` flag | A declared property of the compose context (and part of the key). |

**Purity is a trusted implementation contract, not something the API enforces.** Ordinary Rust code inside `compose` can still read the clock, the filesystem or global state, and nothing in the type system stops it. The contract: compose output is a deterministic function of `Params`, the input type and the dependencies read through `ComposeCtx`. Shards that cannot meet it must opt out of caching (design doc §3.2). Review, tests and the cache-collision test in the design doc §5 are the safeguards.

**Recorded reads include failed lookups.** "Variable `x` is absent" is a dependency just like "variable `x` has type `Int`". If `x` later appears, a cached result composed without it must not be reused.

**Cache lookup is a two-step process:**

1. Find candidate entries by the **primary key**: shard type and implementation version, `Params` (hashed, then compared for equality) and input type.
2. For each candidate, **revalidate its recorded dependencies** against the current `ComposeCtx`: same bindings, types, absences, host configuration and policy. The first candidate that passes is reused. If none passes, compose, and store the result along with its recorded dependencies.

This avoids the trap of needing the full key before lookup, which would mean composing just to discover what the key is. It is the same early-cutoff approach used by incremental build systems.

**Commitment:** compose as a deterministic function of declared inputs under the trusted contract above, with recorded dependency reads (including absences) and two-step lookup.

## 5. Lifecycle and termination

| 1.x | Proposal |
|---|---|
| `setup` → `setParam`\* → `compose` → `warmup` → `activate`\* → `cleanup` → `destroy`, all on one instance | `compose(params)` once per cache entry → `instantiate(&Compiled, &mut InstanceCtx)` per instance (replaces `warmup`) → `activate(&Compiled, &mut State, &mut ActivationCtx, input)` → `cleanup(&Compiled, &mut State, &mut CleanupCtx)` → `Drop` |
| `cleanup` doubles as "stop" and "reset for reuse" | `cleanup` ends an instance. Recycling (the 1.x pool's job) becomes "drop the state, instantiate again", which is cheap if `State` is small. |

**The runtime owns termination and cleanup.** For every successfully instantiated `State`, the runtime calls `cleanup` **exactly once**, whether the instance completes, fails or is cancelled.

- **Partial instantiation:** if `instantiate` fails partway, it must release whatever it acquired before returning the error. No `cleanup` call follows, because no `State` was produced.
- **Cancellation:** a suspended instance is first unwound (its suspended execution is ended), and only then is `cleanup` called and its `State` destroyed. State that suspended code may still reference must not be destroyed under it.
- **`Drop`:** releases any remaining memory ownership. It must not repeat logical cleanup (closing handles, notifying services) that `cleanup` already did.
- **Loop iterations and `Restart` are not termination.** A looped wire's next iteration, or a `Restart`, keeps the same instance and `State`, and does not call `cleanup`.
- **The instance is the panic boundary.** A panic in `instantiate`, `activate` or `cleanup` fails that instance only: the scheduler and other instances are unaffected, and every successfully instantiated state is still cleaned up exactly once (a panic in one node's cleanup does not skip the others). After a panicking `activate`, `cleanup` may see a state left mid-update, and must tolerate it. This requires `panic = "unwind"`; with `panic = "abort"`, a panic ends the process.
- **Attempt every cleanup, everywhere.** Normal termination, rollback after a failed instantiation, and composites with several nested flows all follow one rule, implemented once (`lifecycle.rs`) and used by every flow and composite on both schedulers: every successfully instantiated state gets its cleanup attempted, even if another cleanup panics; the first panic is reported afterwards. During rollback, such a panic is reported in the instantiation error instead of propagating.
- **Finished instances release their execution data.** On every terminal path the scheduler drops the instance's input, locals and shard states; only the outcome is kept. `Mesh::take_outcome` returns a finished instance's outcome and retires its record, so a long-running mesh does not accumulate them.

**Commitment:** this lifecycle and these termination rules. The cancel-and-cleanup-exactly-once acceptance test (design doc §5) is written against them.

### Preserving reload and nested call lifetimes

`Session::reload_preserving` keeps compatible running instances. `Do` selects
an immutable body from a mesh-local registry at the start of each call and
pins it until the call exits, including across suspension. Registry keys are
structural, so each Do consults the registry only when the mesh's reload
revision has changed since its last check, once per accepted reload. A changed body is
cleaned up and instantiated at the next call boundary; unchanged bodies keep
their state. Cleanup uses the body's retained compiled handle, never a newer
revision's node list. A failed instantiation remains retryable if caught.

Compose records both the full dependencies (for cache correctness) and the
dependencies outside reloadable `Do` boundaries (for retaining the caller).
A candidate checks call-site input/output, frame layout and definite
initialization before commit. Structural site identities include ancestor
body definitions, preventing an old in-flight body from selecting rearranged
new descendant sites. Compatible mesh frames remain in place. Definitions,
revision selection and caches belong to the mesh, never to shared mutable
compiled data. [The embedding guide](embedding.md#5-warm-sessions-and-hot-reload)
records the API, restart behavior and limitations.

## 6. Activation and flow control

| 1.x | Proposal |
|---|---|
| `activate(Shard*, SHContext*, const SHVar* input) -> const SHVar*`: the returned pointer points into storage owned by the instance (`outputStorage`) | Output is written into a slot in the instance's state frame, or returned by value for small values. The exact form is a prototype choice; either way the output lives in instance state, never in shared data. |
| Errors and flow control (`Stop`, `Restart`, `Return`, abort) go through the context and `SHVAR_FLAGS_ABORT`; errors are stored as strings in the context | Activation returns a result type: a value, an error, or a flow-control signal (`Stop`, `Restart`, `Return`). |
| `SHContext`: everything else (variables, suspension, the mesh, errors) is reached through one opaque context | **`ActivationCtx`** (prototype): an explicit execution interface, giving short-lived access to the instance's frames (§8), scheduler operations (suspend, run a sub-wire for `Do`) and resource services. |

**Commitment:** explicit result types for values, errors and flow control, instead of context flags. An explicit execution context instead of an opaque one.

The builtin executor consumes its incoming value into owned scratch and
borrows its accumulator from constants, frame slots or call-local storage.
Obsolete reference-counted scratch is released when the accumulator moves
elsewhere; it must not keep captured collections shared through a later frame
mutation. Numeric scratch needs no resource cleanup. A compose-time pass
tracks whether scratch may own a value and selects release/non-release variants
of Const/Get/Set/Inc. Generic/constructor boundaries reset this analysis;
Take/Push and generic vector arithmetic can introduce owned scratch again.
Once released, later reanchoring instructions have no scratch-cleanup check
until another operation may create an owner. The pass preserves node indices
for lifecycle and diagnostics; both schedulers enter only at segment boundaries.
The opcode enum has an explicit u8 discriminant. Get is further specialized to
Local/Mesh opcodes with compose-computed byte offsets. Offset construction checks
multiplication; activation checks against the selected frame's byte length.
No instance pointer is stored in shared code and frame bounds are still checked. Its raw pointers cannot escape the
call: frames are exclusively borrowed, cannot resize, and no callbacks or
suspension occur inside a segment. Every write consumes its input before
replacement and reanchors the accumulator. Segment exit produces an owned
snapshot before any generic shard, nested flow or suspension. Debug and
`output-checks` builds validate every builtin's declared output as usual.
Numeric scratch holds only explicitly constructed numeric variants, so it
can be overwritten without running the general `Var` destructor dispatch.
This is an internal optimization; host shard signatures are unchanged.

Constructor segments consume the incoming value. They recover Vec/BTreeMap
storage only from a uniquely owned Arc and reuse owned buffers across
consecutive constructors; shared inputs remain snapshots. The output is
wrapped in an Arc at the segment boundary. Constructor state is empty: it
must not retain captured elements after consumers drop the output. This avoids
forcing later mutation of those elements to copy shared storage. Checking
builds validate every intermediate constructor output as well.
**Prototype only:**
- the output slot mechanism
- the exact `ActivationCtx` API
- **how suspension is represented.** With the stackful scheduler (§7), suspension happens inside an `ActivationCtx` call and does not appear in the return value. This is deliberately not part of the permanent contract: a stackless scheduler may need suspension in the result type. Explicit shard state alone does not capture nested call continuations.

## 7. Suspension and scheduling

1.x uses stackful fibers (`coro.hpp`, Asyncify on wasm). The design doc (§3.4) has a hypothesis that explicit `State` makes stackless state machines possible.

**Proposal (prototype only):** build one stackful implementation first, using a Rust coroutine library (e.g. `corosensei`). It is the simplest scheduler that runs nested `Do` + `Pause`, which the acceptance test requires. Keep `State` explicit, and keep suspension out of the permanent activation contract (§6), so the stackless hypothesis stays testable later.

**Open question C (resolved):** no stackless version in the first prototype. It would double the scheduler work before the core split is validated.

**Follow-up (2026-10-04):** a bounded stackless experiment was run after the prototype: [`stackless-experiment.md`](stackless-experiment.md). Result: the stackless scheduler is the default and both are maintained, one backend per mesh. Suspension is therefore represented both ways: in the stackless contract as `Step::Suspend` returned from `activate` (with resume points in `State`), and in the stackful contract inside `ActivationCtx`. Leaf and async shards are written once over both (`LeafShard`, `AsyncShard`).

**Follow-up (2026-10-07):** golden path M4 replaced the recursive stackless activation with a directly resumable engine and deleted the stackful scheduler. Suspension is now represented one way: `Step::Suspend` returned from `Shard::activate`, with the resume point in `State` (builtin composites instead expose a `Control` description, and the engine owns their continuations). `LeafShard` keeps the suspension-free `Flow` result. `ActivationCtx` has no suspend call.

**Measurement (historical):** with stackful execution, each instance's memory included its coroutine stack and continuation overhead, not only its shard state; the two were reported separately.

## 8. Variables

| 1.x | Proposal |
|---|---|
| Looked up by name at warmup (`referenceVariable`), returning `SHVar*` into the wire's variable map; globals live on the mesh | Compose resolves each variable name to a **binding**: `Local(slot)` (the instance's frame) or `Mesh(slot)` (the mesh's frame), with type and mutability checked during compose. Activation reads and writes through `ActivationCtx` by binding. Names exist only at compose time and in tooling. |
| `Do` shares the caller's variables implicitly | `Do` composes the sub-wire against the caller's frame layout, so it reads and writes the caller's slots. That frame layout is part of the cache key of that sub-wire's compiled form. |

**Mesh variables (open question D, resolved for the prototype):** a second frame, owned by the mesh. Compiled artifacts contain only **binding descriptions**. Each instance supplies its local frame, and each mesh supplies its mesh frame. Two meshes running the same compiled program therefore have separate mesh variables (e.g. separate `ready-count` values in the benchmark).

Prototype rules: one scheduler thread, and a mutable frame borrow is never held across a suspension.

**Definite initialization:** compose tracks whether each local is definitely assigned at each point. A variable assigned only in a flow that might not run (a `When` body, or a `While`/`Repeat` body, which can run zero times) is not definitely assigned after it, and reading it there is a compose error. `Once` bodies and `Do` sub-wires always run when reached, so assignments in them count. Activation therefore never observes an unassigned local.

**Commitment:** compose-time resolution of variable names to `Local`/`Mesh` bindings, with compiled artifacts holding descriptions and instances/meshes holding the frames, and definite-initialization checks for locals.

## 9. Values (`SHVar`)

| 1.x | Proposal |
|---|---|
| `SHVar`: a 16-byte payload union, an 8-byte side field (object info, version or short string) and a type tag; manual ownership via flags (`REF_COUNTED`, `EXTERNAL`, `FOREIGN`); strings, sequences and tables carry capacities and are managed manually; tables go through function-pointer interfaces | A Rust `Var` enum with owned or reference-counted variants. Sequences and tables are Rust collections, possibly copy-on-write (roadmap §3.7). Objects are `Arc<dyn Object>` with a type id. |

**Prototype only:** a minimal `Var` enum (none, bool, int, float, float vectors, string, seq, table). Size and layout get measured against `SHVar`'s 32 bytes.
**Commitment (later, not for the prototype):** the final `Var` layout. It affects performance everywhere and should be decided with measurements.

**Measured follow-up (2026-10-05):** the [VM execution benchmark](vm-execution-benchmarks.md)
finds a substantial gap against 1.x on hot cheap-shard chains, while independent
collection assignment can benefit from sharing. On x86-64 the current `Var`
is smaller than `SHVar`, but owned output transfer, reference counting and the
activation result envelope still cost work. Compose-selected builtin segments
now avoid those transfers; Float4 Add emits packed SIMD on the measured x86-64
release build. The `Var` enum layout itself is unchanged and remains open to
measurement. The compiled/state split is not contingent on by-value outputs.


## 10. Dropped or deferred from `shards.h`

| 1.x feature | Proposal |
|---|---|
| `mutate` / `crossover` (genetic programming hooks) | Drop. The genetic module that used them was removed in 1.x (`7cc0edd59`); only the declarations remain. |
| `getState` / `setState` / `resetState` (deep state serialization) | Defer. Live editing (roadmap §3.5) preserves only explicitly declared state, which suggests a declared, typed state export later rather than generic hooks. |
| `SHWireProvider` | Drop. It is declared in `shards.h` / `shards.hpp` but has no users in the 1.x tree. |
| Serialization of whole wires to clone them | Not needed: instances are created from compiled wires. Program serialization (`.sho`) stays at the AST level. |
| `SHARD_FLAGS_OWNED_SHARD` and per-instance `refCount` | Not needed: compiled nodes are shared through `Arc`. |

## Summary of the agreed commitments

1. §0: shards implement a Rust trait, with no C shard ABI in the core. Rust projects only for now; binary extensions are a recorded future requirement.
2. §1: per-type static metadata plus shared compiled nodes.
3. §2: immutable parameters as compose inputs, with variable-valued parameters compiled to bindings.
4. §3: opaque handles to interned, immutable types (recursive types deferred).
5. §4: compose as a deterministic function of declared inputs under a trusted contract, with recorded reads (including absences) and two-step cache lookup.
6. §5: the compose / instantiate / activate / cleanup lifecycle, with runtime-owned, exactly-once cleanup.
7. §6: explicit results for values, errors and flow control, and an explicit execution context. Suspension representation stays open.
8. §8: compose-time resolution to `Local`/`Mesh` bindings.

Prototype-only choices (§1 dispatch, §6 output slot, `ActivationCtx` API and suspension representation, §7 stackful coroutines, §9 minimal `Var`) can change after measurement without breaking these.

## Golden path M4 execution update

The stackless runner now owns generation-tagged flow frames and dispatches child entry/completion iteratively. Pending async leaves and Pause resume directly; no ancestor handler is dispatched merely to find a pending leaf. Composite continuations retain the input, phase, loop limit/counter and selected child. The runner resets transient continuation state on value/error/control-signal exits, and routes errors through the nearest Maybe and Return through Do's current named boundary. No frame pointer survives a callback or suspension.

Initialization and cleanup are iterative and preserve the existing depth-first/reverse-depth-first order. Cleanup detaches ownership before calling host code, attempts all leaf cleanups through the shared lifecycle helper, and reports the first panic. Reloaded Do bodies stay pinned while active and replace their child subtree only at the next entry. Native LeafShard and AsyncShard APIs are unchanged. Ordinary variable scope, Keep storage and root-loop yielding are preserved for the later function/frame-layout milestone.
