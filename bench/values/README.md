# Value layout and table storage (golden path M6)

Two one-time measurements recorded here, as golden-path.md §7.2 and §7.3 ask: table storage before and after struct tables, and `Var` alignment 32 against 16. Both use `crates/otter-core/examples/bench_tables.rs`; the alignment comparison adds the instance and depth benchmarks. All numbers: release build, Rust 1.93.0 (the pinned 1.98.1 was being installed on a slow link; the benchmark binaries differ only by storage or alignment), aarch64-apple-darwin, no CPU pinning, one run each unless noted. The runs were short and on a laptop: compare columns within a table, not across machines.

```sh
cargo run --release -p otter-core --example bench_tables -- 200000
```

Each VM workload is a wire with a `Repeat` of 200,000 iterations over the body, timed as one mesh run after one warm-up run; allocations are counted by the example's allocator. `const-take-16` reads one field of a retained 16-key record by a literal key; `var-key-take-16` reads it by a key held in a variable; `make-take-N` builds an N-key record from locals (`Table.Make`) and reads one field; `rebuild-16` rebuilds while the previous record is retained by a variable; `chain-16` runs two same-shape constructors in one segment; `build-64` and `derive-64` use the host API (`TableBuilder`, `into_builder`).

## Storage: `BTreeMap` (before, `7ff5bca`) against struct and sorted-vector tables (after)

| workload | before ns | before allocs | after ns | after allocs |
|---|---:|---:|---:|---:|
| const-take-16 | 80.2 | 0.00 | 42.0 | 0.00 |
| var-key-take-16 | 103.7 | 0.00 | 105.5 | 0.00 |
| make-take-4 | 149.2 | 2.00 | 85.2 | 1.00 |
| make-take-16 | 613.6 | 4.01 | 122.6 | 1.01 |
| rebuild-16 | 632.2 | 4.01 | 116.2 | 1.01 |
| chain-16 | 626.2 | 4.01 | 122.5 | 1.01 |
| build-64 | 6407.2 | 203.00 | 4889.0 | 198.00 |
| derive-64 | 486.9 | 9.00 | 245.6 | 3.00 |

Per iteration; "before" is the same example on the `BTreeMap` storage with the 24-byte `Var`, "after" the struct tables with the 32-byte, 32-aligned `Var`. The literal-key read is an indexed load now; a record build is one allocation; a runtime key lookup (binary search on a shape or sorted entries) costs what the `BTreeMap` lookup did; the host builder's 64 inserts cost about the same as before (sorted-vector inserts in key order append) with fewer allocations, and deriving a changed table from shared storage halves. The second allocation `make-take` used to pay was an empty map built before the constructor ran; the constructor now starts from the consumed input or nothing.

## Alignment: 32 against 16 (64-bit)

Same binaries with the `repr(align)` attribute on `Var` changed; `bench_instances 1000` three runs each (entity workload, ns per instance tick), `bench_depth --progress` (ns per instance tick at depth 1/4/16/32).

| workload | align 32 | align 16 |
|---|---:|---:|
| const-take-16 ns | 42.0 | 57.6 |
| make-take-16 ns | 122.6 | 128.3 |
| derive-64 ns | 245.6 | 283.2 |
| instances, ns per instance tick | 485.0, 481.3, 494.5 | 481.5, 477.4, 471.5 |
| depth 1 / 4 / 16 / 32 | 175.5 / 155.9 / 157.9 / 159.9 | 168.6 / 149.8 / 154.6 / 154.8 |

Alignment 16 wins the instance and depth workloads by 2 to 3 percent, alignment 32 wins the table workloads by 5 to 27 percent; both are within this setup's run-to-run spread for the instance benchmark (about 3 percent) and no workload measurably loses at 32. Alignment stays 32 on 64-bit targets, as the plan prefers (a value never straddles a cache line); on 32-bit targets `Var` is 16-aligned by `Float4` and the same 32 bytes.

**Decision after review (2026-10-07):** the measurement above is in time only, and the review pointed out that the cost of alignment 32 is in size: a frame slot of the engine grew from 128 to 224 bytes, the VM's instruction and the engine's step from 32 to 64, and per-instance memory of the entity workload followed. `Var` is 32 bytes aligned to 16 on every target now (what `Float4` needs, and 1.x's `SHVar` layout); the time difference was within noise either way.
