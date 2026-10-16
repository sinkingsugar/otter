# Compose cost

What loading and composing scripts costs, and a preserving reload. It started with compose-time evaluation (M8): a constant read in many places is one shared value, and compose had walked such values path by path, so a few lines of script took seconds and gigabytes. These checks keep that from coming back unnoticed, in three layers.

## Tests: `crates/otter-lang/tests/compose_cost.rs`

These run in `cargo test` (native only) and use no timing:

- **Allocation ratios.** Allocations are counted on the test's own thread, so parallel tests do not disturb the count. Each workload is compared with a twin that holds as many values, but with one copy of the level below per level, so it expands to what it holds:
  - composing 200 reads of a 5-level shared table constant;
  - a preserving reload of 100 reads of a 14-level map-table constant.
- **Watchdog walks.** A watchdog bounds the walks over values of 64 levels that each hold the level below twice (2^64 paths): conversion, `type_of`, `admits`, equality and the cache-key hash. A regression then fails after a minute instead of hanging CI.

Against the core before the fixes (30677d3), the allocation ratios fail at 530× and 370×, and the watchdog fails the walks.

## Benchmark: `crates/otter-lang/examples/bench_compose.rs`

```sh
cargo run --release -p otter-lang --example bench_compose
cargo run --release -p otter-lang --example bench_compose -- --check bench/compose/baseline.txt
cargo run --release -p otter-lang --example bench_compose -- --write bench/compose/baseline.txt
```

The workloads:
- `shared-table-reads`: `Program::load` and `compose()` of 1,000 reads of a 5-level table constant, against its flat twin.
- `map-table-reload`: `reload_preserving` of 200 reads of a 14-level map-table constant in a looped wire, against its flat twin.
- `literals`: 1,000 statements with distinct table and sequence literals.
- `functions`: 200 functions called in one pipeline.
- `flow-args`: one `All` with 16,384 flow arguments, against its twin of 1,024 `All`s with 16 each.
- `match-cases`: one `Match` with 16,384 cases, against its twin of 1,024 `Match`es with 16 each.
- `bound-flows`: 8,192 statements each running a nested flow and binding a name, against its twin binding none.

The last three keep compose linear in a shard's flow arguments and in a wire's names: before the fix, they took 8.0×, 6.2× and 28× their twins.

Each workload is warmed up once, then run 5 times. It reports the fastest time, and the allocation count and bytes of the leanest run.

`--check` fails when either of these holds:
- a workload allocates more than 25% above `baseline.txt` (counts and bytes, which do not depend on the machine's speed);
- a workload takes more than 4× its flat twin (a ratio, so it holds on any machine).

CI runs `--check` in the `bench` job. After an intended change in allocations, regenerate the baseline with `--write` and say why in the commit.

Release, macOS aarch64 (2026-10-09):

| Workload | ms | Allocations | Bytes |
|---|---:|---:|---:|
| shared-table-reads | 5.1 | 117,532 | 11,060,643 |
| shared-table-reads-flat | 3.5 | 114,217 | 10,243,733 |
| map-table-reload | 3.7 | 46,618 | 7,005,450 |
| map-table-reload-flat | 1.6 | 40,492 | 3,261,054 |
| literals | 4.9 | 218,242 | 21,955,436 |
| functions | 1.1 | 50,472 | 4,824,185 |

The scaling workloads, added 2026-10-16 (same machine):

| Workload | ms | Allocations | Bytes |
|---|---:|---:|---:|
| flow-args | 24.8 | 1,065,262 | 114,483,573 |
| flow-args-flat | 25.5 | 1,138,882 | 116,354,831 |
| match-cases | 21.1 | 966,987 | 98,541,785 |
| match-cases-flat | 23.8 | 1,123,522 | 107,360,015 |
| bound-flows | 27.9 | 1,245,470 | 130,560,929 |
| bound-flows-flat | 21.3 | 983,252 | 112,981,775 |

At 30677d3, before the fixes:
- `shared-table-reads` took 6.2 s, with 192 million allocations and 5.9 GB in total (1,812× its twin);
- `map-table-reload` took 2.1 s with 1.2 GB (1,317× its twin);
- `literals` and `functions` were the same within 2% of allocations.

## CI: the `bench` job

The job runs `bench_compose --check`, then prints `bench_tables`, `bench_instances` and `bench_depth` for the log. Those three are not gated: shared runners vary too much in time, so compare their numbers within one run, or rerun them locally before and after a change, as `bench/values/README.md` does.
