# Runtime overview refresh

See [the report](../../docs/runtime-performance-overview.md) for interpretation.
The 2026-10-05 results cover runtime `f3f46e0`, measured from `bbabad1` with only
benchmark-harness edits. The full VM suite is stored separately under
`../vm-execution/results/2026-10-05-owned-constructors`.

Run sequentially, without other benchmarks/builds:

```sh
cargo build --release -p otter-core -p otter-io --examples
python3 bench/runtime-overview/run.py ../shards/build/Release/shards /tmp/scheduler.json 2
bash bench/http-concurrency/run.sh ../shards/build/Release/shards 3 > /tmp/http.txt
```

On macOS there is no `taskset`; a shim on `PATH` that drops the pinning arguments runs the same sequence unpinned (the 2026-10-07 results were taken that way, on a laptop: `results/2026-10-07-e706cfb` after the review fixes and `results/2026-10-07-7bac245` after the VM-level steps, both summarized in [the report's 2026-10-07 section](../../docs/runtime-performance-overview.md#golden-path-refresh-2026-10-07)).

`run.py` checks entity iteration counts, depth resume counts and equal nonzero
async completions. The recorded HTTP refresh was separately validated: all 36
rows have `ok == server_completed == instances`, and `failed == 0` when reported.
HTTP requires loopback socket access and is unpinned; CPU measurements are pinned
with `taskset`. JSON records preserve commands, output and binary hashes. HTTP
metadata records its command, settings and hashes separately.
