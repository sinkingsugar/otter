# Runtime performance and optimization headroom

Measured 2026-10-05, runtime implementation `f3f46e0`, checkout `bbabad1`.
All native binaries are release builds. This refresh covers the existing CPU VM,
instance/tick, nested-resume, simulated async and local HTTP benchmarks. It does
not measure a full application, graphics, physics, or embedded performance.

**Subsequent ownership fix:** the [inline segment follow-up](vm-execution-benchmarks.md#inline-segment-input-lifetime-review-follow-up)
fixes an additional quadratic retention case and reruns the complete VM suite.
Arithmetic remains near parity; 19/25 long-chain cases meet ≤2× (Get(Int) now
2.05×). The tables below describe the explicitly identified earlier snapshot;
the scheduler/HTTP refresh has not been repeated for the later fix.

**Compose specialization and assembly:** the [later compose-time cleanup pass](vm-execution-benchmarks.md#compose-time-scratch-release-selection)
removes redundant scratch checks. Its full VM rerun and the
[release assembly comparison](vm-assembly-comparison.md) identify remaining
opcode decoding and frame binding costs. The historical tables below retain
their original snapshot.

**Golden path refresh:** the [2026-10-07 section](#golden-path-refresh-2026-10-07) below has the current engine (functions, recursion, 32-byte values, VM-level steps) against 1.x on the scheduler and VM workloads; the historical tables further down describe the 2026-10-05 snapshot.

**Latest internal layout:** [explicit opcode tags and specialized Get offsets](vm-execution-benchmarks.md#explicit-opcode-tags-and-specialized-get-addressing)
were adopted after an alternating four-way experiment and full VM rerun.
Const(Int) is now 1.02× and Get(Int) 1.13× 1.x at width 256. The
[assembly follow-up](vm-assembly-comparison.md#implemented-follow-up-explicit-tags-and-specialized-get-offsets)
records correctness evidence, memory/code costs and limits. Historical tables
below are unchanged; no later scheduler/HTTP rerun is implied.

## M4 direct-resume update (2026-10-06)

The new trampoline replaces the recursive stackless engine. [Five alternating trials and raw data](../bench/trampoline/README.md#persistent-call-site-prefix-follow-up) compare it with the M3 stackless baseline and stackful using identical workloads, 1,000 instances, Rust 1.98.1 release:

| Workload | M3 stackless | Stackful | Trampoline (current engine) |
|---|---:|---:|---:|
| Pending leaf re-poll, depth 32 | 438.92 ns | 51.37 ns | 14.66 ns |
| Full completion/unwind, depth 32 | 662.41 ns | 670.61 ns | 620.61 ns |
| Original depth benchmark (Pause completes each tick), depth 32 | 531.97 ns | 121.96 ns | 97.58 ns |
| Short mixed entity flow | 105.50 ns | 133.50 ns | 105.00 ns |

The M4 soft gate passes: pending re-polls cost 0.285× stackful, and short mixed flows cost 0.995× M3 (limit 1.10×). Structural tests prove zero ancestor dispatches during pending re-polls and one per parent during completion at depths 1/4/16/32. Measured state in the mixed workload is 1,306 bytes per instance versus 978 for M3 stackless (down from 1,930 before exact reservation). Instance state and creation cost still increase with the explicit arena; the raw log retains those measurements. Historical tables below retain their original snapshots, and their old recursive-stackless depth numbers no longer describe the current engine. The stackful scheduler was deleted after this gate (2026-10-07); its column here is its last measurement.

## Golden path refresh (2026-10-07)

Measured at `7bac245` on an Apple Silicon laptop (aarch64, macOS, Rust 1.98.1 release, unpinned, three rounds with 1.x and 2.0 alternating; see [the harness notes](../bench/runtime-overview/README.md)), against the same 1.x release binary as the day's earlier runs. The engine is the trampoline after the function, recursion and value milestones and two optimization commits: review fixes (`e706cfb`) and VM-level steps (`51525a6`: calls to straight-line bodies, loops and branches run inside a VM segment; a call site is prepared and entered in one step; leaf child flows run inside their composite's dispatch). The numbers in this section are medians from [`bench/runtime-overview/results/2026-10-07-7bac245`](../bench/runtime-overview/results/2026-10-07-7bac245) and [`bench/vm-execution/results/2026-10-07-7bac245`](../bench/vm-execution/results/2026-10-07-7bac245); the e706cfb column is the same harness earlier the same day.

Scheduler workloads, ns per instance per tick:

| Workload | 1.x | 2.0 at e706cfb | 2.0 at 7bac245 |
|---|---:|---:|---:|
| Entity, 100 instances | 113.7 | 108.4 | 101.4 |
| Entity, 1,000 instances | 141.9 | 103.9 | 100.5 |
| Entity, 10,000 instances | 406.1 | 103.4 | 101.9 |
| Nested resume, depth 1 | 108.0 | 83.1 | 82.8 |
| Nested resume, depth 4 | 109.7 | 83.1 | 83.9 |
| Nested resume, depth 16 | 104.3 | 87.7 | 85.7 |
| Nested resume, depth 32 | 108.9 | 89.3 | 84.4 |

The entity's think step is a function since M5 (`bench::functions`), so the entity rows are not the M4 gate's workload; against the gate's own engine on this machine, back to back, the entity ticks in 98 µs per 1,000 instances against the gate's 99 and e706cfb's 101, with 1.68 KiB of heap per instance against the gate's 1.66 (the price of the 32-byte value). Every run verified `instances × 500` iterations and 1,000,000 resumes.

VM suite (precomposed chains, nine samples per cell), 2.0 time over 1.x time at width 256, with the fixed per-iteration cost at width 0:

| Workload | 2.0 / 1.x at 7bac245 | at e706cfb |
|---|---:|---:|
| Fixed cost per iteration (width 0) | 10.3 ns vs 5.1 | 28.9 ns |
| Int / Float / Float4 Add | 0.36× / 0.85× / 0.38× | 0.38× / 0.93× / 0.41× |
| Constants (Int, String, Seq, Table) | 0.70× / 0.67× / 0.57× / 0.53× | 0.76× / 0.73× / 0.62× / 0.58× |
| Get (Int, String, Seq, Table) | 0.73× / 0.72× / 0.62× / 0.56× | 0.77× / 0.74× / 0.65× / 0.59× |
| Update (Int, String, Seq, Table) | 0.55× / 0.39× / 0.34× / 0.32× | 0.59× / 0.42× / 0.37× / 0.34× |
| Assign sequence / table / string | 0.09× / 0.06× / 0.46× | 0.09× / 0.06× / 0.46× |
| Take sequence / table | 0.62× / 0.83× | 0.56× / 0.86× |
| Construct sequence / table | 1.62× / 1.11× | 1.65× / 1.09× |
| Push to shared sequence | 1.60× | 1.87× |
| Function call per element (`do-int`, 1.x `Do`) | 2.47× | 5.70× |

The 1.x `Do` case runs as a 2.0 function (`@fn`) called per element; its 2.0 cost fell from 57 to 24.7 ns per call with VM-level calls, and the remaining gap is the per-call argument binding and locals clearing. Constructors and pushes to a shared sequence remain the cases above 1.x. The fixed cost per iteration halved three times over the day (the `Repeat` loop is a VM instruction now) and is twice 1.x's; the slopes at width 64 and above are at or below 1.x for every workload except constructors, shared pushes, `do-int` and Float4 add (1.31× at width 64).

### Flat control flow (`aae5c01`, the same day)

`Repeat`, `While`, `When` and `If` are lowered to flat code with jumps at compose, so an iteration or a branch is a jump inside one VM run and the composite has no frames of its own ([current-state.md](current-state.md)). Same machine and method; [`bench/runtime-overview/results/2026-10-07-aae5c01`](../bench/runtime-overview/results/2026-10-07-aae5c01) and [`bench/vm-execution/results/2026-10-07-aae5c01`](../bench/vm-execution/results/2026-10-07-aae5c01):

| Workload | 1.x | 2.0 at 7bac245 | 2.0 at aae5c01 |
|---|---:|---:|---:|
| Entity, 100 / 1,000 / 10,000 instances, ns per instance tick | 111 / 138 / 429 | 101 / 101 / 102 | 87 / 89 / 88 |
| Entity heap per instance | | 1.68 KiB | 1.30 KiB |
| Nested resume, depth 1 / 4 / 16 / 32 | 114 / 104 / 104 / 112 | 83 / 84 / 86 / 84 | 37 / 39 / 39 / 39 |
| VM fixed cost per iteration (width 0) | 5.2 ns | 10.3 ns | 4.3 ns |
| Function call per element (`do-int`), width 64 / 256 | 571 / 2,440 | 1,581 / 6,319 | 1,460 / 5,973 |

The VM suite's other cells moved within a few percent of 7bac245 (`take-table` 15 percent faster, `add-float4` 12 percent). The `do-int` row is from a supplementary three-round run of that case alone, [`2026-10-07-aae5c01-do-int`](../bench/vm-execution/results/2026-10-07-aae5c01-do-int): in the full run, both `do-int` processes at widths 64 and 256 ran in a slow mode this laptop shows intermittently under background load, where a whole process runs at about 1.7× its usual time for its whole life, presumably on efficiency cores. The full run's `do-int` cells (2,610 and 10,511) are recorded as measured and should not be read as the engine's cost; the nine processes of the supplementary run were all in the normal mode. Comparisons on this machine are only valid interleaved, and the 1.x column of the supplementary run matches the full run's within 2 percent.

### Inlined calls (`26e21be`, the same day)

A call to a small stateless straight-line body is inlined at compose ([current-state.md](current-state.md)). Same machine and method; [`bench/runtime-overview/results/2026-10-07-26e21be`](../bench/runtime-overview/results/2026-10-07-26e21be) and [`bench/vm-execution/results/2026-10-07-26e21be`](../bench/vm-execution/results/2026-10-07-26e21be):

| Workload | 1.x | 2.0 at aae5c01 | 2.0 at 26e21be |
|---|---:|---:|---:|
| Function call per element (`do-int`), width 8 / 64 / 256 | 74 / 584 / 2,581 | 183 / 2,610 / 10,511 (slow mode; 1,460 / 5,973 clean) | 11 / 62 / 206 |
| Entity, 1,000 instances, ns per instance tick | 145 | 89 | 90 |
| Nested resume, depth 32 | 109 | 39 | 38 |

A call to `Math.Add(1)` is one instruction in the caller's stream now, which is what 1.x's inline dispatch does per shard, resolved once at compose instead of at every activation. Every other VM cell is within 5 percent of aae5c01 except `add-float` at widths 64 and 256 (77 and 270 ns against 60 and 206; 1.25× 1.x at width 64). That path did not change: the runner's machine code for the float add is identical in both binaries, and the loop head moved to a 4-byte-misaligned address in this build. Building with `-C llvm-args=-align-all-nofallthru-blocks=4` restores it (21.4 against 20.9 ms per batch for aae5c01, 26.2 unaligned) with no measurable cost on the integer, get, const and call cases; it is not adopted here, since a global code-alignment flag is a build-profile decision, but it is the known lever if the runner's layout drifts again.

### Metaprogramming branch against 1.x (2026-10-09)

Same machine and method as the 2026-10-07 refreshes, against the same 1.x
binary (hash checked). At `692ee62` (M8 done) the scheduler was unchanged
against `26e21be` (entity 88 / 90 / 91 ns per instance tick at 100 / 1,000 /
10,000 instances, 1.x 111 / 135 / 411; nested resume 35 to 38 ns, 1.x 99 to
103), HTTP concurrency at 1,000 instances took 79 ms (1.x 2,606), and the VM
suite had three cases 6 to 20 percent slower: push to a shared sequence, take
from a table, Float4 add. Bisecting found M8's failure locations (`3ceac16`,
a closure around `inline::run`) for all three, and `9a1da16` (already on
`main`) for part of Float4. `a965e32` removed the closure; the VM suite at
`a965e32` is back to `26e21be` on push and take, with Float add 17 to 25
percent faster than at `26e21be`. Results:
[`runtime-overview/results/2026-10-09-692ee62`](../bench/runtime-overview/results/2026-10-09-692ee62),
[`vm-execution/results/2026-10-09-692ee62`](../bench/vm-execution/results/2026-10-09-692ee62),
[`runtime-overview/results/2026-10-09-a965e32`](../bench/runtime-overview/results/2026-10-09-a965e32),
[`vm-execution/results/2026-10-09-a965e32`](../bench/vm-execution/results/2026-10-09-a965e32).

The commit after `a965e32` removed the closures from the runtime's other hot
paths (constructors, engine, shards, table lookups). Interleaved against
`a965e32` on the whole VM suite (four alternating runs): the constructors 15
to 18 percent faster (one allocation, filled in place), `bench_tables` 6 to
21 percent faster on builds, rebuilds and literal reads, every other case
within 3 percent except the per-iteration fixed cost at width 0 (4.5 to 4.7
ns) and `assign-int` at width 8 (20 to 22 ns), which is function placement
(lesson 6): with `run` byte-identical and both binaries built with 64-byte
function alignment they measure the same.

## Lessons from the VM optimization work

Conclusions at `12ef2f0`, after the assembly comparison, controlled layout
experiment and full VM rerun:

1. **1.x's small activations were deliberate engineering.** The inspected
   Const and Get handlers reduce to producing an address or loading a resolved
   pointer. Early resolution, compact dispatch and careful value forwarding
   matter when useful work is only an assignment or addition. A language choice
   or an optimizing compiler alone does not guarantee that execution shape.
   These are principles to retain without copying 1.x's instance architecture.
2. **Representation matters, but the evidence distinguishes which part.**
   Explicit opcode tags removed niche decoding; compose-selected Local/Mesh Get
   offsets removed repeated binding selection and index multiplication. The
   four-way experiment supports both changes. Public `Var` stayed unchanged,
   measured instruction size stayed 32 bytes, and persistent instance state did
   not grow. This supports the current foundation, not a claim that every
   representation choice is optimal. Function code and stack use did grow
   slightly; the [assembly report](vm-assembly-comparison.md#implemented-follow-up-explicit-tags-and-specialized-get-offsets)
   records those costs.
3. **Move decisions to compose when their inputs are known there.** Cleanup
   variants now encode whether scratch may own a value. Runtime still enforces
   frame bounds and integer overflow semantics. Shared compiled instructions
   contain offsets rather than pointers into a particular instance. The
   compiled/state split therefore supports specialization and instance sharing
   together.
4. **Ownership lifetime is part of performance correctness.** Two optimizations
   preserved output values but retained extra collection references, turning
   linear accumulation into quadratic copying. Constructor state now retains no
   output, and segments consume and release obsolete input. Regression tests
   check owner counts and allocation identity as well as output snapshots.
   Reuse must follow ownership becoming available, not prolong ownership just
   to retain an allocation.
5. **Optimize the measured operation.** Float4 already emits packed SIMD.
   Dispatch improvements helped cheap activations; collection construction,
   shared mutation and table access need different work. The existing
   passthrough-retention case (`acc | ExpectSeq` followed by Push) remains an
   investigation item, distinct from the two fixed regressions. Short mixed
   flows also need attention: long-chain wins do not imply every short case
   improves.

6. **`inline::run` is layout-fragile (2026-10-09).** The interpreter is one
   large function (about 2,100 instructions on aarch64), and three things
   outside the instructions a benchmark runs decide what each instruction
   costs, measured on `metaprogramming`:
   - **Arms it never executes.** `Op::Clear` inline cost every loop 0.25 ns
     per iteration at `9a1da16`, and 20 to 45 percent across the VM suite on
     2026-10-09; out of line (`clear_slot`) it costs nothing.
   - **Helpers LLVM inlines into it.** `Table::get` without its closures got
     small enough to be inlined into the `Take` arm: `run` grew by about 250
     instructions and 24 stack reloads, and take, push and every fixed cost
     got 4 to 19 percent slower, take-from-sequence included. `get` is now
     `#[inline(never)]`. A closure wrapped around the loop (`3ceac16`) cost 7
     to 16 percent the same way.
   - **Where the linker puts it.** With `run` byte-identical to `a965e32`, a
     28-byte shift of its start still cost 0.2 ns per iteration (4 to 5 percent
     at width 0, 10 percent for `assign-int` at width 8); with every function
     aligned to 64 bytes (`-C llvm-args=-align-all-functions=6`) old and new
     land at the same address and measure the same, for 7.6 percent more code.
     That flag is not adopted (a build-profile decision, and hosts that build
     `otter-core` as a dependency would not get it); block alignment is
     (lesson 7).

   Neither the loop-alignment flag nor advancing the index before dispatch
   changed the first two. The current code is the best measured, found by
   bisecting variants and comparing the disassembly, not by design. `run`
   keeps two small closures: `Inc`'s overflow error (`ok_or_else`, called out
   of line from the error path only) and the float operand (inlined); removing
   them together with the others changed its layout. **Next
   time `run` changes, make the hot path predictable first** (candidates: keep
   only the cheap arms in the loop and move every other arm out of line and
   cold behind one call; split the dispatch so each hot arm is small and
   independent; place it deterministically; or profile-guided builds), compare
   its disassembly before and after (`scripts/hot-asm.py --compare REV`; the
   summary for this code is `bench/hot-asm/aarch64-apple-darwin.txt`, checked
   with `--check`), and A/B every
   edit to it on the whole VM suite, interleaved: the scheduler benchmarks do
   not exercise these paths.

7. **Values own their storage by hand, and branch targets are aligned
   (2026-10-10).** Adding `Var::Bytes` made the VM suite 10 to 88 percent
   slower without touching the VM: `Var`'s derived drop had an LLVM inline
   cost of 240 against a threshold of 250, and the new variant (360) made
   every drop of a number a call (`-C remark=inline` shows the decisions).
   `Var` now has a hand-written `Drop` and `Clone`, 1.x's
   `destroyVar`/`destroyVarSlow`: one inline tag check, storage released or
   counted out of line, so their cost no longer depends on how many variants
   hold storage ([values-and-types.md](values-and-types.md) §2). What it
   took to match or beat the derived glue, measured on the VM suite and the
   scheduler benchmarks:
   - **Count references in place where the copy is the operation.** `Set`
     (`var::assign_copy`) copies and releases inline; a call per copy cost a
     heap assign about 2 ns. Inlining the counts into every copy instead
     made `make-table` 21 to 32 percent and `bench_depth` 8 to 14 percent
     slower.
   - **Copy whole values and count through the storage pointer**
     (`Arc::increment_strong_count`/`decrement_strong_count`), not variant
     by variant: rebuilding a value field by field, or matching on a `&mut`
     to drop it, spilled it to the stack.
   - **Copy shared sequences in bulk** (`var::seq_mut`): one bitwise copy,
     then the references counted, into room to grow. `push-shared-seq` got
     14 to 38 percent faster than with `Arc::make_mut`.
   - **The engine keeps plain assignment**: wrapping its stores in the same
     helpers cost `bench_depth` 5 to 10 percent.

   **Placement decides as much as code.** On Apple M-series the same `run`
   measured 14 or 20 ms on one case from where its blocks landed, and an
   unrelated edit moved results 40 percent either way. Native builds now
   align every branch target that is not a fallthrough to 16 bytes
   (`.cargo/config.toml`, `-align-all-nofallthru-blocks=4`, about 5 percent
   more code; wasm and the ESP32 targets do not match it): comparisons stop
   depending on luck, and shipped speed with them. Hosts that build
   `otter-core` from another workspace need the same flag to get the
   measured speed. `hot-asm.py` ignores the padding. Compare builds only
   with the same flags on both sides.
   What a host gets without the flag, for the code at lesson 8 (same
   source, ABBA, 2026-10-10): the VM suite's geometric mean against 1.x is
   0.47 either way (65 of 75 cases faster than 1.x aligned, 63
   unaligned); unaligned loses `take-seq` 8 to 9 percent and `assign-table`
   at width 8 14 percent, everything else within 5 percent; the entity tick
   and `bench_depth` are within 1 percent. A different build can land worse
   unaligned: that is the variance the flag removes.

   Left after this work (`34fddd7` against M9, both aligned): no VM suite
   case more than 5 percent slower, most heap and constant cases 10 to 40
   percent faster; the entity tick about 4 percent slower with
   `stackless/engine.rs` unchanged, from register allocation in
   `Engine::activate` (fewer instructions, 396 stack loads against 336).

8. **The runtime's step carries only runtime code (2026-10-10).** The
   step was compiled twice (`steps::<LOCATE>`), and both copies carried the
   compose-time meter's paths behind a branch: the metered `run`
   variants, fuel and depth charges, the failure trace. Their registers
   cost every runtime step, and each milestone added some (stack loads in
   `Engine::activate`: 313 at M8, 336 at M9, 396 at `34fddd7`). The step is
   now generic on metering as the VM is (`steps::<METERED, LOCATE>`, the
   meter read only through a local that is `None` in the runtime's
   instantiation), each instantiation is its own function, and the arms
   that run once per call site (`prepare_and_enter`) or only after a
   stateless call (`end_invocations`) are out of line. The runtime step is
   1,358 instructions with 124 stack loads. Against M9 (aligned, ABBA): the
   entity tick at parity (+0.6 percent; `34fddd7` was +4), `bench_depth` 2 to
   5 percent faster, the VM suite unchanged against `34fddd7`. CI's
   `hot-asm` job now fails on any change to these summaries, so growth is a
   decision recorded in a commit, not a side effect.

Keep the VM foundation and pursue collection ownership/access and short mixed
flows next. The latest results and remaining misses belong in the
[VM report](vm-execution-benchmarks.md#explicit-opcode-tags-and-specialized-get-addressing),
not a universal runtime speed ratio. Deeper stackless resume remains a separate
control-flow project. Future fusion must preserve lifecycle, source/error,
mutation and reload boundaries; the current one-instruction-per-node mapping
is not yet a general optimizing IR.

The [final independent review](../.agent-handoffs/reviews/2026-10-05-12ef2f0-claude-bc7c33.md)
found no issues within scratch lifetime, Get offset safety and benchmark-evidence
scope. It ran targeted native debug/release tests and Miri, checked source hashes
and recomputed reported tables from retained data. It did not rerun benchmarks,
the full check set, WASI or CI, or independently inspect the 1.x assembly side.
Earlier constructor/input-lifetime fixes have separate verification records.
This is bounded evidence for the design, not a whole-runtime safety or optimality
certificate.

## Results

The [complete VM suite](vm-execution-benchmarks.md#constructor-retention-review-follow-up)
was already rerun against this implementation: 25 workloads, widths 0/8/64/256,
both 2.0 schedulers and 1.x, nine retained samples per cell, verified outputs.
At width 256, stackless meets the ≤2× time target in 20 of 25 workloads:

| Workload | 2.0 stackless time / 1.x time (lower is better) |
|---|---:|
| Int Add | 0.75× |
| Float / Float4 Add | 0.99× / 1.02× |
| Constants | 0.82–1.37× |
| Get | 1.21–1.89× |
| Update | 0.52–0.90× |
| Assign sequence / table | 0.13× / 0.11× |
| Assign string | 2.12× |
| Construct sequence / table | 3.24× / 3.07× |
| Push to shared sequence | 3.42× |
| Take sequence / table | 0.47× / 3.72× |
| Do integer subflow | 1.31× |

These ratios describe long hot chains, not short flows or entire applications.
The report also retains zero-width costs and fitted slopes. Sharing semantics
help collection assignment; this is not a faster deep copy. Constructor reuse
applies to consumed input/consecutive constructors, not arbitrary invocations.

Fresh scheduler medians, ns per instance per tick (three process runs):

| Entity instances | 1.x | 2.0 stackful | 2.0 stackless |
|---:|---:|---:|---:|
| 100 | 70.3 | 110.0 | 92.6 |
| 1,000 | 185.8 | 133.6 | 90.5 |
| 10,000 | 637.5 | 294.6 | 95.4 |

The entity performs math, variable updates, a nested call and suspension.
Every run checked `instances × 500` completed iterations. At 10,000 instances,
2.0's spawn-through-first-activation cost was 0.79 µs/instance stackless and
3.11 µs stackful. Current RSS growth was 1.50 / 5.34 KiB per instance, respectively;
counted live heap was 1.47 / 1.21 KiB. These are fresh 2.0 creation/memory results;
1.x creation/memory was not rerun in this refresh.

Pure nested-resume medians, ns per instance per tick:

| Nested depth | 1.x | 2.0 stackful | 2.0 stackless |
|---:|---:|---:|---:|
| 1 | 131.1 | 121.8 | 103.7 |
| 4 | 138.9 | 120.7 | 141.6 |
| 16 | 164.5 | 123.2 | 317.7 |
| 32 | 166.2 | 134.9 | 561.8 |

All runs checked 1,000,000 resumes. Both harnesses now use depth 32 instead of
64, within 2.0's compose nesting limit. Historical depth-64 results are not
current executable coverage. Stackless explicitly re-enters suspended parents;
its depth-dependent cost remains visible.

Simulated async, 1,000 instances with 50-step latency: stackless notification
mode took 4.5 µs/tick versus 14.6 µs polling every tick; stackful took 5.8 versus
39.7 µs. All four modes completed 19,000 requests; notification reduced future
polls from 1,000,000 to 38,000. This compares 2.0 wake modes, not 1.x.

HTTP execution medians against the same local server with 50 ms/request delay:

| Requests | 1.x | 2.0 stackful | 2.0 stackless |
|---:|---:|---:|---:|
| 1 | 51.9 ms | 51.7 ms | 51.6 ms |
| 32 | 205.0 ms | 52.3 ms | 52.5 ms |
| 100 | 509.0 ms | 62.0 ms | 62.7 ms |
| 1,000 | 2,488.5 ms | 107.7 ms | 120.4 ms |

All 36 runs checked client successes and server completions against N. At 1,000,
the server saw 32 concurrent requests for 1.x versus 1,000 for both 2.0 backends;
client peak thread counts were 38 versus 5. This measures the different I/O
scheduling strategies and 1.x's worker-pool ceiling, not raw VM speed. Creation
is timed separately in the raw data; HTTP runs were not CPU-pinned.

CPU measurements used CPU 2 on the same i9-14900KF, Rust 1.98.1 and existing
1.x release binary as the VM report. Frequency was not locked. Processes ran
sequentially, with no concurrent builds/benchmarks. CPU rounds alternate the
2.0 backend order; 1.x precedes them each round. HTTP uses the existing grouped
runtime order. Three samples describe this machine, not statistical guarantees.
[Raw data and reproduction commands](../bench/runtime-overview/README.md).

## Are the architecture choices sound?

**Keep the compiled/state split, explicit bindings, shared leaf/async APIs and
both backends. Keep the executor representation open to change.** This is an
engineering assessment supported by these measurements, not proof that every
future optimization will be cheap.

- **Compose is the right specialization boundary.** Types, operands and frame
  bindings are resolved before activation; shared immutable compiled artifacts
  can hold optimized code once for every instance. The new typed builtin stream
  already reaches arithmetic parity without changing the public `Var` layout
  or requiring every shard author to write two implementations.
- **The instruction stream is a useful starting point, not a finished optimizing
  IR.** `CompiledFlow` currently keeps nodes and instructions in one-to-one
  correspondence; activation/lifecycle state is still indexed by node. Fusing
  instructions or flattening calls requires an explicit mapping back to logical
  nodes for cleanup, errors, source locations and reload boundaries. A lowering
  pass is feasible within this design, but is not implemented today. (Done in
  the golden path, 2026-10-07: `CompiledFlow::pc_nodes` maps every instruction
  to the node it stands for, and `Repeat`, `While`, `When` and `If` are lowered
  to flat code with jumps and hidden slots at compose, their children's nodes
  merged into the parent frame. Calls, `Match`, `Once`, `Sub`, `Maybe` and
  `Conditions` still keep frames; see the [2026-10-07
  section](#golden-path-refresh-2026-10-07).)
- **Owned values at generic boundaries have a cost.** Builtins can borrow within
  a segment; generic shards return owned `Var`s, and suspension preserves owned
  input snapshots. More ownership-aware builtins and liveness-based elimination
  of temporary values fit the design. Borrowed outputs from arbitrary host
  shards would require an additional API/lifetime contract. Do not let borrowed
  frame pointers escape across callbacks, mutation or suspension. The constructor
  regression demonstrates why hidden retained aliases can destroy performance.
- **Table storage is a concrete data-layout target.** A known fixed shape could
  share its keys and address values by compose-resolved slots instead of a
  BTreeMap lookup. Sorted iteration, dynamic tables, snapshots and mutation must
  keep their defined semantics. (Done in golden path M6: struct tables with an
  interned shape, slot-indexed `Take`, in-place same-shape rebuilds; the
  `BTreeMap` is gone. [`bench/values`](../bench/values/README.md) records a
  16-key build-and-read going from 614 ns and 4 allocations to 123 ns and 1.)
- **Stackless continuation traversal is the largest measured control-flow
  limitation.** An explicit continuation stack/program counter could avoid
  re-entering every suspended ancestor. That requires reworking control shards,
  unwinding, cancellation and reload handling; it is more than an inline tweak.
  The common leaf/async contracts and compiled/state split can survive that work.
  (Done in golden path M4: the trampoline above; stackful was then deleted.)
- **SIMD is available, but not the missing general solution.** The measured
  Float4 builtin already emits packed arithmetic. Table lookup, reference
  ownership and ancestor traversal need different optimizations. General loop
  vectorization would additionally require dependency/alias/effect information
  that the current instruction stream does not express.

Next experiments should measure fixed-shape table access and short mixed
builtin/host flows first, then prototype direct stackless resume with the same
cleanup/cancellation/reload tests. Fusion needs explicit read/write and error
ordering rules before transformations become general. Generic per-node state
boxing is another footprint opportunity. These are proposed follow-ups, not
features delivered by this benchmark refresh; no universal parity claim follows
from the current arithmetic results.
