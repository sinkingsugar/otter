# M4 trampoline gate

Baseline engine: `c0aaa3961b1d2de4e9b7f9512b49c668c064d727` (M3). Candidate source and executable hashes are in `2026-10-06-gate-metadata.json`. Both release builds use the same `bench_depth.rs` from this directory's implementing checkout and the unchanged `bench_instances` workload. `run.py` alternates engines in rotating/reversed order, uses 1,000 instances, checks work counts and retains raw stdout beside its CSV.

Reproduce by extracting the baseline commit into a separate checkout, copying the candidate `crates/otter-core/examples/bench_depth.rs` over its benchmark, and building core examples in both checkouts with the pinned toolchain:

```sh
cargo build --release -p otter-core --examples
python3 bench/trampoline/run.py /path/to/baseline/target/release/examples target/release/examples /tmp/m4-gate.csv 5
```

The measurements separate:

- **pending:** a controllable async leaf remains pending through 1,000 timed ticks, after warmup. No completion or entry work is mixed into these samples.
- **completion:** make that leaf ready, then time full completion/unwind. Each next entry/first poll is outside the timed sample. Every sample validates exactly 1,000 × 1,000 completions.
- **progress:** the original pre-M4 depth benchmark, unchanged: `While(true {Inc(resumes) Pause})` at the bottom of nested Do calls. Each timed tick completes Pause and starts a new one, so it measures useful work at the leaf plus re-entry into While. It is retained separately because it is not a pending-future re-poll.
- **mixed:** the existing entity workload (`bench_instances`, 1,000 instances), including Keep, Once, vector arithmetic, mesh writes, Do and Pause. The runner verifies 500,000 iterations, rather than accepting timing output without checking its work count.

`2026-10-06-initial.csv` and `2026-10-06-screening.csv` are preliminary measurements. Their runner did not retain/validate the mixed-work count; they are not the final gate evidence. The gate CSV and matching log supersede them for gate decisions. No workload was shortened or changed in response to a missed threshold.

The structural gate is in `crates/otter-core/tests/trampoline.rs`: pending re-polls dispatch zero parents at depths 1/4/16/32, full completion dispatches each parent once, stale wakes cannot resume a replacement instance, and repeated suspend/complete cycles do not grow live allocations or retained owners. Shared acceptance tests cover control signals, cancellation, cleanup panics, partial initialization and preserving reload. Arena stale-handle and generation-exhaustion cases also run under Miri.

## Results (2026-10-06)

Five alternating process trials on Linux x86-64, Intel Core i9-14900KF, Rust 1.98.1, release. Values are medians in ns per instance per timed tick. No affinity pinning; these are local CPU measurements, not embedded or whole-application performance claims.

| Workload | M3 stackless | Stackful | Trampoline |
|---|---:|---:|---:|
| pending (depth 32) | 819.69 | 52.15 | 16.30 |
| completion (depth 32) | 970.79 | 1167.50 | 704.83 |
| progress (depth 32) | 961.67 | 128.39 | 103.84 |
| mixed | 110.10 | 164.10 | 119.60 |

Both soft gates pass: pending is 0.313× stackful (limit 1.5×); mixed is 1.086× M3 stackless (limit 1.10×). The latter has limited margin and remains a useful regression workload. Frame ownership is more explicit but costs additional instance memory; consult the retained mixed-workload stdout for state/heap/creation measurements, rather than interpreting resume speed as a universal improvement. The initial and screening CSVs are superseded by this validated run.

### Compact metadata follow-up

The unchanged five-trial gate was repeated at `0692517c31a6bb46616bc758ecbdcb80395cb105` after replacing copied occurrence paths with shared trees. `2026-10-06-compact-gate.csv`, its raw log and metadata retain the exact evidence. Medians (ns/instance/tick):

| Workload | M3 stackless | Stackful | Trampoline |
|---|---:|---:|---:|
| pending (depth 32) | 468.67 | 52.16 | 16.04 |
| completion (depth 32) | 696.74 | 978.71 | 679.58 |
| progress (depth 32) | 556.30 | 127.16 | 102.12 |
| mixed | 107.80 | 140.70 | 118.20 |

Both limits still pass: pending is 0.308× stackful, mixed is 1.0965× M3 stackless. Absolute baseline depth timings varied considerably between runs on the unpinned workstation; compare alternating samples within each run. The mixed-flow margin remains narrow.

### Exact frame reservation follow-up

`2026-10-06-reserved-gate.csv`, its raw log and metadata repeat the unchanged five-trial workloads after exact arena/state/child reservation. All verification processes completed before sampling. An earlier preliminary run overlapped local checks and is not used for this gate. Medians (ns/instance/tick):

| Workload | M3 stackless | Stackful | Trampoline |
|---|---:|---:|---:|
| pending (depth 32) | 854.32 | 54.17 | 16.41 |
| completion (depth 32) | 984.57 | 1314.27 | 708.17 |
| progress (depth 32) | 980.78 | 132.78 | 104.13 |
| mixed | 112.70 | 166.00 | 118.90 |

Both limits pass: pending is 0.303× stackful; mixed is 1.055× M3 stackless. This run supersedes the compact-metadata run for the current runtime implementation.

### Shared reload metadata follow-up

The unchanged five-trial run at `ab4bc6b744701b503e0524a2e1783b02dc14455c` is retained in `2026-10-06-shared-gate.csv`, its raw log and metadata. Verification processes finished before sampling. Medians (ns/instance/tick):

| Workload | M3 stackless | Stackful | Trampoline |
|---|---:|---:|---:|
| pending (depth 32) | 846.22 | 54.49 | 16.64 |
| completion (depth 32) | 990.90 | 774.19 | 682.82 |
| progress (depth 32) | 990.74 | 131.48 | 102.45 |
| mixed | 112.80 | 165.40 | 118.00 |

Both soft limits pass: pending is 0.305× stackful; mixed regresses 4.61%. This supersedes the exact-reservation run for the current snapshot. It does not clear the device acceptance gate.

### Persistent call-site prefix follow-up

The unchanged five-trial run at `c88c59ded12184fb9d0b56eb7add81bf00b104cf` is retained in `2026-10-06-prefix-gate.csv`, its raw log and metadata. All local checks and reviewer tests finished before sampling. Medians (ns/instance/tick):

| Workload | M3 stackless | Stackful | Trampoline |
|---|---:|---:|---:|
| pending (depth 32) | 438.92 | 51.37 | 14.66 |
| completion (depth 32) | 662.41 | 670.61 | 620.61 |
| progress (depth 32) | 531.97 | 121.96 | 97.58 |
| mixed | 105.50 | 133.50 | 105.00 |

Both limits pass: pending is 0.285× stackful; mixed is 0.995× M3 stackless (no regression in this run). Absolute timings vary on the unpinned workstation; compare engines within each alternating run. This supersedes the shared-metadata run for the current snapshot. Device clearance remains a separate requirement.
