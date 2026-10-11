# Stackless Scheduler Experiment

**Status:** Experiment complete (2026-10-04), including matched comparisons with 1.x (C++).

**Outcome (2026-10-07):** the stackful scheduler was deleted after the golden path M4 gate passed ([golden-path.md §6.4](golden-path.md#64-gate-for-deleting-stackful)); the direct-resume engine described in the M4 follow-up at the end is the only scheduler. The decision paragraph, the shard-API discussion and the measurements below are kept as the historical record of why.

**Latest measurements:** the [2026-10-05 runtime overview](runtime-performance-overview.md)
refreshes steady-state, resume depth, async and HTTP results after VM optimization.
The tables below retain the original experiment's evidence.

**Decision (2026-10-04, maintainer; reviewed by Astra):** the stackless scheduler is the default, and both schedulers are maintained. Each mesh uses one backend; there is no cross-backend `Do`. A workload dominated by deep, frequent resumes can run its mesh on the stackful backend. Leaf and async shards have one shared implementation; only shards that suspend or run nested flows have two. Runtime tracing (§6) now informs optimization and deployment choices; it no longer gates the decision. Targets without the stackful backend (wasm, and ESP-IDF since 2026-10-05, see [esp32.md](esp32.md)) run stackless only.
**Question:** Can a stackless scheduler preserve the prototype's semantics with an acceptable shard API? (Design doc §3.4; contract §6-§7 left the suspension representation open.) The 4 KB per instance it might save is secondary.
**Scope kept:** same compiled/state model, same compose, same acceptance suite. The stackful scheduler stays as the reference.

---

## 1. What was built

Code: `crates/otter-core/src/stackless/`.

- **Compose is shared, not duplicated.** Compose became generic over a small `Backend` trait (the kind of compiled node). Both schedulers use the same wire definitions, cache, dependency recording and definite-initialization checks. Compose logic for every shard lives once, in `shards/mod.rs`, as generic helpers; both implementations call them. The stackful code path is unchanged in behavior (`Backend` defaults to `Stackful`).
- **Suspension is a return value.** A stackless `activate` returns `Step`: the stackful `Flow` variants, plus `Suspend`.
- **Resume points live in state.** A stackless `FlowState` records which node suspended and that node's input, and resumes there. A shard that runs nested flows keeps its own resume point (a phase, a loop counter) in its `State`.
- **The scheduler is a loop.** Each tick activates every running instance's root flow; each nesting level continues from its saved point. No coroutines, no stacks.
- **Same contract.** The instance is the panic boundary, cleanup runs exactly once for every instantiated state, and a cancelled instance is never activated again. Cancelling is just cleanup: a suspended stackless instance is only data, so there is no stack to unwind.

## 2. Semantics: results

The acceptance suite (`tests/prototype.rs`) now runs unchanged against both schedulers, generated once per scheduler by a macro. Two tests were added for resume correctness, which matters mostly for the stackless scheduler:

- **Suspension inside every composite:** a `Pause` inside `Once`, `When`, `Repeat` (3 iterations) and `While`. Both schedulers must produce the same result (313) after the same number of ticks (7 suspensions).
- **Deep resume:** a `Probe` before a never-ending `While { Pause }`, two levels deep through `When` and `Do`. Resuming must never re-run nodes that already ran (exactly one activation each), and a cancel from deep inside must clean up exactly once.

| | Stackful | Stackless | Stackless on wasm |
|---|---|---|---|
| Acceptance tests | 15/15 | 15/15 | 12/12 (3 panic tests excluded, see §4) |

This includes Astra's nested suspension test (two instances of one shared compiled sub-wire, suspended at different points; cancel one, resume the other), the panic paths, partial instantiation, cache behavior and state isolation.

Mutation check: making the stackless `FlowState` restart from the beginning instead of resuming fails 4 tests (deep resume, every-composite, nested suspension, state isolation).

## 3. Measurements

**Separate execution-throughput evidence (2026-10-05):** the
[VM execution benchmark](vm-execution-benchmarks.md) measures long uninterrupted
shard chains. Its initial baseline found substantial 1.x advantages on cheap
operations; the follow-up introduces a shared builtin executor and reports
the remaining gaps. The scheduler/instance results below do not establish
competitive per-shard throughput; the two suites measure different costs.

Release builds, Linux, i9-14900KF, 3 runs each. `bench_instances` is the entity benchmark from the design doc §5 (two nesting levels, one suspension per iteration); `bench_depth` isolates resume cost.

**Creation and memory per instance** (spawn through first activation; `ps` current RSS; live heap counted by the allocator):

| | Stackful | Stackless |
|---|---|---|
| Time per instance (100 / 1000 / 10000) | 4.6-11 / 5.1-7.1 / 4.0-4.4 µs | 0.8-1.0 / 0.9-1.0 / 0.8-0.9 µs |
| Resident memory per instance | 4.8 KB | 0.93-0.96 KB |
| of which live heap | 0.62-0.68 KB | 0.79-0.90 KB |
| of which non-heap (stack pages) | ~4.1 KB | ~0.03-0.14 KB |
| Shard state + locals (runtime accounting) | 656 B | 968 B |

Stackless state is ~310 bytes larger: resume points (each flow state stores its resume node and its saved input; `Pause` stores its start time; composites store a phase or counters). It no longer needs a touched stack page, so the total is about 5× smaller.

**Matched comparison with 1.x (C++)**, added after review: the same entity logic and nesting, the same instance counts, the same number of suspensions per tick, Release builds on the same machine. Each run warms up for 10 ticks, then times 1000 ticks as a batch (no throttling: 1.x's `@run` ran 100 empty ticks in 0.012 ms). Each run also counts its completed work, which matched the expected count exactly in every run on all three runtimes: entity iterations = instances × 500 (two ticks per iteration on all three), resumes = instances × ticks. Numbers are total mesh time per tick divided by instances; 3 runs each. The 1.x scripts are in `bench/shards-1x/` (`run.sh`).

*Steady state* (the entity from `bench_instances`; ns per instance per tick):

| Instances | 1.x C++ | 2.0 stackful | 2.0 stackless |
|---|---|---|---|
| 100 | 103-111 | 145-249 | 146-158 |
| 1000 | 188-201 | 163-176 | 118-123 |
| 10000 | 634-656 | 286-296 | 116-120 |

*Pure resume against nesting depth* (`bench_depth`: `depth` nested `Do`s, then a loop at the bottom that increments a counter and pauses forever, so every tick is a pure resume; 1000 instances; ns per instance per tick):

| Depth | 1.x C++ | 2.0 stackful | 2.0 stackless |
|---|---|---|---|
| 1 | 129-144 | 131-132 | 117-131 |
| 4 | 139-147 | 125-128 | 141-172 |
| 16 | 169-175 | 136-138 | 282-414 |
| 64 | 171-178 | 143-144 | 918-1385 |

What this shows:

- **Resume is where stackless loses.** Stackful resume (1.x and 2.0) is close to constant: it switches straight back into the suspended frame. Stackless resume is linear in depth: it re-enters every level to reach the suspended node. In absolute terms, at depth 64 stackless adds about 0.75-1.2 µs per resume: for 10,000 instances resumed every frame, that is roughly 7.5-12 ms per frame, which is very significant. For a few hundred instances resumed occasionally, it is negligible.
- **The 2.0 stackful prototype is not slower than mature 1.x on these workloads**, except at small instance counts (100), where 1.x is faster: its per-tick overhead is lower (the prototype has dynamic dispatch, `RefCell` borrows and a `catch_unwind` per resume, none of it optimized yet). At 10,000 instances 1.x slows down sharply (634-656 ns), plausibly from poor cache locality across 10,000 instances of ~45 KB each plus their stacks; not investigated.
- **Stackless steady state stays flat** with instance count (~118 ns at 1000 and 10000), consistent with its small, contiguous per-instance state.
- **Variance:** the 2.0 stackful runs at 100 instances (145-249) and the deep stackless runs vary more than the others; the ranges are reported as measured.
- When a wire runs a full iteration per tick (as the entity does), every scheduler pays for the full nesting depth anyway, so the depth penalty only shows when instances resume deep without completing.

## 4. Wasm

The stackless scheduler builds and runs on `wasm32-wasip1` with no Asyncify and no JSPI: the 12 non-panic acceptance tests pass as a wasm binary under Node's WASI (`scripts/run-wasi.mjs`, also run in CI). The stackful scheduler cannot be built for wasm at all; `corosensei` has no wasm support, and 1.x needs Asyncify for the same reason (see the 1.x CLAUDE.md on JSPI).

Panics: `wasm32-wasip1` aborts on panic, so the per-instance panic boundary cannot hold on wasm for either scheduler. The 3 panic tests are compiled only where `panic = "unwind"`. This is a platform limit, not a scheduler difference. Containing panics on wasm would need wasm exception handling and an unwinding standard library build.

Not yet run: a browser (`wasm32-unknown-unknown`) build, and wasm performance.

## 5. What changes for shard authors

(See also §7: leaf and async shards were later moved to a single implementation shared by both schedulers.)

Out of 16 shards:

- **Compose: no change.** All 16 share one compose implementation across both schedulers.
- **Leaf shards (10: `Const`, `Set`, `Update`, `Get`, `Inc`, `Add`, `IsLess`, `IsMoreEqual`, `Spawn`, `Probe`): only the return type changes**, `Flow::Next(v)` to `Step::Next(v)`. Mechanical.
- **`Do`: no resume state needed.** Its nested `FlowState` resumes itself; `Do` just forwards `Suspend`.
- **Shards that suspend or run nested flows (5: `Pause`, `When`, `While`, `Once`, `Repeat`) need an explicit resume point in `State`:**
  - `Pause`: the time it started waiting (`Option<Instant>`), instead of a local variable across `ctx.suspend()`.
  - `When`, `While`: a phase (predicate or body suspended), so the predicate is not re-evaluated on resume.
  - `Once`: whether the body is mid-run, so a suspended first run is resumed instead of skipped.
  - `Repeat`: the iteration count and the iteration total captured at start.

  Code size: the stackless versions of these five are roughly 1.5-2× the stackful ones.

**The new obligation, and its risk.** A shard that can suspend must reset its resume point whenever it returns anything other than `Suspend`, including errors. Forgetting is a silent bug: the next activation would resume a stale phase. The prototype handles it with one helper (`settle`), used by every composite, and the resume tests catch mistakes in `FlowState`. In stackful code this class of bug does not exist, because the stack holds the resume point.

**Locals across suspension.** In stackful code, any Rust local (an iterator, a borrowed buffer, a partially built value) survives `ctx.suspend()`. In stackless code it must be moved into `State`, which rules out borrows across a suspension. This was a non-issue for the prototype's shards, but would matter for shards that do long, stateful work in a loop.

**Async I/O fits stackless naturally (analysis, not tested).** Rust futures are poll-based: a shard can keep its future in `State`, poll it on each activation, and return `Suspend` while it is pending. That is the stackless contract as is, so it fits the 1.x rule against blocking in `activate` without a coroutine. A stackful shard would instead poll and call `ctx.suspend()` in a loop.

## 6. Assessment

Against the question in the header:

- **Semantics: preserved** for this shard subset, including nested pause/resume/cancel, exactly-once cleanup (with panics, where unwinding exists) and isolation, on both native and wasm.
- **Shard API: acceptable but not free.** No change for compose and leaf shards. Shards that suspend or nest flows need explicit resume points, plus an invariant (reset on every non-suspend exit) that the stackful model does not have.
- **No broader redesign was needed.** The compiled/state model, compose and the contract stayed as they are; the only structural change is `Backend`-generic compose.
- **Costs and benefits:**
  - about 5× less memory per instance, and 4-5× faster instance creation;
  - steady-state cost that stays flat as instance count grows (faster than both stackful runtimes at 1000+ instances);
  - runs on wasm without Asyncify;
  - but resume cost is linear in nesting depth: slower than stackful (1.x or 2.0) once the suspension point is more than a few levels deep, and much slower at depth 16 and beyond.

Open before deciding (revised after review by Astra):

1. **Runtime suspension depth, weighted by resume frequency, not syntactic depth.** An AST census would measure the wrong thing: `Do` crosses wire definitions, loops change how often a suspension resumes, and native shards can suspend internally. A rarely resumed deep request and a deep loop resumed every frame cost very differently. The next step is to trace representative 1.x workloads: record the active depth at each suspension, and whether each resume made progress or only re-checked something still waiting.
2. **Polling.** Both prototype schedulers resume every running instance every tick, so a paused instance is re-checked even when nothing it waits for has changed. A scheduler that only resumes instances whose wait has completed could matter more than making each resume cheaper.
3. **Deep-resume mitigation is a hypothesis, not a contained optimization.** While a child stays suspended, a scheduler could resume it directly instead of walking its ancestors. But when that child completes, the relevant parents still need their saved inputs, phases, loop counters, error propagation and cancellation handled. Worth testing; it does not by itself make stackless win everywhere.
4. **A realistic async shard on both backends**, e.g. an HTTP-style request holding a future, including cancellation while it is pending. This exposes the authoring cost the small prototype hides: moving locals into state, lifetimes, and resetting continuations.
5. **Browser wasm** (`wasm32-unknown-unknown`) and wasm performance. The WASI result shows portability, not browser readiness.

**Current judgment:** stackful has the stronger case for simplicity and deep resume; stackless has the stronger measured case for memory, flat scaling with instance count, and portability. Keep both as experimental backends for these bounded checks, but do not commit to maintaining both as production implementations, and do not choose on the depth crossover alone.

## 7. Follow-up: one implementation for both schedulers

Keeping both schedulers is affordable only if most shards are written once. Two prototype APIs (`crates/otter-core/src/shards/`) do that; the acceptance target was one leaf implementation and one async implementation running unchanged on both backends, with matching success, failure, cancellation and cleanup behavior.

- **`LeafShard`** (`leaf.rs`): for shards that cannot suspend. "Leaf" means "never suspends", not "does little work": an I/O wrapper qualifies only if it returns without blocking. One implementation, with an activation context generic over both schedulers (`LeafCtx`: frames and instance id); its result is adapted to `Flow` or `Step` at the backend boundary. The 9 leaf shards (`Const`, `Set`, `Update`, `Get`, `Inc`, `Add`, `IsLess`, `IsMoreEqual`, `Probe`) now have a single implementation each, and their per-backend copies are gone. `Spawn` stays per-backend: its compiled form holds a backend-specific wire.
- **`AsyncShard`** (`async_shard.rs`): for shards that wait on an async operation. `start` builds the operation's future once per activation, from owned inputs; the adapter keeps that same future in the shard's state and polls it on every resume until it completes. While pending, the stackful adapter calls `ctx.wait()` and the stackless one returns `Suspend` after `ctx.set_waiting()`; both use the same per-instance waker. Cancellation drops the pending future before the shard's state, and resources held by earlier shards, are released. Futures need not be `Send`.
- **Wake modes, shared by both schedulers:** `WakeMode::PollEveryTick` resumes waiting instances every tick; `WakeMode::OnNotify` resumes a waiting instance only after its waker fired. Instances suspended for a tick (`Pause`) are resumed every tick in both modes.

**The realistic example** is `Request` (`sim.rs`), against a simulated service that behaves like async I/O: latency, failure, waker registration, wakeups on completion. Its cancellation semantics are explicit: an abortable request is aborted when its pending future is dropped; a detached one keeps running, completes later and fires a stale waker.

**Tests** (all on both schedulers; the async ones also on wasm):

- completion through nested flows (`When` → `Do` → `Request`), in both wake modes;
- failure, with cleanup exactly once;
- cancellation while pending: the request is aborted, and the abort happens before the earlier shard's resource is cleaned up;
- a late wakeup after cancellation (detached request) is harmless in both wake modes: the cancelled instance is never resumed;
- notified waiting polls exactly twice per request (start, and after the wakeup); polling every tick re-checks it every tick;
- `Pause` still resumes every tick in notify mode.

Mutation checks: creating a new future on every resume fails 5 async tests; not dropping the pending future in cleanup fails the abort-ordering test.

**Polling against notification** (`bench_async`: 1000 instances looping on a request with a 50-step latency; only `mesh.tick()` is timed; 1000 ticks; 3 runs; same completed work in every configuration, 19,000 requests):

| ns per instance per tick | Stackful | Stackless |
|---|---|---|
| Poll every tick (1,000,000 polls) | 47-49 | 15-18 |
| Resume on notification (38,000 polls: 2 per request) | 5.9-6.0 | 4.5-4.6 |

Notification-driven resumption matters more than the scheduler choice for waiting workloads: it is 3-8× cheaper here, on both. The prototype's notify mode still visits every instance each tick to check its flag; a ready queue would make idle instances free.

**Lifecycle helpers (after review, 2026-10-04).** Astra found three gaps, all in code that had been duplicated across flows and composites: a cleanup panic during rollback skipped earlier states (and, on the stackless scheduler, escaped `tick`); a composite whose body cleanup panicked skipped its predicate's cleanup; and finished stackless instances kept their input and locals alive. The cleanup policy is now written once (`lifecycle.rs`: `cleanup_each`, `instantiate_all`) and used by every flow and composite on both schedulers; finished instances release their execution data; `take_outcome` retires finished records. Tests for each combined-failure case run on both schedulers. The lesson: share lifecycle helpers as deliberately as shard logic.

**What remains doubled:** the shards that suspend or run nested flows: `Pause`, `When`, `While`, `Once`, `Repeat`, `Do`, plus `Spawn`. That is a small set today, but not a fixed one. Future modules may add shards that run nested flows (transactions, retries, timeouts), and each would need both implementations and parity tests. The boundary should stay explicit: a module that only adds leaf and async shards writes each once.

## 8. Follow-up: real async I/O (`otter-io`)

`crates/otter-io` validates the `AsyncShard` adapter against real sockets. It follows the 1.x HTTP module (`shards/modules/http`):

- **One shared runtime, outside the shards** (`runtime.rs`): a multi-threaded Tokio runtime with 4 workers, like 1.x's `TOKIO_RUNTIME`. A shard spawns its work there and gets an `IoTask`, which the `AsyncShard` adapter polls from the mesh thread with the instance's waker. Shards never touch the reactor directly, and the mesh never blocks.
- **`Http.Get`** (`http.rs`): the 1.x request path. The `reqwest` client is cached by configuration with a 50 s pool idle timeout. The request races a cancellation token. A non-success status fails with the (truncated) body. TLS is an explicit feature (`rustls-ring` or `native-tls`), as in 1.x. Differences from 1.x: the client cache is process-wide (1.x caches it per mesh), and only `url` and `timeout` are supported.

**What cancellation means, verified rather than assumed.** Dropping the pending `IoTask` cancels its token. The task drops its in-flight request future, and the connection closes: a local test server observes the client closing while it is still waiting to respond, both before the headers and midway through the body. Cancellation stops local polling and releases the request's resources. It cannot undo work the server already received (the server records the request as received).

Two library behaviors were confirmed by deliberately breaking the code:

- **Dropping a Tokio `JoinHandle` only detaches the task.** If `IoTask`'s drop does not cancel the token, the request keeps running and the connection stays open; both cancellation tests fail.
- **The 1.x pattern does not cancel body reads.** 1.x races only `send()` against its token, not `response.text()`. Replicating that, cancelling midway through a body leaves the connection open until the body completes or the request times out. `otter-io` races the body read too. This is inferred for 1.x from its code and reproduced in 2.0; it was not run against 1.x itself.

**Tests** (`crates/otter-io/tests/http.rs`, against a local server with controlled responses; all on both schedulers; 12/12, stable over 10 repeated runs and in Release):

- a successful response, in both wake modes;
- connection refused, and a 404 with its body in the error;
- cancellation while waiting for headers, and midway through the body: the connection closes, cleanup runs once, and late wakeups (the runtime finishing the cancelled task, the server being released) never resume the instance;
- another instance increments a counter on every one of 100 ticks while a request is pending;
- with notification-driven waiting, a request pending for 200+ ticks is polled at most a few times; polling every tick polls it 200+ times.

Native only. Browser integration (`fetch`, and the browser event loop instead of Tokio) is separate work.

## 9. HTTP concurrency: 1.x against 2.0

The I/O work in §8 validated the adapter; this measures what it changes. In 1.x, every in-flight `Http.Get` occupies a worker thread of the shared async pool for the whole request: `awaitne` schedules it on the pool (`TidePool`: 8 workers, growing up to 32), where `block_on` runs the request future until it completes, while the waiting wire re-checks every tick. In 2.0, requests run directly on the shared Tokio runtime; the mesh holds an `IoTask` and, with notification-driven waiting, re-polls it only when woken. The hypothesis from reading the code was that 1.x cannot have more than 32 requests in flight.

**Setup** (`bench/http-concurrency/`): one fixed-latency server shared by both runtimes (`latency_server`: 50 ms per request, every connection served concurrently, a 64-byte body, `Connection: close` so connection reuse favors neither client). It records its **peak number of concurrent requests**. Both clients run N wires, each doing one `Http.Get` with a 30 s timeout. Creating the instances (which in 1.x includes cloning and composing each copy) is timed separately from executing the precomposed requests. Each run also records the client's peak OS thread count (sampled from outside, Linux) and checks that every request completed (no failures in any run). Release builds, Linux, i9-14900KF, 3 runs per point.

| N | 1.x: time / server peak / threads | 2.0 stackless | 2.0 stackful |
|---|---|---|---|
| 1 | 51.8-51.9 ms / 1 / 14 | 51.6-51.7 ms / 1 / 5 | 51.6-51.7 ms / 1 / 5 |
| 32 | 205.9-206.1 ms / 10 / 16 | 52.3-52.5 ms / 32 / 5 | 52.4-52.9 ms / 32 / 5 |
| 100 | 506.8-508.7 ms / 13 / 19 | 61.2-62.6 ms / 100 / 5 | 58.8-61.2 ms / 100 / 5 |
| 1000 | 2488-2490 ms / 32 / 38 | 107-117 ms / 1000 / 5 | 105-126 ms / 1000 / 5 |

Instance creation: 1.x 4.2-25.6 ms, 2.0 stackless 0.001-0.16 ms, 2.0 stackful 0.01-6.0 ms (excluded from the times above).

What this shows:

- **The ceiling is real, and lower than predicted below saturation.** At 1000 requests the server never saw more than 32 at once in 1.x, the pool's maximum. At smaller N the effective limit was lower (10 at N = 32, 13 at N = 100), because the pool grows gradually from its 8 base workers. So 1.x completes requests in waves: 32 requests take about four 50 ms waves.
- **2.0 keeps every request in flight** (server peak = N) and finishes 1000 requests in about 2-2.5× the single-request latency, with a constant 5 threads (the mesh thread plus the 4 Tokio workers). The two 2.0 schedulers perform the same here: the requests' waiting dominates, not the scheduler.
- **Threads:** 1.x adds pool threads as requests queue (14 to 38); 2.0 stays at 5.

Scope: these are matched prototype benchmarks, not general 2.0-versus-1.x guarantees. The pool limit affects every `awaitne`-based shard in 1.x, not only HTTP, but only HTTP was measured. The 2.0 `Http.Get` supports a subset of 1.x's parameters; both use `reqwest` 0.13 over plain HTTP here.

To reproduce:

```sh
cargo test --workspace
cargo run --release --example bench_instances -- 1000                  # creation, memory, steady state
cargo run --release --example bench_depth                              # resume cost vs depth
cargo run --release --example bench_async                              # polling vs notification
cargo test -p otter-io                                                # real HTTP against a local server
bench/http-concurrency/run.sh path/to/1.x/shards                       # HTTP concurrency, 1.x vs 2.0
bench/shards-1x/run.sh path/to/1.x/shards                              # the same two benchmarks on 1.x
cargo test -p otter-core --test prototype --target wasm32-wasip1 --no-run
node scripts/run-wasi.mjs target/wasm32-wasip1/debug/deps/prototype-*.wasm
```

## Golden path M4 follow-up

The recursive stackless activation described above is now replaced by the iterative runner in `stackless/engine.rs`; the original M3 source at `c0aaa39` remains the benchmark oracle. Runtime frames are addressed by checked generation handles. Composites describe their immutable children to the runner and never activate children themselves. Pending leaf re-polls visit zero composite handlers, proven at depths 1, 4, 16 and 32. Full completion visits each parent once. Initialization, rollback and cleanup are iterative too. The frame arena contains no unsafe code and has targeted Miri checks for stale handles and generation exhaustion.

The gate (benchmark, native debug/release, docs-off, WASI, ESP-IDF acceptance on three chips, Miri on the arena, review) passed on 2026-10-07, and stackful, `corosensei`, `cfg(stackful)`, `--stackful` and the per-scheduler test macros were deleted in the following commit; `Mesh` is the one scheduler and `Shard` the one contract. The ESP-IDF acceptance build generates a direct runner from the shared core/frontend suites because the device has no libtest harness; panic-unwind tests and host filesystem watching remain target-gated. Very large diagnostic-input cases use a device-sized over-limit input, and named-call depth follows the device's default of 32. This does not change the benchmark workloads.
