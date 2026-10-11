# Spec: Runtime and Values for Shards 2.0 Functions

> **Superseded by [golden-path.md](golden-path.md)** (2026-10-06). Kept as background for the reasoning; where they differ, the golden path wins.

**Status:** proposal, 2026-10-06. One of two specs written after the design review in chat; the companion spec covers the language and semantics (signatures, scope rules, `@fn`, syntax). This one covers what the runtime and the value model must do for that language to hold. Compatibility with 1.x is not a goal.

**Decisions this spec builds on** (settled in review, revised 2026-10-06 after the cross-review): `Do` is removed; named callables have private frames; `Params` are typed runtime values lowered into slots, with lowercase labels everywhere; `@fn` has fresh invocation state by default and `Stateful: true` opts into `Keep` and per-call-site instances; stateless functions may recurse; fixed tables become structs; vectors are f32; `Match` is exhaustive or has a default; assignment is `= x`, `value | Var(x)`, `value | Update(x)`, `value | Push(x)`, `Keep(x init)`; the scheduler is stackless on a trampoline.

The companion spec owns the shape of the signature. This spec treats the signature as given: input type, output type, `Params`, persistent state (yes/no), mesh `Uses`/`Mutates`, `Suspends`, `Effects`.

---

## 1. Values

### 1.1 `Var` becomes opaque for collections

`Var::Table`, `Var::Seq` and `Var::String` stop exposing their storage. Hosts and shards use an API:

```rust
impl Var {
  pub fn as_table(&self) -> Option<TableRef<'_>>;
  pub fn as_seq(&self) -> Option<&[Var]>;
  pub fn as_str(&self) -> Option<&str>;
}

pub struct TableRef<'a> { /* opaque */ }
impl TableRef<'_> {
  pub fn get(&self, key: &str) -> Option<&Var>;
  pub fn len(&self) -> usize;
  pub fn iter(&self) -> impl Iterator<Item = (&str, &Var)>;  // sorted by key
}

pub struct TableBuilder { /* opaque */ }
impl TableBuilder {
  pub fn new() -> Self;
  pub fn with_shape(shape: &Shape) -> Self;    // preallocates slots
  pub fn insert(&mut self, key: impl Into<Arc<str>>, value: Var) -> &mut Self;
  pub fn build(self) -> Var;
}
```

Nothing outside `otter-core` names `BTreeMap`. The external host port is the first consumer; land this before it depends on the map.

### 1.2 Two table representations behind one `Var::Table`

```rust
enum TableRepr {
  /// Keys known at compose: an interned shape plus one slot per key.
  /// Fits because `Var` is 32 bytes (§1.4).
  Struct { shape: Shape, slots: Arc<[Var]> },
  /// Keys not known at compose: sorted flat vector, binary search on read.
  Map(Arc<Vec<(Arc<str>, Var)>>),
}
```

Shape validation at a host boundary (a host shard returning a value typed as a fixed table) is a pointer comparison of `Shape` and runs unconditionally, in release too. Only the per-slot type checks stay behind the `output-checks` feature.

- `Shape` is an interned, immutable, sorted list of keys. Equal key sets intern to the same `Shape`, so a shape comparison is a pointer comparison.
- A table literal whose keys are all literal composes to `Struct`. A fixed table type (`rest: None`) is always `Struct` at runtime; compose guarantees it, debug builds assert it.
- `Take` on a fixed table resolves the key to a slot index at compose and emits an indexed read. `Take` on an open table does a binary search.
- `TableRef::get` works on both; callers do not know which they hold.
- Equality, hashing, printing and `type_of` are identical for both representations of the same contents. A test asserts this for every pair.
- Converting `Map` to `Struct` happens when a value with an open type flows into a fixed-typed parameter or variable after `Expect*` narrows it: one allocation, then indexed access from there on.

Memory: `Struct` is one `Arc<[Var]>` allocation; `Map` is one `Arc<Vec>`. Both are copy-on-write through `Arc::make_mut`.

### 1.3 Vectors

`Float2`, `Float3`, `Float4` are `[f32; N]`. Scalar `Float` stays `f64`. Integer vectors, `Color`, `Bytes` and objects remain deferred.

### 1.4 `Var` size

`Var` is 32 bytes, aligned to 16, as in 1.x's `SHVar`: two per cache line, `Float4` always SIMD-aligned, room for `Struct { shape, slots: Arc<[Var]> }` without a thin-pointer indirection, and inline small strings up to about 22 bytes. A test pins `size_of::<Var>() == 32` and `align_of::<Var>() == 16`.

### 1.5 Parameter labels

All parameter labels are lowercase, for native shards and functions alike: `Repeat(times: 3 action: {...})`, `Scale(factor: gain)`. Inside a function body a parameter is an immutable local with that name. Uppercase names are shards, lowercase names are values, no exceptions. `ShardDesc` parameter names change accordingly; the catalog, docs and diagnostics follow.

## 2. Frames

### 2.1 Frames per invocation, and per call site when stateful

Every instance of a wire owns a frame: a `Vec<Var>` sized at compose, slots addressed by compose-computed offsets relative to that frame. This is the current design and it stays.

A function call site is a shard occurrence. Two cases, decided by the function's declaration (companion spec):

- **Stateless `@fn` (the default):** the invocation owns its frame. Non-recursive call sites preallocate one frame in their `State` at instantiate and reuse it, since an occurrence is never active twice at once; the frame's locals are reinitialized at every entry. Call sites on a recursive cycle allocate the frame at invocation entry and release it at return (§2.5).
- **`Stateful: true`:** the call site owns one callee instance for the life of the caller instance. Its frame holds `Keep` slots that persist across invocations. Such functions may not recurse (compose error).

In both cases the callee's compiled code addresses slots relative to the callee frame, so the caller's layout does not change when the callee gains or loses a local. This is the reload requirement from review: private namespaces inside one flat allocation still shift offsets; separate allocations do not.

Cost: one extra allocation per call-site instance, paid at instantiate, never at activate. Instances are measured in the hundreds to tens of thousands, allocations in the single digits per instance, so this is acceptable. If measurement later shows otherwise, an arena per instance with stable sub-frame offsets is the fallback and does not change the contract.

### 2.2 Frame layout

```
[ params... | keep... | locals... | temporaries... ]
```

- `Params` occupy the first slots, in declaration order. The call site writes them at invocation entry and never again until the next invocation.
- `Keep` slots follow. They are initialized at instantiate from their literal and survive across invocations. They are reset only by cleanup.
- Ordinary locals follow. They are definitely-initialized by the existing analysis before any read.
- Temporaries (`%n` from lowering) last.

The compiled function records this layout once. Every call site instantiates the same layout.

### 2.3 Argument evaluation (runtime contract for the companion spec's rules)

At invocation entry, in declaration order, each argument expression runs with the original pipeline input as its input, and its result is written to its param slot. Arguments are evaluated before the body starts. If an argument expression fails, the call fails and the body does not start. Arguments that are plain variables or literals are lowered to a copy, not a flow.

### 2.4 Mesh variables

Unchanged: one mesh frame per mesh, slots resolved at compose. A function reaches a mesh variable only through its declared `Uses`/`Mutates`, by bare name; a local may not shadow a declared mesh name. Compose rejects any other mesh read or write with the diagnostic in §7.

### 2.5 Recursion

Allowed for stateless `@fn` only. Rules:

- A function on a call cycle must declare `Input` and `Output` (the companion spec requires them for every `@fn` in the first slice, which makes this automatic). A call composes against the callee's declared signature, never its body, so compose does not recurse and needs no fixpoint.
- Compose computes strongly connected components of the call graph. Effects are the union over the component. A cycle through a `Stateful: true` function or a `@wire` is a compose error (`recursive-stateful`).
- Compiled bodies inside a cycle refer to each other by program-owned function id, not by `Arc`, so a recursive program does not leak.
- Call sites on a cycle allocate their frame at invocation entry (heap, runtime-owned, never the Rust stack) and release it at return. Non-recursive call sites keep the preallocated frame of §2.1.
- A per-mesh `max_call_depth` (default 256 native, 32 on ESP-IDF) is checked at invocation entry. Exceeding it is an activation error (`recursion-limit`) naming the function and the depth; the instance fails and cleanup runs once. Today's `MAX_FLOW_DEPTH` of 48 exists because stackless `activate` recurses in Rust; after the trampoline (§3.2) it is replaced by this limit.
- A recursive group is one compiled unit for reload: an in-flight recursion finishes on its group; the next entry from outside the group selects the new one.
- Lands as its own milestone after the trampoline, with tests: direct recursion (tree fold), mutual recursion, suspension inside recursion, the limit error, and a rejected cycle through a `Stateful` function.

## 3. Scheduler

### 3.1 Stackless only

The stackful scheduler is removed once the condition in §3.4 is met. Until then it stays as the reference for parity tests and nothing new is written for it. `cfg(stackful)` code paths are deleted in one commit, with the `Backend` generic kept in case a second backend is ever wanted, but `Mesh` stops being generic in the public API.

### 3.2 Direct resume: the trampoline

Today a suspended instance is resumed from its root flow, and every nesting level re-enters its child until the suspended leaf is reached. Resume cost is linear in depth, and `MAX_FLOW_DEPTH` exists because nesting recurses on the Rust stack.

The replacement is Astra's iterative engine (companion spec): control shards never call a child's `activate`. They return a request (`Enter(child)`, `Wait`, `Complete(result)`), and a runner loop owns the execution. Every flow and composite state is a **frame in a per-instance arena** addressed by handle; a suspended instance records the handle of its innermost waiting frame plus each frame's return continuation. A wakeup activates that frame directly. A result walks up one continuation at a time, each step being one dispatch, until a frame suspends again or the root completes.

Consequences this spec commits to:

- Re-polling a pending leaf dispatches zero ancestor handlers. This is the hard gate for deleting stackful, tested structurally with a counter on composite dispatch, not inferred from timing.
- Returning through N levels costs N dispatches once. That is the same as a stackful return and is not a regression.
- Errors are results: a frame completes with `Err`, and the continuation chain delivers it upward until a `Maybe` frame handles it. There is no out-of-band error path.
- Recursion (§2.5) is the same machinery with frames allocated at entry instead of preallocated.
- Every composite (`If`, `When`, `While`, `Repeat`, `Once`, `Maybe`, `All`, `Any`, `Match`, `Spawn`, the function call) is rewritten once onto this protocol. This is the same set that would otherwise be maintained twice for the two schedulers, so the rewrite replaces that cost rather than adding to it.

An earlier draft of this section proposed descending by node index as a cheaper first step. It was withdrawn in review: it is a faster linear walk, not direct resume, and its `completed` slot could not carry an error. Build the trampoline directly.

### 3.3 Wake modes stay

`PollEveryTick` and `OnNotify` are unchanged. `OnNotify` already keeps idle instances off the tick loop; direct resume matters for instances that are resumed every tick (`Pause`, polled waits).

### 3.4 Condition for deleting stackful

On `bench_depth` at depth 32, 1000 instances, release build, stackless with the trampoline is within 1.5x of stackful (soft gate), and the structural test in §3.2 shows zero ancestor dispatch on a pending re-poll (hard gate). And every parity test in the shared suite passes on stackless alone, natively, on WASI and on ESP-IDF QEMU. When all hold, delete stackful in one commit and update `AGENTS.md`, the docs and CI in the same commit.

## 4. Compose cache for functions

A function body composes once per distinct key and is shared by every call site with that key:

```
key = (function definition hash, input type, [param types], [types of Uses/Mutates bindings])
```

Call sites with the same key share the compiled body (`Arc`). Each call site still instantiates its own frame and its own nested states. This closes the hole where `Do` composed inline per call site and bypassed the cache.

Reload: the compiled body is immutable. A changed function definition composes to a new body under a new key; call sites in unchanged callers pick it up at their next invocation entry (the existing revision registry, keyed per call site, is reused). An in-flight invocation finishes on its pinned body; a recursive group is pinned as a unit (§2.5). For a `Stateful` function whose `Keep` set changed: match `Keep` slots by name and type (not initializer: it runs only at instantiation, so editing `Keep(n 0)` to `Keep(n 10)` does not reset a live counter); retain the ones that match, initialize new ones, report every reset by name; native shard state survives only under an explicit compatibility contract, otherwise it resets; never reset the caller. Whether a reset is applied or the reload is rejected is host policy: `watch` applies and reports, an embedding host rejects unless it passes a reset policy. `r` in `watch` remains the full restart for when the user does not trust a retained state.

## 5. Effects on `ShardDesc`

```rust
pub struct Effects {
  pub suspends: bool,
  pub io: bool,
  pub time: bool,
  pub random: bool,
  pub unknown: bool,   // host shard that declared nothing
}
```

- Native shards declare effects in their `ShardDesc`. A shard that declares nothing is `unknown: true`. Core catalog shards are all classified in this step.
- Compose computes a function's effects as the union over its body, including nested flows and called functions.
- Pure means: no `Keep`, no `Uses`/`Mutates`, and effects all false. `unknown` is not pure. `@fn` with a declared `Pure: true` fails compose if the body is not pure, with the diagnostic in §7.
- `describe` prints effects; `check --json` carries them per occurrence.

## 6. Authoring eval harness

`bench/authoring/`: a fixed task list (30 to 50 small tasks: a counter, a retry with backoff, a struct transform, a looped poll with `Pause`, a function with two params, a `Match` with default), the catalog reference generated by `otter catalog`, and a runner that feeds task plus reference to a model, runs `otter check --json`, feeds diagnostics back, and records first-pass success and repair rounds to success, capped at 5. Output is one CSV row per (task, model, variant). Variants are syntax alternatives under test. This is how taste disputes are settled from here on; the first run compares the final syntax against the pre-review syntax as a baseline.

The runner is a small Rust binary that shells out to a model CLI given on the command line; it does not embed any API client.

## 7. Acceptance tests

Each is a `.shs` script in `crates/otter-lang/tests/functions/` with the expected result, or the expected diagnostic pinned by code, span and symbol, with the human text under a snapshot test. All run on the stackless scheduler; while stackful exists they run on both. Parameter labels are lowercase (§1.5), `Input`/`Output` are declared, and `Stateful: true` marks functions that use `Keep`.

**7.1 Two call sites share one body, keep separate state.**
```
@fn(Counter Stateful: true Input: None Output: Int Params: {step: Int} {
  Keep(n 0)
  n | Math.Add(step) | Update(n)
})
@wire(main {
  none | Counter(step: 1) | Log
  none | Counter(step: 10) | Log
} looped: true)
@mesh(m) @schedule(m main) @run(m iterations: 2)
```
Expected log: `1`, `10`, `2`, `20`. Expected compose count for `Counter`: 1 (both sites have param type Int). Asserted through the compose cache's hit counter.

**7.2 Suspension inside a function resumes the frame, not the root.**
```
@fn(Slow Stateful: true Input: None Output: Int Params: {ticks: Int} {
  Keep(i 0)
  Repeat(times: ticks action: { Pause() i | Math.Add(1) | Update(i) })
  i
})
@wire(main { none | Slow(ticks: 3) | Log } looped: true)
@mesh(m) @schedule(m main) @run(m iterations: 8)
```
Expected: `Slow` completes after 3 suspensions, logs `3`, then on the next invocation `i` continues from 3 and logs `6`. Expected ticks per invocation: 4. Structural assertion (§3.2 hard gate): after the first `Pause`, each re-poll dispatches the `Pause` frame and no composite frame; the composite dispatch counter stays at zero until `Pause` completes.

**7.3 Cancellation discards invocation state, cleans up exactly once.**
Same as 7.2 but the mesh is dropped after tick 2. Expected: `Slow`'s cleanup runs once, `Pause`'s cleanup runs once, no further activations.

**7.4 Misspelled update is a compose error.**
```
@fn(Acc Input: None Output: Int {
  0 | Var(total)
  1 | Update(totl)
  total
})
```
Expected: code `unknown-variable`, span at `totl` on line 3, symbol `totl`, suggestion `total`.

**7.5 Reading a caller local is a compose error.**
```
@fn(Inner Input: None Output: None { x | Log })
@wire(main { 5 = x  none | Inner() })
```
Expected: code `unknown-variable`, span at `x` on line 1, symbol `x`, and the help names the caller's local and the parameter rewrite (`Inner(x: x)` with `Params: {x: Int}`).

**7.6 Undeclared mesh access is a compose error.**
```
@mesh(m) @mesh-var(m config: {rate: Float})
@fn(Rate Input: None Output: Float { config.rate })
```
Expected: code `undeclared-mesh-access`, span at `config` on line 2, symbol `config`, help `add Uses: [config]`.

**7.7 Pure claim that is false.**
```
@fn(Now Pure: true Input: None Output: Int { Time.Now })
```
Expected: code `not-pure`, span at `Time.Now`, symbol `Time.Now`, effect `time`.

**7.8 Keep without Stateful is a compose error.**
```
@fn(Acc Input: None Output: Int { Keep(n 0) n })
```
Expected: code `keep-in-stateless`, span at `Keep`, help `add Stateful: true to Acc`.

**7.9 Struct access is indexed.**
```
@fn(Area Input: None Output: Float Params: {p: {x: Float y: Float}} { p.x | Math.Multiply(p.y) })
```
Expected: compose output for the `Take` of `p.x` is an indexed read (asserted through the inline op kind in a unit test), and `p` at runtime is `TableRepr::Struct`.

**7.10 Reload adds a callee local without touching the caller.**
Preserving reload of a `main` that calls `Counter` after `Counter` gains a new `0 | Var(tmp)` local. Expected: `main`'s frame layout hash is unchanged, `main`'s `Keep` values survive, `Counter`'s `Keep(n)` survives (matched by name, type and initializer), the reload report lists `Counter` as retained.

**7.11 Reload changes a `Keep`.**
Same as 7.10 but `Keep(n 0)` becomes `Keep(n 0.0)`. Expected: `n` is reset and the report names it; `main` is retained. A second variant reorders two `Keep`s without changing them: both are retained (name matching, not position).

**7.12 Recursion (own milestone, §2.5).**
A tree fold over `{value: Int children: [...]}` summing values; mutual recursion `Even`/`Odd`; a recursive function with `Pause` inside; `@fn(Down Input: Int Output: Int { Down })` with `max_call_depth: 8` failing with code `recursion-limit`, function `Down`, depth 8, cleanup once; and a cycle through a `Stateful: true` function failing with code `recursive-stateful`.

**7.13 Direct resume cost.**
`bench_depth` at depths 1, 4, 16, 32, 64 after §3.2, recorded in `docs/runtime-performance-overview.md`, replacing the earlier stackless column. Pass condition is §3.4.

## 8. Order of work

1. `Var` API (§1.1), with `Map` only, so hosts move off `BTreeMap` immediately. Small, unblocks the external host.
2. Lowercase parameter labels (§1.5) across the catalog, and effects on `ShardDesc` (§5) with signature inference in compose, printed by `describe` and `check --json`. Mostly existing compose data.
3. The trampoline (§3.2): every composite rewritten once onto the request protocol, structural test, `bench_depth`; delete stackful when §3.4 holds. This goes before functions because the function call is itself a composite and should be written once, on the final protocol.
4. Function frames and call sites (§2.1 to §2.4), with the companion spec's `@fn` syntax; tests 7.1 to 7.8, 7.10, 7.11. Delete `Do` in the same change.
5. Struct tables (§1.2) and indexed `Take`; test 7.9. Then the storage benchmark from `values-and-types.md` §7 against the old BTreeMap, recorded once.
6. Recursion (§2.5); test 7.12.
7. Authoring eval harness (§6), first run.

Each step lands with its tests on the shared suite and keeps the existing suites green. Steps 1, 2 and 5 are independent of each other; 4 needs 2 and 3; 6 needs 4.

## 9. Open questions

- **Shape interning lifetime.** Shapes join the type registry, which never frees. A reload soak test (1000 reloads introducing distinct record shapes) decides whether registry scoping is needed before or after this work.
- **Keep across failure.** If a function invocation fails after writing a `Keep`, the write stands (it is state, not a transaction). The companion spec should say this in the language rules; the runtime does nothing special.
- **Param slot writes on every call.** For a call site in a tight loop, writing params every invocation is a copy per param; with `Arc` values it is a refcount bump. If measurement shows this matters, a param whose argument is a literal can be resolved at compose and skipped at runtime.
- **Tail calls.** Frame reuse for self tail calls is possible on the trampoline but is not part of the recursion milestone; correct recursion with the depth limit comes first.
