# Performance Gates

Status: 2026-10-11. The instruction and allocation gate (§2) and the wall-clock A/B (§3) are implemented on the `host-objects` branch ([bench/instructions](../bench/instructions/README.md), §2.1, §3.1). Both run in CI; nothing needs to run locally (development is mostly on macOS). It comes before Track A1 (objects and services, [host-requirements.md §7](host-requirements.md#7-priorities-2026-10-11)), so that A1's new `Var` variant and every later core change is measured against a strict baseline. The work is meant to be done on a Linux machine: Valgrind does not run on Apple Silicon macOS.

## 1. Why

Runtime speed is not gated today. What CI checks:

- **hot-asm** (macOS job): fails on any change to the disassembly of `inline::run` and the engine step. Exact, but it only sees the shape of those two functions.
- **Compose costs** (`bench_compose --check`, [bench/compose/README.md](../bench/compose/README.md)): allocation counts against `bench/compose/baseline.txt`, plus shared/flat time ratios. Compose only.
- `bench_tables`, `bench_instances` and `bench_depth` run in the `bench` job but assert only correctness (iteration and resume counts), not time.

The matched benchmarks (the VM suite in `bench/vm-execution`, `bench/runtime-overview`, `bench/http-concurrency`) run by hand, compare against a 1.x binary, and are wall-clock. A 2.0-against-2.0 A/B before and after a change has been done by hand each time.

That gap has already cost us: adding one `Var` variant pushed derived drop glue past LLVM's inline threshold and slowed the whole VM suite by 10–88% (fixed by `Var`'s hand-written `Drop`/`Clone`, [values-and-types.md](values-and-types.md)); wrapping `inline::run`'s loop in a closure cost 7–16% on several cases. Neither would have failed CI. A1 adds `Var::Object`, the same kind of change.

## 2. The gate: instructions and allocations per iteration

Wall-clock time on shared CI runners is noisier than the regressions that matter, so the strict gate counts work instead of timing it.

**Instruction counts under Valgrind** (Cachegrind; Callgrind when a report needs per-function attribution). The same binary on the same input executes nearly the same instructions every run, so the gate can be strict. Cachegrind's cache and branch simulators (`--cache-sim=yes --branch-sim=yes`) report L1/LL misses and mispredicts as secondary columns: recorded, reported in diffs, not gated at first (they are models of a generic machine, not of any real core).

**Per-iteration counts, by differencing.** Each workload runs at N and at 2N iterations; the gate uses `(count(2N) - count(N)) / N`. Process start, parsing, compose, instantiation and teardown cancel, leaving the steady-state cost of one iteration. This also removes most nondeterminism: the randomly seeded `std::collections::HashMap`s (`compose.rs`, `types.rs`, `var.rs`, `reload.rs`, `otter-lang`'s `program.rs` and `lower.rs`) are used mostly at compose time. If a per-iteration count still jitters, find the source and seed or remove it rather than widening the tolerance.

**Allocations per iteration, counted exactly.** Valgrind replaces `malloc`, so allocator cost is invisible in its instruction counts. A counting global allocator in the gate's driver (as `bench_compose` already has) records allocations and bytes per iteration, natively, with the same differencing. Gated exactly: a hot loop that starts allocating is a regression however cheap the allocator is.

**Workloads**, each at a fixed iteration count with no calibration:
- the VM suite: all 25 cases of `bench/vm-execution/run.py` at widths 8, 64 and 256, run through `otter` directly (the generated scripts already verify their results and iteration counts);
- `bench_instances` steady ticks, `bench_depth` (suspend/resume through deep nesting), `bench_tables`;
- the async path (`bench_async`) if its per-iteration count is stable under Valgrind's serialized threads; otherwise left to the wall-clock A/B;
- a preserving reload cycle;
- added with A1 (done): object clone/drop through locals, sequences and tables (`object/*`; mesh variables take the same `Var` copy and drop), and a service lookup at activation (`service/lookup`).

The driver should reuse the VM suite's script generator rather than copying it (factor it into a module both harnesses import).

**Baseline and check**, the same pattern as the compose gate:
- `bench/instructions/baseline.txt`: one row per workload, with instructions, allocations and bytes per iteration, plus the Valgrind version, toolchain and target it was taken with;
- `--check baseline.txt` fails when any workload's instructions per iteration rise above the tolerance, or its allocations or bytes per iteration rise at all; improvements are reported, and recorded by regenerating;
- `--write baseline.txt` regenerates it; the commit says why (as for hot-asm and the compose baseline).

**Target and pinning.** x86_64 Linux (`ubuntu-latest`). The toolchain is already pinned (`rust-toolchain.toml`); pin the Valgrind version in CI too, since a different Valgrind can shift counts. Build with the repository's release profile and `.cargo/config.toml` flags, as shipped. An aarch64 Linux job (GitHub's arm runners) can be added later if x86 and arm counts diverge in ways that matter.

**CI.** A new job, or a step in `bench`, running `--check`. Valgrind runs ~50× slower than native: pick N so the job stays within a few minutes, and keep the per-iteration differencing so a small N still gives exact numbers.

### 2.1 As built (2026-10-11)

- **Driver and harness.** `crates/otter-lang/examples/bench_gate.rs` runs one workload with a counting allocator; `bench/instructions/run.py` runs it at N and 2N under Cachegrind and writes or checks `bench/instructions/baseline.txt`. One Valgrind run gives instructions and allocations together (the counting allocator still sees every allocation under Valgrind's `malloc`); `--allocations-only` runs natively, on any OS. The VM suite's generator moved to `bench/vm-execution/workloads.py`, with an untimed variant (no clock reads, so the output does not vary), and `bench_depth`'s and `bench_tables`' workloads moved into `otter_core::bench`, so the examples and the gate share them. 85 workloads, about 5 seconds on 24 cores.
- **Exact, tolerance 0** (§6's first question). Measured with `--jitter`: five full runs identical in every column, after two fixes. The runtime's own hash maps were randomly seeded, so compose probed differently in each run and the fixed cost differed between the N and 2N runs (the plan's suspected source, found with Callgrind in the type interner and the name-to-slot maps); they now use SipHash with fixed keys (`otter_core::hash`), the same function at the same cost; maps a host passes in (`defines`) stay standard. And Cachegrind probes the cache geometry from the CPU it starts on, which on a hybrid CPU (P- and E-cores) changes with the core; `run.py` passes a fixed geometry. With both, the cache and branch columns are exact too; they stay ungated.
- **Sensitivity.** A single `black_box(index)` in `inline::run`'s loop (about one instruction per VM operation) fails all 85 workloads, the entity tick at +0.87%, well below what wall-clock timing resolves.
- **Environment.** Valgrind replaces `memcpy` and `malloc` with its own, and libc code is counted, so counts depend on the Valgrind and C library build. The baseline records rustc, target, C library (`ldd --version`, which names the distribution's package revision) and Valgrind, and `--check` refuses a mismatch. CI pins the images (`ubuntu-24.04`, and `ubuntu-24.04-arm` for a second baseline: the code differs per architecture, and aarch64 is what development machines run) rather than the Valgrind version alone. Each baseline is named by its target (`bench/instructions/TARGET.txt`, the default for `--check`), and each job uploads the baseline it measured as an artifact on every run: regenerating after an intended change is `gh run download`, not reproducing CI's environment. On a Linux machine, a change can also be checked against a baseline written at its base.
- **Local runs** (§6's second question): none needed; CI runs both architectures. No Docker path. On macOS, `--allocations-only` checks allocations natively.
- **Not yet:** the async path (`bench_async`) and A1's object workloads.
- **Considered: counting wasm instructions instead.** Deterministic and portable (no Valgrind, any OS), but it measures another program: LLVM's middle end (inlining, as in the `Var` drop-glue regression) is shared, its back end is not, so register allocation, spills and instruction selection, which hot-asm and Cachegrind see, would not show in a stack machine's count; the allocator and libc differ too; and it needs a wasmtime host to read fuel. A possible portable cross-check later, with §4.

## 3. What the gate does not see, and the wall-clock A/B

Instruction counts are blind to code layout and to branch prediction on real cores. The ±40% swings measured on Apple M-series came from block placement with identical instructions ([runtime-performance-overview.md](runtime-performance-overview.md), lessons 6 and 7). Two things cover that:

- **hot-asm** stays as it is: exact shape of the VM loop and the engine step.
- **A local wall-clock A/B tool**, `scripts/perf-ab.py BASE_REV`: builds both revisions with the same flags, runs them interleaved in mirrored order on the VM suite (all cases, widths 8, 64, 256) and the core benchmarks, reports per-case medians, deltas and spread, and exits non-zero beyond a threshold (3% proposed). It is part of the check set for any change to values, the VM or the engine, and its result goes in the commit message. Not a CI gate: shared runners are too noisy for it.

### 3.1 As built (2026-10-11): the A/B runs in CI

Giovanni's call: no local monitoring, delegate to CI. `scripts/perf-ab.py BASE [--head REV]` builds both sides in release with the repository's flags (into `target/perf-ab/`, which CI's cache keeps), calibrates the VM cases' iterations once, then times both sides round by round in mirrored order (ABBA): the VM suite (25 cases at widths 8, 64 and 256, through `otter run`), `bench_instances`' entity tick, `bench_depth --progress` and `bench_tables`. It reports each case's median per side, the change, and the spread of the per-round ratios, as a markdown table, to the job summary in CI. The `perf-ab` workflow (macOS, Apple Silicon) runs it on every pull request against the base, and by hand (`workflow_dispatch`) on any two revisions.

It reports and does not fail yet: shared runners are noisy, and the threshold (`--fail-above`, 3% proposed) is set only after A/A runs (the same revision on both sides) have measured this runner's noise floor, as the jitter measurement set the instruction tolerance. A first local A/A on a hybrid desktop CPU showed spreads up to 33% on single cases (a process on an efficiency core against one on a performance core), which is what the calibration must rule out on the runner.

First measurement on the runner (run 38142369130, macOS arm64, 5 rounds): this pull request against `main`, effectively an A/A since the hot code is identical (hot-asm). Per case the noise is large: median |change| 2.8%, 42 of 88 cases beyond 3%, 5 beyond 10%, the largest 13.5%, median per-round spread 18.5%. The geometric mean over all cases moved +0.04%. So a 3% per-case threshold would fail nearly every run; the likely gate is on the geometric mean (and per case only for large moves), to be decided after A/A runs on `main` (`workflow_dispatch` needs the workflow on the default branch).

A/A on `main` (run 38143043858, `0121099` against itself): median |change| 8.9%, 40 of 88 cases beyond 10%, the largest 43.6%; geometric mean −0.70%. The runner's per-case noise varies between runs (2.8% median in the first measurement, 8.9% here), so no per-case threshold is usable; the geometric mean stayed within about 1 to 2% in both. The first A1 change measured (`Var::Object`, `inline::run` 76 instructions larger) came out at −1.41% geometric mean, heap cases −4.6%: within the noise, so no evidence of a cost, nor proof of none. Proposal: if the A/B is to fail at all, on the geometric mean beyond 3%, with more rounds to tighten it.

## 4. Later: instruction counts on the device targets

QEMU's TCG plugins (`libinsn.so`) count executed instructions deterministically, and CI already boots the ESP32 firmware in Espressif's QEMU (`scripts/esp32-qemu.sh`). Per-iteration counts on Xtensa and RISC-V would catch regressions that only show on the device. First check whether Espressif's QEMU fork builds with plugin support; this stage follows the Valgrind gate.

## 5. Order of work

On the `host-objects` branch, with a draft pull request to `main` so CI runs on every push:

1. The instruction and allocation gate, with its CI job and a baseline taken at the branch point (`main`).
2. `scripts/perf-ab.py`.
3. A1 in the decided order ([host-requirements.md](host-requirements.md) H1, H3, H4/H6), each commit passing both gates or regenerating the baseline with a reason.

## 6. Decided

- **Tolerance on instructions per iteration:** exact. Jitter measured zero once the hashers were seeded (§2.1); every hot-path change regenerates the baseline with a reason.
- **Local runs:** Linux machines and CI; no Docker path for now (Giovanni, 2026-10-11).
