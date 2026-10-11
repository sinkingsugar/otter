# Performance Gates

Status: plan, 2026-10-11. Approved in direction by Giovanni; nothing here is implemented yet. It comes before Track A1 (objects and services, [host-requirements.md §7](host-requirements.md#7-priorities-2026-10-11)), so that A1's new `Var` variant and every later core change is measured against a strict baseline. The work is meant to be done on a Linux machine: Valgrind does not run on Apple Silicon macOS.

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
- added with A1: object clone/drop through sequences, tables and mesh variables, and a service lookup at activation.

The driver should reuse the VM suite's script generator rather than copying it (factor it into a module both harnesses import).

**Baseline and check**, the same pattern as the compose gate:
- `bench/instructions/baseline.txt`: one row per workload, with instructions, allocations and bytes per iteration, plus the Valgrind version, toolchain and target it was taken with;
- `--check baseline.txt` fails when any workload's instructions per iteration rise above the tolerance, or its allocations or bytes per iteration rise at all; improvements are reported, and recorded by regenerating;
- `--write baseline.txt` regenerates it; the commit says why (as for hot-asm and the compose baseline).

**Target and pinning.** x86_64 Linux (`ubuntu-latest`). The toolchain is already pinned (`rust-toolchain.toml`); pin the Valgrind version in CI too, since a different Valgrind can shift counts. Build with the repository's release profile and `.cargo/config.toml` flags, as shipped. An aarch64 Linux job (GitHub's arm runners) can be added later if x86 and arm counts diverge in ways that matter.

**CI.** A new job, or a step in `bench`, running `--check`. Valgrind runs ~50× slower than native: pick N so the job stays within a few minutes, and keep the per-iteration differencing so a small N still gives exact numbers.

## 3. What the gate does not see, and the wall-clock A/B

Instruction counts are blind to code layout and to branch prediction on real cores. The ±40% swings measured on Apple M-series came from block placement with identical instructions ([runtime-performance-overview.md](runtime-performance-overview.md), lessons 6 and 7). Two things cover that:

- **hot-asm** stays as it is: exact shape of the VM loop and the engine step.
- **A local wall-clock A/B tool**, `scripts/perf-ab.py BASE_REV`: builds both revisions with the same flags, runs them interleaved in mirrored order on the VM suite (all cases, widths 8, 64, 256) and the core benchmarks, reports per-case medians, deltas and spread, and exits non-zero beyond a threshold (3% proposed). It is part of the check set for any change to values, the VM or the engine, and its result goes in the commit message. Not a CI gate: shared runners are too noisy for it.

## 4. Later: instruction counts on the device targets

QEMU's TCG plugins (`libinsn.so`) count executed instructions deterministically, and CI already boots the ESP32 firmware in Espressif's QEMU (`scripts/esp32-qemu.sh`). Per-iteration counts on Xtensa and RISC-V would catch regressions that only show on the device. First check whether Espressif's QEMU fork builds with plugin support; this stage follows the Valgrind gate.

## 5. Order of work

On the `host-objects` branch, with a draft pull request to `main` so CI runs on every push:

1. The instruction and allocation gate, with its CI job and a baseline taken at the branch point (`main`).
2. `scripts/perf-ab.py`.
3. A1 in the decided order ([host-requirements.md](host-requirements.md) H1, H3, H4/H6), each commit passing both gates or regenerating the baseline with a reason.

## 6. Open, for Giovanni

- **Tolerance on instructions per iteration:** 0.5% per workload after measuring the real jitter, or exact (zero tolerance; every hot-path change then regenerates the baseline with a reason). Proposal: measure jitter first; if it is zero, make it exact.
- **Local runs:** Docker on macOS for `--check`, or Linux machines and CI only.
