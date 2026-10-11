# VM execution: 1.x against 2.0

Measured locally on 2026-10-05. This answers a different question from the
[scheduler benchmarks](stackless-experiment.md): how much does executing a
long chain of already-composed shards cost, with no suspension or scheduling
inside the timed region?

**Initial baseline:** the 1.x release executes these small, hot shard chains
substantially faster than the unoptimized 2.0 prototype. Both 2.0 backends show
the gap. Sharing compiled programs and reducing instance memory remain useful,
but those advantages do not establish competitive instruction throughput.
This is a measured optimization priority, not a reason to abandon the
compiled/state split or a conclusion about Rust versus C++ in general.

**Follow-up:** the [builtin executor rerun](#builtin-executor-follow-up-2026-10-05)
reaches roughly 1.x speed for the measured Add chains and most cheap value
operations. Collection construction, table lookup and shared-sequence Push
still miss the 2× target. Constructor caching was subsequently removed to
fix a retention regression; see the [review follow-up](#constructor-retention-review-follow-up).
The original measurements below remain the baseline.

## Method

The [reproducible harness](../bench/vm-execution/README.md) generates the same
script for all three engines. Each batch times a `Repeat` inside one running
wire; no `Pause`, async work, mesh ticking, logging, parsing, compose or
instantiation is timed. The inner iteration reads a seed, runs a chain,
saves the result and increments a counter. After timing, full result equality
and the exact counter are checked. Every calibration, warmup and retained
sample must pass, or the run aborts.

Widths 0, 8, 64 and 256 distinguish fixed loop/sink costs from marginal chain
costs. The same instantiated body runs two warmup batches followed by three
retained batches. Three independent processes per engine/cell yield nine
retained samples. Job and engine order are shuffled deterministically. All
engines use the same calibrated iteration count in each cell. See the harness
README for calibration thresholds and the precise meaning of every workload.

Machine: Linux x86-64, Intel i9-14900KF, pinned to performance core CPU 2;
`intel_pstate` with the existing `powersave` governor. Frequency/turbo were not
locked. No workspace builds or other benchmark jobs ran concurrently with
measurements.
This is a single-machine result; minimum/maximum and median absolute deviation
are retained, not a claim of machine-independent cycle counts.

Builds:

- **1.x:** adjacent checkout at `37597927e`, freshly rebuilt with
  `cmake --build ../shards/build/Release --target shards -j 4`.
  GCC 16.2.1, Release, `SHARDS_INLINE_EVERYTHING=ON`, debugger and Tracy off.
  The core compile command uses `-O3 -march=broadwell`, inlining/unrolling
  flags and `-ffast-math -fno-finite-math-only`; no `-fsanitize` instrumentation
  flag. The checkout reported a dirty dependency submodule; this is recorded
  in metadata, not represented as a completely pristine source tree.
- **2.0 default:** runtime at `77dd307`, fresh
  `cargo build --release -p otter-cli`, pinned Rust 1.98.1, default release
  settings and features; no `output-checks`.
- **2.0 sensitivity build:** same source, ThinLTO, one codegen unit and
  `target-cpu=broadwell`. This tests whether stronger compiler settings close
  the gap; it is not a runtime optimization patch.

The two default builds have different compiler settings, intentionally
reflecting their existing release configurations. Inputs avoid overflow,
NaNs and rounding-sensitive cases. The sensitivity build narrows the target
CPU/build-optimization difference but does not equate all C++/Rust semantics.

## Initial baseline results

Median **ns per complete 256-motif iteration**, including the fixed
seed/result/counter/Repeat overhead. Lower is better. A motif is one shard
for Const/Get/Update/Add, two for Take/independent assignment, a call plus an
Add for Do, and a frontend expression for construction. Do not read this as
an equal-shard-count comparison across rows.

| Workload | 1.x | 2.0 stackful | 2.0 stackless | Stackless / 1.x |
|---|---:|---:|---:|---:|
| Integer Add | 290.4 | 3,923.2 | 3,884.2 | 13.37× |
| Float Add | 371.2 | 4,039.7 | 4,087.0 | 11.01× |
| Float4 Add (variable operand) | 362.2 | 4,831.8 | 4,829.4 | 13.33× |
| Integer Const | 122.6 | 2,446.6 | 2,445.4 | 19.95× |
| String Const (256 bytes) | 127.9 | 3,838.9 | 3,856.5 | 30.16× |
| Integer Get | 126.9 | 3,259.3 | 3,336.0 | 26.30× |
| Sequence Get (64 elements) | 200.3 | 4,643.0 | 4,686.0 | 23.39× |
| Get + table field Take | 860.4 | 11,064.9 | 11,854.6 | 13.78× |
| Do with one integer Add | 4,799.0 | 8,528.3 | 8,015.1 | 1.67× |
| Construct 4-element sequence | 1,400.8 | 11,076.2 | 11,210.2 | 8.00× |
| Construct 4-field table | 2,064.5 | 34,148.1 | 34,839.0 | 16.88× |
| Append to shared sequence | 690.9 | 9,926.9 | 10,436.3 | 15.11× |
| Independent integer assignment | 447.4 | 7,045.5 | 7,135.4 | 15.95× |
| Independent string assignment | 1,239.8 | 11,463.4 | 11,475.3 | 9.26× |
| Independent sequence assignment | 16,783.5 | 11,396.7 | 11,856.2 | 0.71× |
| Independent table assignment | 19,936.0 | 11,401.4 | 11,415.4 | 0.57× |

This confirms a large gap in cheap shard throughput. It also shows a real
benefit from 2.0's ownership choice: independent sequence/table assignment is
faster because sharing avoids copying the collection contents. These sizes
are examples, not a measured crossover curve over collection size.

The `update-*` chains are kept separately in the raw results: after the first
Update, 1.x forwards the destination's address and later updates become
self-assignments. The `assign-*` cases above explicitly re-read the original
source before **every** update. Treating these as equivalent copy benchmarks
would substantially misrepresent 1.x's behavior.

Raw measurements, calibration, summaries, fitted slopes and build provenance:
[main suite](../bench/vm-execution/results/2026-10-05-default/),
[independent assignment](../bench/vm-execution/results/2026-10-05-assign/).
All results and completion counts passed. Widths 0/8/64/256 and all retained
samples are available; the table does not select the fastest single sample.
Fits are descriptive only: the 1.x self-assignment sequence/table curves have
R² around 0.97, so a constant slope is a less complete description there.

### Compiler-settings sensitivity

Same 256-motif iterations, with nine retained samples per cell again:

| Workload | 1.x rerun (ns) | 2.0 default stackless (ns) | 2.0 tuned stackless (ns) | Tuned / 1.x rerun |
|---|---:|---:|---:|---:|
| const-int | 122.9 | 2,445.4 | 2,485.3 | 20.22× |
| const-string | 128.0 | 3,856.5 | 3,787.2 | 29.58× |
| get-seq | 199.1 | 4,686.0 | 4,699.5 | 23.60× |
| add-int | 288.4 | 3,884.2 | 3,866.7 | 13.41× |
| add-float4 | 363.1 | 4,829.4 | 4,794.9 | 13.20× |
| assign-seq | 17,029.1 | 11,856.2 | 10,748.1 | 0.63× |
| assign-table | 16,758.8 | 11,415.4 | 10,707.7 | 0.64× |

The stronger build does **not** close the dispatch/value-transfer gap. Its
stackful results are also retained in the [sensitivity data](../bench/vm-execution/results/2026-10-05-lto/).
This is not an attribution of every small difference to compiler flags: the
unchanged 1.x table-assignment binary also ran faster in the second suite
(about 16.8 µs versus 19.9 µs). Independent suite/build runs can differ beyond
within-run dispersion. The large hot-path gap and the collection-sharing
advantage persist in both suites.

Reproduce the sensitivity build and selection with:

```sh
CARGO_TARGET_DIR=/tmp/shards-vm-lto CARGO_PROFILE_RELEASE_LTO=thin \
  CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 RUSTFLAGS='-C target-cpu=broadwell' \
  cargo build --offline --release -p otter-cli
python3 bench/vm-execution/run.py ../shards/build/Release/shards \
  --otter /tmp/shards-vm-lto/release/otter --source1 ../shards --cpu 2 \
  --cases const-int const-string get-seq add-int add-float4 assign-seq assign-table \
  --widths 0 64 256 --out target/vm-execution/tuned-run
```

## What the baseline execution paths explain

These describe the initial baseline before the builtin executor below. They
are code/assembly observations, not a profiler's allocation of the measured
time to individual causes:

1. **1.x has specialized built-in dispatch.** Its
   `shards/core/runtime.cpp::shardsActivation` walks a null-terminated pointer
   array, calls `activateShardInline`, checks control flow and forwards the
   returned pointer. `shards/modules/core/inlined.cpp` directly handles
   `Const`, `Get`, variable operations and specialized arithmetic, among
   others. A constant can return its stored value's address; a local read can
   return its cell. Generic extension shards need not get these fast paths.
2. **2.0 pays for owned outputs and a general result envelope.** The current
   `Var` is 24 bytes/alignment 8 on this target, versus 1.x's 32-byte,
   16-aligned `SHVar`. However, `Flow`/`Step` are 32 bytes and their `Result`
   envelopes are 40 bytes. Smaller `Var` size alone does not predict the cost
   of forwarding it through an activation call. Release disassembly of
   `CompiledFlow<Stackless>::activate` shows an indirect call, result/tag
   handling, multiple stack value moves, and drop handling between nodes.
3. **Reference-counted reads are not pointer forwarding.** `Const` and `Get`
   return cloned values; strings, sequences and tables use `Arc`. The inspected
   release code contains `lock incq` in `Var::clone` and `lock decq` in flow
   value replacement. This is real work even without thread contention.
4. **Compose does not yet specialize 2.0 arithmetic as aggressively.** The
   shared `math::arith` checks runtime variants and supports mixed numeric
   shapes. Float vectors go through f64 components before becoming f32 vectors
   again; 1.x has dedicated vector activation cases. Integer overflow checks
   also differ. The arithmetic result includes these implementation/semantic
   differences, not just dispatch.
5. **Collection construction has different reuse behavior.** The same source
   expression lowers differently in each frontend. These cases include the
   resulting execution and allocation/reuse choices. They must not be reported
   as equal numbers of shard dispatches or as evidence about only enum layout.

The 2.0 stackful loop uses `iter().zip()`; the stackless loop uses explicit
indexing. Their similar results show that switching schedulers or blaming
iterator syntax does not account for the large observed gap. This is not an
isolated A/B proof that every iterator formulation compiles identically.

## Baseline decisions and limits

Keep the shared immutable compiled artifacts and per-instance state model.
Before treating the hot execution contract as settled, measure changes to:

- borrowed/pass-through results or stable output slots, with explicit lifetime
  rules across variable writes and suspension;
- compose-selected builtin activation paths, including typed arithmetic;
- the size and movement of activation results and avoidable clone/drop pairs;
- table construction reuse and compose-resolved field access;
- state erasure/downcast costs and state storage, separately from the above.

Do not introduce unchecked borrowing or raw-pointer lifetime assumptions just
because 1.x is faster here. Each candidate needs the same result checks, both
backends' acceptance suites, and reruns of the instance/scheduler benchmarks.
The current measurements do not isolate the contribution of `Any` downcasts,
bounds checks, allocator traffic or cache misses; no hardware-counter profile
was obtained.

This suite excludes many-instance scaling, creation/compose time, deep
suspension/resumption, I/O, graphics, browser/ESP32 execution, unique-buffer-only
mutation and generic host-shard dispatch. Table-field mutation is absent
because the matching 2.0 shard is not implemented. Hot repeated motifs favor
predictable branches and cache residency. Whole applications mix these costs
with work that may dominate them; no application-wide speed ratio is claimed.


## Baseline validation

All timed workloads passed their value and completed-work checks on all three
engines. The final generator reproduced every measured script byte-for-byte;
`scripts.sha256` in each result directory records their hashes. Parser tests
reject wrong results/counts, missing/extra samples and invalid durations.

Local repository checks passed: workspace check, formatting (including the
ESP32 workspace), all-target clippy with warnings denied, workspace tests,
docs-off core/IO metadata and catalog tests and frontend tests, release
nesting-limit tests, rustls-ring clippy, wasm core/frontend clippy, and the
prototype/metadata/frontend WASI test binaries under Node. The HTTP tests
needed loopback sockets enabled outside the restricted sandbox. No runtime
source or dependencies changed. CI results are separate from these local
checks.

## Builtin executor follow-up (2026-10-05)

The revised default release build reaches roughly 1.x throughput for the
measured integer, Float and Float4 Add chains. This is a runtime change,
not an LTO or fast-math build. The same `Var` enum still measures 24 bytes;
changing how values travel through the executor accounts for these gains.
The 2× target is **not** met across all collection workloads.

Both schedulers use one shared executor for compose-selected core operations.
The useful 1.x ideas implemented here are:

- **Typed builtin dispatch:** compose selects Const/Get/Set/Ref/Update/Inc,
  Int/Float/Float4 Add, Take and non-clearing Push. Rust implementation identity
  selects a builtin, so a host shard with the same name is not substituted.
- **Borrowed value forwarding:** constants and frame reads forward pointers
  within an uninterrupted segment. Assignment clones before replacing its
  destination, then forwards that destination; self-assignment skips the copy.
  The executor returns an owned snapshot before generic activation or suspension.
- **Reusable numeric storage:** explicit numeric variants use a call-local
  `MaybeUninit<Var>` slot that needs no destructor dispatch. General results
  use ordinary drop-managed scratch. Checked integer overflow is retained.
- **Direct vector arithmetic:** Float4 + Float4 adds `f32` lanes directly;
  the default x86-64 release emits packed `addps`, with no explicit intrinsics
  or target-specific source. Mixed scalar/vector inputs use the existing
  arithmetic rules. The [assembly excerpt](../bench/vm-execution/results/2026-10-05-inline/simd.txt)
  is tied to the binary hash in the measurement metadata.
- **Collection buffer reuse:** Seq.Make and Table.Make keep an output buffer
  in instance state and reuse it only when no saved output shares it. Shared
  outputs trigger a fresh allocation. Compiled artifacts remain immutable.

### Matched rerun

Same machine, CPU affinity, default release settings and complete harness
method as above: all 25 workloads, widths 0/8/64/256, three independent
processes and nine retained samples per engine/cell. No builds or other
benchmarks ran concurrently. All calibration, warmup, value and iteration
checks passed. The table again gives complete 256-motif iteration medians,
not isolated instruction latency or the fastest sample.

| Workload | 1.x ns | 2.0 stackful ns | 2.0 stackless ns | Stackless / 1.x |
|---|---:|---:|---:|---:|
| add-int | 294.9 | 190.3 | 186.2 | 0.63× |
| add-float | 368.8 | 357.0 | 357.6 | 0.97× |
| add-float4 | 369.0 | 372.0 | 369.9 | 1.00× |
| const-int | 124.3 | 126.5 | 127.5 | 1.03× |
| const-string | 139.3 | 131.0 | 128.4 | 0.92× |
| get-int | 125.6 | 199.4 | 194.1 | 1.55× |
| get-seq | 202.4 | 203.3 | 197.5 | 0.98× |
| update-int | 326.2 | 248.7 | 246.2 | 0.75× |
| assign-int | 440.2 | 579.6 | 571.2 | 1.30× |
| assign-string | 1,288.5 | 2,589.2 | 2,584.3 | 2.01× |
| assign-seq | 17,236.0 | 2,060.8 | 2,063.8 | 0.12× |
| assign-table | 17,127.1 | 2,058.2 | 2,053.0 | 0.12× |
| take-seq | 1,567.7 | 653.1 | 653.4 | 0.42× |
| take-table | 875.6 | 3,334.3 | 3,206.9 | 3.66× |
| do-int | 4,860.1 | 6,931.1 | 6,378.9 | 1.31× |
| push-shared-seq | 734.9 | 2,536.3 | 2,533.0 | 3.45× |
| make-seq | 1,380.0 | 10,760.3 | 11,399.0 | 8.26× |
| make-table | 2,012.0 | 10,975.7 | 11,398.4 | 5.67× |

[Full raw data, calibration, summaries and slopes](../bench/vm-execution/results/2026-10-05-inline/)
include the omitted Const/Get/Update variants. Source provenance is the
working tree based on `98a75c6`, containing this optimization; metadata does
not imply the binary came from that clean baseline. `runtime-source.sha256`
records the measured core source files (including the new executor), and
`scripts.sha256` records the generated scripts. This commit carries the
implementation and data together.

### Remaining costs and scope

Table Take still performs a BTreeMap key lookup. The 1.x implementation has
fixed-field indexing and a cached table-value path; these are candidates for
future compose-resolved storage/access work. A cached raw pointer needs an
explicit ownership/invalidation contract, not just the 1.x address trick.
Push still pays copy-on-write uniqueness checks and growth; construction
still goes through generic leaf activation with owned Arc results. The same
source also lowers differently for construction. The measurements do not
attribute every remaining nanosecond to one of these mechanisms.

Independent string assignment is around the 2× boundary; sharing still wins
strongly for the tested sequence/table assignments. The specialized math
coverage here is Add on the listed types, not every math operator or shape.
Generic host shards keep their existing APIs and dispatch. Compiled flows
now retain an extra instruction array; constructor buffers occupy per-instance
state. Neither program-size cost nor arbitrary application throughput is
established by the hot-chain results.

### Correctness and local validation

The raw-pointer accumulator is confined to one executor call over exclusive,
fixed-size frames. No arbitrary callback or suspension occurs inside it;
reads end before storage replacement, and the accumulator is reanchored
after writes. Output checks remain enabled in debug builds and with the
release `output-checks` feature. The lifetime contract is documented in
`inline.rs` and the shard contract.

The final implementation passed the complete local check set listed under
baseline validation, plus release core unit/acceptance tests with
`output-checks`. New shared tests cover snapshots across suspension and
nested writes, assignment aliases, and saved constructor outputs. Four
executor tests pass Miri on `nightly-2026-10-03`, including differential
owned-value execution, numeric scratch reuse, self-push and replacing a
collection after extracting an element. Miri is additional evidence, not a
proof of all executions. CI results are recorded separately from local checks.

The instance and depth examples were also rerun against an isolated build
of `98a75c6`, with default release settings, CPU 2 and three alternating
process rounds. `bench_depth` now uses 32 instead of 64 for its last depth
because the current compose limit is 48; the same one-line adjustment was
applied to the baseline example. All work counters matched.

| 10,000-instance workload | Baseline | Builtin executor |
|---|---:|---:|
| Stackful ns / instance tick | 299.2 | 279.3 |
| Stackless ns / instance tick | 119.6 | 92.1 |
| Stackful ready µs / instance | 3.28 | 3.16 |
| Stackless ready µs / instance | 0.90 | 0.79 |

This workload's per-instance state and measured heap remained unchanged
(696 bytes / 1.21 KiB stackful, 1,008 bytes / 1.47 KiB stackless). It does
not use the collection constructors and therefore says nothing about their
new buffer cost. Resume medians improved at most depths; stackless depth 32
increased from 535.1 to 546.6 ns/instance/tick (2.1%). This small three-run
sample is not evidence of zero regression on every workload.
[Commands, binary hashes and every output](../bench/vm-execution/results/2026-10-05-inline/scheduler-rerun.json)
are retained alongside the hot-chain data.

## Constructor retention review follow-up

[Review F1](../.agent-handoffs/reviews/2026-10-05-7c974be-claude-929d96.md)
identified a regression in the buffer caches introduced at `7c974be`:
constructing `[acc]` or `{a: acc}` and discarding that output still left the
captured accumulator in constructor state. A subsequent Push detached and
copied the growing sequence on each iteration, producing quadratic work.
Captured strings and tables were also retained beyond the output's lifetime.

Seq.Make and Table.Make now have unit state again and keep no output cache.
The builtin executor recognizes constructor segments and consumes their
incoming value. `Arc::try_unwrap` recovers uniquely owned Vec/BTreeMap storage;
shared inputs are left intact and a fresh buffer is used instead. Consecutive
constructors work on an owned buffer and wrap it in an Arc only at a boundary.
Table constructors sort compiled entries once and reuse matching key layouts;
shape changes rebuild the keys. No buffers or captured values remain in shard
state after output consumption. This path uses safe Rust.

Debug and release `output-checks` builds materialize each intermediate output
for validation. Normal release execution avoids those copies. Numeric scratch
and SIMD arithmetic remain unchanged. The historical measurements above still
describe `7c974be`; the corrected construction results are recorded separately
below. Constructor reuse is limited to consumed input and uninterrupted
constructor runs, not a cache persisting across arbitrary shard activations.

The shared `discarded_sequence_constructor_releases_captured_values` and
`discarded_table_constructor_releases_captured_values` regressions compare
Arc owner counts before activation and while paused after consuming the
output with Count. They cover captured sequences, strings and tables while
the constructor's instance is still alive. Both tests fail on both native
backends with the old cache (three owners instead of two for the sequence)
and pass after its removal. Saved-output snapshot coverage remains in
`collection_outputs_keep_saved_snapshots`. These deterministic ownership
checks guard the cause of the regression without a wall-clock threshold.

The corrected build was rerun with the complete matched suite: 25 workloads,
four widths and nine retained samples per engine/cell. All value/count checks
passed. [Raw measurements and source fingerprints](../bench/vm-execution/results/2026-10-05-owned-constructors/)
are from the working tree based on `b7cfa9b`, containing this review fix.
Selected full 256-motif iteration medians (ns):

| Workload | 1.x rerun | 2.0 stackful | 2.0 stackless | Stackless / 1.x |
|---|---:|---:|---:|---:|
| make-seq | 1,370.4 | 4,439.0 | 4,437.6 | 3.24× |
| make-table | 1,524.2 | 4,695.0 | 4,686.4 | 3.07× |
| add-int | 289.5 | 217.2 | 218.2 | 0.75× |
| add-float | 364.7 | 359.4 | 361.5 | 0.99× |
| add-float4 | 360.5 | 371.6 | 367.0 | 1.02× |

Compared with the previous 2.0 medians, sequence construction fell from
11,399.0 to 4,437.6 ns and table construction from 11,398.4 to 4,686.4 ns.
They remain over the 2× target against the paired 1.x rerun. The unchanged
1.x table workload also ran faster in this suite than the earlier suite;
compare paired ratios and retain the machine/frequency caveats above.

The residual collection work differs concretely: 1.x's `Const` uses a
`VariableResolver` prepared during warmup. In `shards/core/foundation.hpp`,
`reassign` copies SHVar fields directly between resolved pointers in an
existing collection template. Rust still clones/drops owned Var elements.
Likewise, 1.x Take has cached-field/fixed-index paths while Rust's table
lookup remains a BTreeMap lookup. These are implementation and ownership
contracts, not proof that C++ inherently executes equivalent work faster.
The 1.x resolver explicitly clears borrowed fields at cleanup to avoid
freeing their storage; a borrowed-view design in Rust would also need an
explicit lifetime contract.

The review's growing-accumulator scripts were run separately, at 10,000 and
40,000 iterations, three alternating process runs per variant/backend before
and after the fix, pinned to CPU 2. Every final count matched. At 40,000,
sequence medians fell from 2.465/2.474 seconds to 5.47/5.46 ms
(stackless/stackful); table medians fell from 2.462/2.460 seconds to
5.97/6.00 ms. These are whole-process wall times, including startup and parsing,
not isolated VM timings. [Scripts, commands, binary hashes and raw samples](../bench/vm-execution/results/2026-10-05-owned-constructors/retention-repro.json)
are retained. The owner-count regressions provide the deterministic guard;
timing alone does not prove an asymptotic bound.

The complete local check set passed again, including native and WASI shared
regressions, release nesting limits, docs-off tests and both clippy variants.
Release core tests with `output-checks`, frontend constructor tests with
`otter-core/output-checks`, and all five inline executor Miri tests also
passed. Independent verification of the review fix remains a separate step.

## Inline segment input lifetime review follow-up

A second review (`2026-10-05-a003cb7-claude-3dc841#F1`) found that even without
constructor state caches, `inline::run` borrowed the previous output for its
whole segment. `[acc]` followed directly by `1 | Push(acc Clear: false)` kept
that captured accumulator shared during Push, causing repeated full copies.
The earlier constructor-cache finding was independently verified as fixed;
this is a separate retention cause.

Both schedulers now move the incoming Var into the builtin executor. Const,
Get, Set and Inc release obsolete owned scratch when reanchoring; Take, Push
and generic arithmetic replace it with their owned result. Typed numeric
arithmetic can leave only resource-free numeric scratch, so its hot loop does
not need a general cleanup check. At segment exit, scratch output is moved,
while borrowed code/frame/numeric output is cloned into an owned snapshot.
No borrowed pointer crosses the existing callback/suspension boundaries.

`discarded_inline_input_does_not_copy_captured_sequence` checks actual Arc
allocation identity across constructor → Const → Push → Pause for sequence
and table constructors on both schedulers. It failed on both backends before
the fix. Unit regressions also cover Get replacement, intermediate Take
scratch, passthrough, and self-push snapshots. All pass with the fix, including
shared-suite WASI coverage and all seven inline Miri tests.

At 40,000 iterations, three alternating release process runs per shape/backend
on CPU 2 gave these medians (whole-process time, startup/parsing included):

| Constructor feeding Const → Push | Before stackless / stackful | Fixed stackless / stackful |
|---|---:|---:|
| `[acc]` | 2,586.5 / 2,591.6 ms | 4.64 / 4.74 ms |
| `{a: acc}` | 2,592.9 / 2,590.1 ms | 5.18 / 5.25 ms |

At 10,000 iterations the fixed runs took 2.14–2.39 ms, versus 123–126 ms
before. All final counts matched. Allocation-identity assertions guard the
cause without timing thresholds. This does not claim to fix the pre-existing
passthrough-retention behavior discussed in the review.

The final implementation passed the full 25-workload VM suite, four widths,
nine retained samples per engine/cell, with every value/count check passing.
[Raw results, reproductions and source fingerprints](../bench/vm-execution/results/2026-10-05-owned-segment/)
identify the working tree based on `a003cb7`. Selected 256-motif medians (ns):

| Workload | 1.x | 2.0 stackful | 2.0 stackless | Stackless / 1.x |
|---|---:|---:|---:|---:|
| add-int | 303.7 | 238.1 | 234.8 | 0.77× |
| add-float | 378.6 | 375.5 | 372.4 | 0.98× |
| add-float4 | 376.6 | 391.3 | 389.1 | 1.03× |
| const-int | 128.6 | 197.8 | 195.5 | 1.52× |
| get-int | 129.6 | 266.6 | 265.4 | 2.05× |
| make-seq | 1,443.4 | 4,577.3 | 4,576.9 | 3.17× |
| make-table | 1,583.8 | 4,857.1 | 4,849.7 | 3.06× |

19/25 stackless cases now meet ≤2× at width 256, versus 20/25 in the previous
run: Get(Int) crosses the threshold. Const/Get and Do have measurable overhead;
arithmetic remains near or faster than 1.x. These are separately measured
runs with unlocked CPU frequency; the 1.x timings also changed. This fix does
not claim a throughput improvement across every workload. Scheduler/HTTP
measurements in the runtime overview remain explicitly pinned to the earlier
runtime and were not repeated for this ownership change.

The full required local check set passed on the final implementation, including
both native backends, WASI execution, docs-off tests, release nesting limits,
TLS and WASI clippy. Release core tests with output checks, all seven inline
Miri tests, and benchmark validator tests passed. Independent verification of
this new finding remains a separate step.


## Compose-time scratch release selection

The next optimization moves the cleanup decision out of execution. An
exhaustive compose pass selects release/non-release Const/Get/Set/Inc opcodes,
tracking whether owned scratch may exist. Segment entry and generic/constructor
boundaries assume owned input; Take/Push/generic vector arithmetic may introduce
scratch again. After release, subsequent reanchoring opcodes perform no cleanup
check. Instruction/node correspondence stays one-to-one. The previous lifetime
fix remains in force; `clear_scratch` is removed.

The complete 25-workload, four-width, nine-sample suite passed again on the
working tree based on `4cbfb0e`. [Raw results and source hashes](../bench/vm-execution/results/2026-10-05-compose-cleanup/).
Selected width-256 medians, ns:

| Workload | 1.x | 2.0 stackful | 2.0 stackless | Stackless / 1.x |
|---|---:|---:|---:|---:|
| const-int | 128.5 | 188.3 | 185.5 | 1.44× |
| get-int | 130.5 | 261.4 | 259.3 | 1.99× |
| add-int | 300.6 | 229.2 | 225.0 | 0.75× |
| add-float | 378.5 | 372.2 | 372.2 | 0.98× |
| add-float4 | 375.0 | 391.7 | 385.6 | 1.03× |

21/25 stackless cases meet ≤2× in this run; threshold-adjacent cases should not
be treated as robust categorical wins given unlocked CPU frequency. Const(Int)
and Get(Int) previously measured 195.5 and 265.4 ns; they now measure 185.5 and
259.3 ns. The decision moved to compose, but the remaining dispatch/addressing
costs prevent universal parity. [Inspection of the actual release assembly](vm-assembly-comparison.md)
shows those costs and preserves raw disassembly, binary hashes and commands.

The full local check set passed, including WASI and release output checks.
After a final equivalent edit making the ownership-opcode match exhaustive,
workspace clippy, core unit/shared tests and all eight inline Miri tests passed
again. A new allocation-identity test covers a generic boundary introducing an
owner after the previous segment had cleared scratch; the shared constructor
regression now also enters its constructor after a non-owning segment. All
lifetime and snapshot regressions remain passing. No scheduler/HTTP rerun is
claimed for this change.

## Explicit opcode tags and specialized Get addressing

A four-way experiment on the runtime based on `4b52472` tested the existing
representation, explicit opcode tags alone, specialized Get offsets alone,
and both. [Method, matrix and assembly analysis](vm-assembly-comparison.md#implemented-follow-up-explicit-tags-and-specialized-get-offsets)
show gains from both changes, with 32-byte instruction stride preserved on the
measured x86-64 release build. Both are retained. Public Var, snapshot semantics,
integer overflow behavior and per-instance state representation are unchanged.

The complete final suite passed: 25 workloads, four widths, nine retained
samples per engine/cell, all outputs/counts verified. Selected width-256 medians
(ns per chain):

| Workload | 1.x | 2.0 stackful | 2.0 stackless | Stackless / 1.x |
|---|---:|---:|---:|---:|
| const-int | 125.3 | 129.1 | 127.8 | 1.02× |
| get-int | 128.7 | 149.0 | 145.9 | 1.13× |
| add-int | 297.3 | 178.6 | 179.3 | 0.60× |
| add-float | 373.1 | 365.9 | 362.1 | 0.97× |
| add-float4 | 372.4 | 411.2 | 379.4 | 1.02× |
| assign-int | 453.1 | 557.6 | 558.1 | 1.23× |
| take-seq | 1,578.9 | 626.1 | 628.3 | 0.40× |

21/25 stackless workloads meet ≤2× at width 256. The remaining cases are
sequence/table construction, shared-sequence Push, and table Take (3.08–3.59×).
This is a substantial improvement in cheap-operation throughput, not universal
parity or a whole-application claim. Short widths, both backends and dispersion
are retained in the data; the stackful Float4 median is 1.10× in this full run,
whereas the separate alternating matrix has it closer to parity. Frequency
remains unlocked; compare the paired data rather than treating threshold
crossings or every timing difference as a stable property.

[Raw full-suite results, matrix samples, variant patches and final assembly](../bench/vm-execution/results/2026-10-05-explicit-opcodes/).
Matrix patches are relative to `4b52472`; the full-suite metadata and source
hashes identify the final working tree. Tests and a comment were added after
building the matrix candidates, so their hashes and final binary are recorded
separately rather than presented as identical artifacts.

Validation: the full local check set passed, including native/WASI shared
suites, both clippy variants, docs-off tests and release nesting limits.
Release core tests with output checks and all nine inline Miri tests passed.
The new `specialized_get_checks_the_selected_frame` checks that an oversized
other frame cannot validate a bad offset, and that offset multiplication cannot
wrap. Existing tests cover both frame kinds, scratch ownership, snapshots and
self-push. No bounds checks were removed. Scheduler/HTTP timings were not
refreshed for this internal representation change.
