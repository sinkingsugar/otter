# Golden Path: functions, scope and the stackless engine

**Status:** approved plan, 2026-10-06. Merged by Opus from [Astra's proposal](astra-isolated-callables-proposal.md) and [Fable's runtime spec](spec-runtime-and-values.md) after a four-way review with Giovanni. **This document is the executable contract.** Where the two source specs disagree with it, this document wins; they remain as background for the reasoning.

**Mandate:** compatibility with 1.x and with current 2.0 scripts is not a goal. Delete old forms outright; no aliases, deprecations or migration tooling. Update existing tests and examples directly.

**For the implementing session:** read this file, then `AGENTS.md` and `docs/current-state.md`. Work milestone by milestone (§9). Each milestone is one or more commits on the `golden-path` branch (CI runs through its draft pull request to `main`) that pass the full check set in `AGENTS.md`, update the docs listed in its gate, and get an Astra review through the review-handoff skill before the next milestone starts. Do not start a milestone early because its code looks easy.

---

## 1. The model in one paragraph

A **definition** owns shared, immutable compiled code. An **instance** owns persistent state. An **invocation** owns temporary locals. There are three named constructs: `@fn` (a function, invoked from a pipeline like any shard), `@fn ... stateful: true` (a component: one persistent instance per call site), and `@wire` (a process, scheduled or spawned). Named constructs see only what their signature declares. Anonymous blocks (`If`, `When`, `Repeat` bodies) are lexical. A function that remembers has one place per call site; a function that does not remember may call itself. Execution is stackless on an iterative engine whose frames are directly addressable, so resuming a suspended leaf never touches its ancestors.

## 2. Settled decisions

| # | Decision | Notes |
|---|---|---|
| D1 | `Do` is deleted. No `Inline` and no textual macros. Metaprogramming is compose-time evaluation, non-escaping flow parameters and hygienic macros over `Code` values, specified in [metaprogramming.md](metaprogramming.md) (M8 to M10). | Calling a named definition never shares the caller's frame. |
| D2 | Script functions are called exactly like native shards: `speed \| Scale(factor: gain)`. | One call form, one catalog, one `describe`. Name collisions with native shards or aliases are compose errors. |
| D3 | **All** parameter and declaration labels are lowercase, native shards included: `Repeat(times: 3 action: {...})`, `@run(m iterations: 2)`, `@fn(... input: Int output: Int)`. | Uppercase is a shard, lowercase is a value. Inside a function body, a parameter is an immutable local of the same name. |
| D4 | `@fn` is stateless by default: fresh locals per invocation. `stateful: true` is explicit and opts into `Keep` and per-call-site instances. | Compose infers what the body needs and checks it against the declaration. `Keep` without the flag is `keep-in-stateless` with a fix-it. |
| D5 | `pure: true` is a separate, optional, checked contract on effects. | Isolation is not purity. Unknown native effects are never pure. |
| D6 | Stateless functions may recurse (milestone M7). Stateful functions and wires may not. | Calls compose against declared signatures, so compose never recurses. |
| D7 | Assignment: `= x` immutable bind, `value \| Var(x)` declare mutable, `value \| Update(x)` assign existing, `value \| Push(xs)` append, `Keep(x literal)` persistent. | `>=`, `>`, `>>`, `Set`, `Ref` are deleted. Each form errors in the wrong situation. |
| D8 | `Match` is exhaustive or has `default:`. Silent pass-through is gone. | Coverage is proven for Bool, None and finite unions of those; everything else needs `default:`. |
| D9 | Stackless only, on a trampoline with frame handles. Stackful is deleted after the gate in §6.4. | Not before. |
| D10 | `Var` is 32 bytes. Fixed tables are structs. `Float2/3/4` are f32; scalar `Float` stays f64. | §7. |
| D11 | Diagnostics are pinned by code, span and symbol; human text is a snapshot test, not the contract. | |
| D12 | Taste disputes from here on are settled by the authoring eval (§8), not by argument. | |

## 3. Language

### 3.1 Signatures

Every native shard and script definition exposes one signature view (a common descriptor over static native `ShardDesc`s and owned script descriptions; do not leak script descriptions to get `&'static`):

- identity and revision;
- `input` type, `output` type;
- ordered named `params`: type, required or literal default;
- lifetime: stateless or stateful;
- mesh access: `uses` (reads) and `mutates` (writes), with types;
- effects (§3.7);
- source location and docs when available.

For `@fn`, `input`, `output` and `params` are **required** in this plan (use `params: {}` when there are none). `output: None` discards whatever the body ends on (2026-10-10, after the M9 authoring eval, where a body ending on a value was 12 of 13 compose errors); any other declared output must accept the body's (`output-type-mismatch`). Effects are inferred; if declared, the body must stay within them. Mesh access is **declared, never granted by inference**: `uses` permits reads, `mutates` permits writes, and a read-modify-write needs both. Compose infers actual mesh usage, including through called definitions, and rejects anything undeclared (`undeclared-mesh-access`). `describe` prints the signature, and `check --json` carries the inferred input and output type **at every shard occurrence**.

```shards
@fn(Scale input: Float output: Float params: {factor: Float} {
  Math.Multiply(factor)
})

3.0 | Scale(factor: 2.0) | Log
```

### 3.2 Scope

A named definition sees exactly:

1. its pipeline input, and the reserved immutable binding `input` (the entry value, not the moving accumulator; the declaration label `input:` names its type, the body binding `input` is its value);
2. its parameters, as immutable locals;
3. its own locals, and `Keep` slots when stateful;
4. mesh variables declared in `uses`/`mutates`, by bare name;
5. native shards and other definitions, subject to their own signatures.

No lookup into the caller's locals. No fallback from an unknown local to the mesh. A miss is `unknown-variable`, and when the caller has a local of that name the help says how to pass it as a parameter.

Blocks (`If`, `When`, `Repeat`, `Maybe`, ... bodies) are lexical: they read enclosing locals and may `Update` enclosing mutable ones. A declaration inside a block is block-local and does not escape it. No shadowing: a name may not redeclare a visible local, parameter, `Keep` slot or declared mesh name (`duplicate-binding`). Disjoint sibling blocks may reuse a name. `input` is reserved: a parameter or local named `input` is `reserved-name`, with help saying it is the entry value.

### 3.3 Arguments

- Labels are validated against the parameter list first.
- Argument expressions run once each, in source order, before the callee is entered. Each receives the call's original pipeline input, not the previous argument's result. They run in the caller's lexical scope.
- Plain literals and variables lower to a copy, not a flow.
- Argument expressions may suspend; already computed arguments are kept and never re-evaluated.
- If an argument fails, the callee is never instantiated or entered. Effects of earlier arguments are not rolled back.
- Parameters are immutable snapshots for the whole invocation, including across suspension. A mesh write during the invocation does not change a bound parameter.
- Defaults are typed literals only.
- No captures and no copy-back. Values go in through the input and parameters, and come out through the output (a fixed table when there are several). The caller writes them with `Update`.

Runtime argument values never enter compose cache keys; their types do.

### 3.4 Lifetimes

**Stateless `@fn`:** every invocation starts with fresh locals. Native shard state inside it lives for the invocation (it survives suspensions within that invocation) and is cleaned up on return, failure, cancellation, `Stop` or `Restart`. `Keep`, `Once` and calls to stateful functions are rejected. Suspension and I/O are allowed.

**`stateful: true`:** each static call site, inside each owning instance, owns one component instance. Call sites share compiled code, never state. Ordinary locals are still fresh per invocation; only `Keep` slots and the persistent state of its native shards and stateful children survive between invocations. Stateful functions may be called from wires and other stateful functions, not from stateless ones. Re-entry and recursion are rejected (`recursive-stateful`).

**`@wire`:** a process with its own persistent component tree. Each root iteration of a looped wire gets fresh ordinary locals and keeps `Keep` and component state. Wires are scheduled or spawned, never called.

**Failure inside a stateful invocation:** `Keep` and mesh writes already made stand (state is not a transaction). Transient activation state is reset before the next invocation. A native component that cannot recover marks itself poisoned; later entries fail until an explicit reset. Never silently rebuild it and replay initialization.

**Control signals:** `Return` exits the nearest named invocation (or ends the root iteration at the root). `Restart` unwinds to a fresh root iteration, keeping persistent state. `Stop` ends the process. Suspension is not an exit and runs no cleanup. Cleanup runs exactly once per instantiated state, all cleanups are attempted even if one panics, and cancellation does not undo external I/O.

### 3.5 `Keep`, `Once`, `Push`

- `Keep(n 0)` declares persistent mutable storage with a literal initializer, applied at instantiation. Allowed only at the top lexical level of a wire or stateful function, outside branches and loops. Passes its input through.
- `Once` remains for one-time effects, only in stateful owners. It is no longer the way to declare state.
- `Push` neither declares nor clears. Per-iteration sequences are declared explicitly: `[] | Var(xs)`. An unannotated empty sequence types as `[Any]` until element-type inference exists.

### 3.6 Syntax summary

| Write | Meaning | Error when |
|---|---|---|
| `value = x` | immutable bind | `x` visible already |
| `value \| Var(x)` | declare mutable | `x` visible already |
| `value \| Update(x)` | assign | `x` missing (`unknown-variable` with did-you-mean) or immutable (`immutable-binding`) |
| `value \| Push(xs)` | append | `xs` missing, immutable or not a sequence |
| `Keep(x 0)` | persistent slot | outside a stateful owner, or not top level |
| `Match([...] default: {...})` | match | not exhaustive and no `default:` (`non-exhaustive-match`) |

Comparisons stay named shards. M2 deletes `>=`, `>`, `>>`, `Set` and `Ref` from the lexer, parser, lowering, catalog and docs. `Do` is **not** deleted in M2: it survives through M4 (ported minimally to the trampoline) and is replaced and deleted in M5, so every milestone keeps the suite green. Unmatched pass-through in `Match` is `default: {}` (an empty flow outputs its input); `default: {input}` means the enclosing invocation's entry value, which is different inside a function.

### 3.7 Effects

Each native `ShardDesc` declares:

```rust
pub struct Effects {
  pub suspends: bool,
  pub io: bool,
  pub time: bool,
  pub random: bool,
  pub unknown: bool, // a host shard that declared nothing
}
```

A shard that declares nothing is `unknown`. Every core catalog shard is classified in M3. A definition's effects are the union over its body, its argument expressions and everything it calls (over each strongly connected component once recursion exists), plus its mesh access and lifetime from the signature. `pure: true` requires: stateless, no mesh access, all effect flags false, no suspension. A failure is `not-pure`, naming the offending shard and effect. Effect metadata on native shards is a trusted host contract, not a sandbox.

M3 implementation: `signature::Signature` is the shared owned-or-borrowed view; native descriptions and compiled wires expose it. `CompiledFlow::analysis` carries inferred effects, mesh access and relative occurrence paths; frontend analysis attaches source locations. See [metadata implementation](shard-metadata-and-compose.md#golden-path-m3-signatures-and-inference) for JSON fields and conservative native classifications. Function declarations and purity/permission enforcement remain M5.

## 4. Lowering and caching

- A compiled function body holds callee-relative bindings, its frame layout, effects, code and the node-to-source mapping. A call site holds argument evaluation code plus a reference to a compiled body; bodies are never cloned into callers.
- Cache key: definition structure and revision, native implementation identity, input type, parameter types, types of declared mesh bindings, and recorded dependencies. Hash collisions are resolved by structural equality (the existing rule). Runtime values and caller layouts are excluded.
- Mesh bindings in a body are requirement indices, linked to a mesh's actual slots at instantiation after type and permission checks. Bodies do not bake in one mesh's offsets.
- Sharing a body must not make two call sites report the same source location for an error: occurrence paths stay per call site, outside the cache key.

## 5. Frames

- The runtime owns a **frame arena per instance**. Frames are addressed by generation-tagged handles. No raw pointers to frames survive a callback, suspension or reload.
- Function frame layout: `[ params | keep | locals | temporaries ]`, recorded once per compiled body.
- Frame storage for a non-recursive stateless call site may be preallocated, but it belongs to the **owning invocation's frame**, never to the compiled call site: once the enclosing function can recurse, several live invocations reach the same static occurrence. Reuse releases old locals at every entry. Preallocation saves the `Var` slots only: native shard state inside a stateless function is still instantiated and cleaned up per invocation (free for arithmetic with `State = ()`, a real cost for a function wrapping `Http.Get`; that cost is correct by the semantics, not a bug). Recursive call sites allocate at entry and free at return. Stateful call sites own their frame for the life of the owner.
- A callee frame is a separate allocation from the caller's, so a callee gaining or losing a local never shifts caller offsets.
- Builtin instruction segments keep today's rule: borrow frames only inside a segment with no callbacks, suspension or reload inside it; export owned values at generic and suspension boundaries.

## 6. Execution engine

### 6.1 Protocol

Control shards never call a child's `activate`. They return requests to a runner loop:

- `Enter(child, input, continuation)`: allocate or select the child frame, record where the result goes, transfer control.
- `Wait`: park the instance on its current innermost frame.
- `Complete(Result<Var>)`: deliver a value **or an error** to the parent's continuation.
- `Signal(Return | Restart | Stop)`: unwind to the matching boundary.

A continuation records the parent frame, its phase or program counter, any input or partial result it needs, loop counters and handler boundaries, and the compiled code it belongs to. Errors travel through `Complete` like values until a `Maybe` frame handles them; cancellation is not a catchable error. Transient state of reused control frames is reset centrally on every non-`Wait` exit, so no shard author has to remember it. Cleanup of nested state is iterative too, so deep teardown never recurses on the Rust stack.

Every composite is rewritten once onto this protocol: `If`, `When`, `While`, `Repeat`, `Once`, `Maybe`, `All`, `Any`, `Match`, `SubFlow`, `Spawn`, and later the function call. `Do` still exists during M4 and must be ported too, but minimally (it is an `Enter` into an inlined flow); it is deleted in M5, so do not invest in it. `LeafShard` and `AsyncShard` keep their single implementations; async futures live in their frame's state and wakers mark the instance ready.

M4 implementation uses `stackless::engine`: builtin composite descriptors request child entry or return a completion (value, error or signal) to the runner. The arena owns all flow states by generation-tagged handle; activation, initialization and cleanup use explicit work lists. Initialization counts the immutable child graph before reserving frames and uses exact child/state capacities to avoid transient doubling allocations on devices. Existing anonymous blocks and `Do` still share the wire's variable frame, as required until M5. `Mesh::set_max_call_depth` limits named entries (256 native, 32 ESP-IDF), independently of the retained recursive compose limit. M4 preserves the existing loop scheduling rule: a root iteration yields even if a `Pause` already yielded within it. Reload registry keys share immutable ancestor-path nodes (with iterative drop), and dependency snapshots share immutable definitions; equality and hashing remain structural. Compose occurrence metadata retains shared relative-path trees and materializes full paths only for tooling; nested calls do not copy descendant paths into every compiled ancestor. Frontend source locations likewise share prefixes in a flat trie; completed parser lists release spare capacity. Full-depth tooling analysis remains tested on native/WASI, while device diagnostics checks avoid unused reports.

### 6.2 Complexity target

- Re-polling a pending leaf dispatches **zero** ancestor handlers, at any depth.
- Completing through N levels costs N dispatches once, like a stackful return.
- Wake modes `PollEveryTick` and `OnNotify` are unchanged.

### 6.3 Depth limits

`MAX_FLOW_DEPTH` (48) exists because stackless activation recurses in Rust. After the trampoline it is replaced by a per-mesh `max_call_depth` (default 256 native, 32 on ESP-IDF), checked at invocation entry, failing with `recursion-limit`. Keep the compose nesting limit until compose, instantiation and cleanup are each checked to be non-recursive.

### 6.4 Gate for deleting stackful

All of:

1. **Hard:** a structural test with a dispatch counter shows zero composite dispatches during pending re-polls at depths 1, 4, 16, 32.
2. **Soft:** `bench_depth`, depth 32, 1000 instances, release: trampoline within 1.5x of stackful on pending re-poll; short mixed flows regress at most 10% against the current stackless baseline. Report pending re-poll and full completion separately, with alternating samples. If a soft gate misses, report it and ask Giovanni; never change the workload to pass.
3. Every shared-suite test passes on the trampoline natively (debug and release), docs-off, on WASI, and on ESP-IDF QEMU in CI; targeted Miri on the frame arena.

Then, in one commit: delete stackful, `corosensei`, `cfg(stackful)`, `--stackful`, parity-only scaffolding; make `Mesh` non-generic in the public API; update `AGENTS.md` (the "two implementations" rule disappears), the docs and CI. Keep semantic regression tests and archived benchmark data.

Until the gate passes, the old stackless engine and stackful stay as oracles for unchanged semantics; nothing new is written for stackful.

## 7. Values

### 7.1 Opaque collections (first, M1)

Nothing outside `otter-core` names `BTreeMap`. `Var::as_table() -> Option<&Table>` (an opaque `Table`, which `Var::Table` holds; earlier drafts called the view `TableRef`), `as_seq() -> Option<&[Var]>`, `as_str() -> Option<&str>`, a `TableBuilder` (`Table::into_builder` derives a changed table), sorted iteration. The external host moves to this API before it couples to the map.

### 7.2 Layout (M6)

- `Var` targets **32 bytes**, as 1.x's `SHVar`. Prototype the real layout first (the nested `TableRepr` tag and padding count) and only then pin it. On 64-bit targets: pin `size_of::<Var>() == 32` and alignment 32, so a `Var` in an array never straddles a 64-byte cache line (alignment 16 does not guarantee that). On 32-bit targets (ESP32, wasm32): alignment 16 (what `Float4` needs) and whatever natural size the prototype gives, pinned by a `cfg`-gated test; do not pad to 32 there. Wrap vector payloads so `Float4` sits at a 16-byte offset and assert the offset in a test. Benchmark align 32 against 16 on the 64-bit VM suite once and record it; keep 32 unless it measurably loses.
- `Float2`, `Float3`, `Float4` become `[f32; N]`. This is a numerical change: update literals, constructors, arithmetic, comparisons, printing, metadata, hashing and tests together, with rounding and non-finite edge cases. Scalar arithmetic does not change.
- Inline small strings are deferred to separate work, measured on their own.

### 7.3 Struct tables (M6)

```rust
enum TableRepr {
  Struct { shape: Shape, slots: Arc<[Var]> }, // keys known at compose
  Map(Arc<Vec<(Arc<str>, Var)>>),           // sorted, binary search
}
```

- `Shape` is an interned sorted key list; equal shapes are pointer-equal.
- Fixed table types are always `Struct` at runtime. Constant-key `Take` on a known shape lowers to an indexed read. Open tables use `Map`.
- Shape validation at host boundaries is a pointer compare and runs in release. Per-slot type checks stay behind `output-checks`.
- Equality, hashing, printing and `type_of` are identical across representations for equal contents; test every pairing.
- Copy-on-write through `Arc::make_mut` for both.
- Named parameters never depend on tables: they lower to frame slots.

### 7.4 Registry lifetime

Shapes join the type registry, which never frees today. Add a reload soak test (1000 reloads introducing distinct shapes) and report growth. Do not claim bounded memory for long `watch` sessions until scoped registries exist; that is a follow-up, not part of this plan.

## 8. Authoring eval (M0, rerun after M5 and M7)

`bench/authoring/`: 30 to 50 fixed small tasks (counter, retry with backoff, struct transform, looped poll with `Pause`, two-parameter function, `Match` with default, tree fold once recursion lands), the reference generated by `otter catalog`, and a small Rust runner that shells out to a model CLI given on the command line (no embedded API client). Loop: task plus reference to the model, `otter check --json`, diagnostics back, at most 5 repair rounds, then run the task's test script. Record per (task, model, variant): semantic success, first-pass check success, repair rounds, tokens, latency. Separate syntax failures from missing-shard failures.

The first run happens **before M2**, on today's syntax, as the baseline. Variants for later runs include `Param(factor)` against bare lowercase parameters. Small samples are evidence, not proof; report them as such.

The post-M5/M7 [Haiku rerun of 2026-10-07](../bench/authoring/results/2026-10-07-haiku-functions/README.md) includes recursive tree folding and two harder tasks shared with the pre-M2 control. It reports identical-prompt comparisons separately from the changed function prompt and the new recursion capability; strict output failures and manual coverage limits remain visible. Broad milestone reviews are a separate requirement.

## 9. Milestones

| M | Content | Gate (beyond the full check set) |
|---|---|---|
| **M0** | Docs hygiene: rewrite `current-state.md` as one screen of what is implemented, current decisions and open items; move history to `docs/history.md`; fix stale README claims. Authoring eval harness and baseline run. | A fresh session can find the next task in under 100 lines of reading. Baseline CSV committed from **real** runs only. If no model CLI is available, commit the harness, record the baseline as blocked in `current-state.md`, and continue with M1 to M3; never commit mock or placeholder results. |
| **M1** | Opaque `Var` collection API (§7.1), `Map` storage unchanged. | No `BTreeMap` in any public signature. Embedding guide updated. |
| **M2** | Surface syntax: lowercase labels everywhere (D3), assignment forms (D7), `Keep` for wires (replacing the `Once({0 >= n})` idiom, which M2 removes), `Push` without implicit declare/clear, exhaustive `Match` (D8), `reserved-name` for `input`, block scoping and no shadowing (§3.2 for blocks). `Do` stays. All tests and examples rewritten. This is the largest mechanical diff in the plan: change only what the syntax requires, and per `AGENTS.md` discovery does not authorize unrelated fixes; record them instead. | Old forms fail to parse with a plain error. Tests E and F (§10) pass. |
| **M3** | Signatures and effects (§3.1, §3.7): common descriptor view, every core shard classified, inference, `describe` output, per-occurrence types in `check --json`. | Every catalog entry has effects. `check --json` shows input and output type at every shard. |
| **M4** | Trampoline (§5, §6) for all existing composites. Then delete stackful when §6.4 holds (separate commit). | §6.4 in full. `bench_depth` results replace the stackless column in the runtime overview. |
| **M5** | Functions: `@fn`, stateless and stateful lifetimes, call sites, private frames, arguments (§3.3), body caching (§4), mesh `uses`/`mutates`, `pure`, reload policy (§11). Delete `Do`. | Tests A to D and G to K pass. Eval rerun. |
| **M6** | Values: 32-byte `Var`, f32 vectors, struct tables, indexed `Take`, storage benchmark against `BTreeMap` recorded once, registry soak test. | Test L passes. Collection rows of the VM suite rerun. |
| **M7** | Recursion (D6, §6.3): SCC effects, function ids instead of cyclic `Arc`, frames at entry, `max_call_depth`, recursive-group pinning. | Test M passes. Eval rerun with the tree task. |

| **M8** | Compose-time evaluation: `#( ... )` and `@const` ([metaprogramming.md §2](metaprogramming.md#2-m8-compose-time-evaluation)). | §2.4 there. |
| **M9** | Flow parameters, `Run`, and the public continuation interface for native composites ([§3](metaprogramming.md#3-m9-flow-parameters)). | §3.4 there. |
| **M10** | `Code`, `Quote`, `$` splices, `@macro`, hygiene, `expand` ([§4](metaprogramming.md#4-m10-code-as-data)). Starts only on the condition in §4.1 there. | §4.7 there. |

M1, M2 and M3 are independent of each other. M4 needs nothing new. M5 needs M3 and M4. M6 needs M1. M7 needs M5. M8 needs M5; M9 needs M8; M10 needs M9 and its start condition.

## 10. Acceptance tests

Script fixtures live in `crates/otter-lang/tests/functions/` (and existing suites for M2), with Rust tests for lifecycle and internals. Diagnostics assert code, span and symbol; text is snapshotted.

- **A. Shared body, runtime parameters.** `Scale` called with `factor: 2.0` and `factor: 4.0` on `3.0` logs `6`, `12`; one compose of `Scale`; changing a runtime argument composes nothing.
- **B. No caller capture.** `@fn(Bad input: Int output: Int params: {} { Math.Add(secret) })` called after `10 = secret` in the caller: `unknown-variable` at `secret` inside `Bad`, help suggests a parameter.
- **C. Fresh locals.** A stateless function doing `0 | Var(n)` then `n | Math.Add(1) | Update(n)`, called 3 times from one site, logs `1`, `1`, `1`; init and cleanup once per invocation, not per resume.
- **D. Components.** `@fn(Counter stateful: true input: None output: Int params: {step: Int} { Keep(n 0) n | Math.Add(step) | Update(n) })` at two call sites with `step: 1` and `step: 10` in a looped wire, 2 iterations: logs `1`, `10`, `2`, `20`; one compose. Without `stateful: true`: `keep-in-stateless` at `Keep`.
- **E. Bindings.** `1 | Update(coutner)` after `0 | Var(counter)`: `unknown-variable`, suggestion `counter`, no slot created. Second `Var(counter)`: `duplicate-binding` with a related span. `Update` of an `=` binding: `immutable-binding`.
- **F. Match.** Bool cases without default compose; `2 | Match([1 {10}])` is `non-exhaustive-match`; with `default: {}` it logs `2`. Regression: inside a function that received `99`, `2 | Match([1 {10}] default: {})` gives `2` and `default: {input}` gives `99` (lands in M5, when functions exist).
- **G. Stable parameters across suspension.** A function that pauses then adds `amount`, called with `amount` read from mesh `gain` (declared in `uses`); the host changes `gain` from 2 to 100 while suspended: result `5`, argument evaluated once. A variant suspends inside an argument expression and checks earlier arguments are not rerun.
- **H. Argument order and failure.** Two arguments evaluated through a test `Host.Mark` shard on input 7: trace `first`, `second`, both parameters 7; suspending in the second does not rerun the first; failing the second never instantiates the callee.
- **I. Effects and mesh.** `pure: true` calling `Time.Now`: `not-pure`, effect `time`. Calling an undeclared-effects test shard: `not-pure`, effect `unknown`. Mesh access without `uses`: `undeclared-mesh-access`, including indirectly through another function.
- **J. Engine.** Nested `If`/`Repeat`/`Maybe`/functions around a controllable async leaf: zero ancestor dispatch on pending re-polls at depths 1, 4, 16, 32; predicates run once; errors reach the nearest `Maybe` after abandoned frames are cleaned; `Return`/`Restart`/`Stop` hit the right boundary; cancellation cleans every instantiated state once, also after a cleanup panic where unwinding exists; stale wakes cannot resume a cancelled or recycled frame; owner and allocation counts stay flat over repeated suspend and resume; one waiting instance never blocks another.
- **K. Reload.** A caller counter runs while a stateless callee is suspended; a compatible revision adds a callee local and changes its arithmetic: the old call finishes with the old result, the next call uses the new one, the caller's frame layout hash and counter are unchanged. An output or parameter change is rejected atomically. For a stateful callee: editing `Keep(n 0)` to `Keep(n 10)` retains `n`; changing its type resets `n` and reports it; reordering two `Keep`s retains both. Under the embedding default policy the type change is rejected instead.
- **L. Values.** Struct and map representations compare and hash equal for equal contents; sorted iteration; snapshot isolation; indexed `Take` (asserted on the instruction kind); unknown key is a compose error; wrong-shape host value is rejected in release; `size_of::<Var>() == 32` and the `Float4` payload offset; f32 rounding cases for `Float2`.
- **M. Recursion.** First scalar cases: direct recursion (factorial on `Int`), mutual `Even`/`Odd`, `Pause` inside recursion. Then a tree fold over a representable shape, since this plan adds no recursive types: a flat node sequence `[{value: Int children: [Int]}]` where children are indices, and `Sum(node: Int)` recursing over child indices. Limit: `@fn(Down input: Int output: Int params: {} { Down })` with `max_call_depth: 8` fails with `recursion-limit`, function `Down`, depth 8, cleanup once. A cycle through a stateful function fails with `recursive-stateful`.

## 11. Reload policy

- A call site selects the newest accepted body at **invocation entry**. An in-flight invocation finishes on its pinned body. A recursive group is pinned as a unit while its outermost invocation is in flight.
- A call inlined at compose (a small stateless straight-line callee, `docs/current-state.md`) is part of its caller's body: the edit takes effect at the caller's next entry. A wire whose body inlined the edited function moves onto the recompiled body at the end of the iteration in flight, keeping all of its state (its own nodes', its components', its `Keep` slots), and is reported as swapped; a run of a wire that does not loop finishes on the body it started with.
- A candidate is admitted only if input, output and parameter types are compatible **and** its effects and mesh access fit what callers were admitted with. More effects need re-admission.
- Stateless callees may change their private locals freely; new invocations get the new layout and the caller never moves.
- Stateful callees: `Keep` slots match by **name and type** (not initializer, not position). Matching slots keep their values, new slots initialize, changed or removed slots reset. Native shard state inside the component is retained only when **both** hold: the old and new occurrence correspond (same definition at the same structural occurrence path, not merely an equal shard type after reordering), and that shard declares a compatibility contract. Otherwise it resets. Retention or reset happens only at an inactive component boundary; an active old invocation keeps its original state until it returns. The reload report lists retained, reset and restarted state by name. The caller is never reset.
- Whether resets apply is **host policy**: `otter watch` applies and reports them (running `watch` is the opt-in); an embedding host rejects any candidate that would reset state unless it passes a reset policy. `r` in `watch` stays the full restart.
- Rejecting a candidate leaves running code and state untouched.

## 12. Out of scope

Runtime `eval`; escaping closures (non-escaping flow parameters are M9); shared component instances addressed by several callers; tail-call frame reuse; computed `Keep` initializers; inline small strings; scoped type registries; state serialization or migration across machines; browser wasm; graphics and physics ports. Propose any of these to Giovanni separately.
