# VM execution benchmark

Measures the execution of precomposed, warmed shard chains on 1.x and both
2.0 schedulers. See [the report](../../docs/vm-execution-benchmarks.md) for
results, interpretation, and build details. Python 3 standard library only.

```sh
cargo build --release -p otter-cli
# Build your 1.x checkout in Release with SHARDS_INLINE_EVERYTHING=ON.
python3 bench/vm-execution/run.py ../shards/build/Release/shards \
  --source1 ../shards --cpu 2 --out target/vm-execution/run-1
```

Pick an allowed performance core on your machine; omit `--cpu` on platforms
without Linux affinity. Do not run builds or other benchmarks concurrently.
The harness creates a **new** output directory and refuses to overwrite one.
It does not build either runtime or change CPU power settings. Use default
release features, without 2.0's optional `output-checks` feature. The comparison
is of the shipped implementation/build choices, not the two languages under
identical compiler flags.

## What is timed

Each generated script runs unchanged on all three engines. One wire executes
an outer `Repeat` of batches. Every batch times an inner `Repeat` with
`Time.Now`; there are no suspensions, I/O, logging, parsing, compose, instance
creation or mesh ticks inside that loop. Two initial batches warm **the same
instantiated shards**, then three measured batches follow. Each cell is run
in three independent processes per engine, giving nine retained samples.
Job and engine order are shuffled with a recorded seed.

Each inner iteration starts from a seed, executes `width` copies of a source
motif, saves its result, and increments a completed-iteration counter. The
harness verifies full result equality and the exact iteration count after
**every** batch, including warmup and calibration. Failure or incomplete
output aborts the run. There is fixed Repeat/Get/Update/Inc/timer overhead;
width 0 is its control. No time is subtracted from individual samples.

The calibration runs 10,000 iterations twice on each engine. All engines in
a cell then use the **same** iteration count, targeting 20 ms for the fastest
engine, capped at an estimated 250 ms for the slowest. This cap can make the
fastest engine's batches shorter than 20 ms. Durations and counts are retained;
these are elapsed wall times, not CPU cycle counts. Turbo, frequency scaling
and other system activity can affect the result despite affinity.

## Workloads

- `const-*`: a repeated literal; measures actual constant-shard dispatch and
  output ownership. Scalar 7, a 256-byte string, 64 integer sequence elements,
  or a 16-field integer table.
- `get-*`: repeated reads of a local holding that value.
- `update-*`: a chain of `Update(slot)` calls starting from a seed. In 1.x,
  Update returns the destination cell, so calls after the first become
  self-assignment (`_cloneVarSlow` returns immediately). The optimized 2.0
  builtin executor also forwards the destination cell within its instruction
  segment. This measures that forwarding/alias fast path,
  **not** repeated independent copies.
- `assign-*`: every motif is `Get(seed) | Update(slot)`, so each assignment
  reads the original source again. Source and destination remain distinct.
  This measures 1.x deep copying with buffer reuse versus 2.0 sharing, plus
  one Get per assignment. It is not a memcpy microbenchmark.
- `add-int`, `add-float`: a dependent chain of additions of 1, starting at 1.
  No overflow, NaNs, infinities or rounding-sensitive inputs.
- `add-float4`: a dependent vector addition chain with a variable operand;
  vectors are converted before timing because the 2.0 frontend does not
  currently support the 1.x `@f4` builtin. Conversion is not measured.
- `take-seq`, `take-table`: each motif is `Get(seed) | Take(key)`; two shards,
  **not** a pure isolated Take. The key is a constant index or field name.
- `do-int`: each motif calls a wire containing one `Math.Add(1)`. This measures
  repeated synchronous calls, not increasing nesting depth or suspend/resume.
- `push-shared-seq`: each iteration resets a destination from a retained
  64-element seed, then appends `width` ones. Full contents are checked. The
  first 2.0 push must detach shared storage; later pushes can grow in place.
  The 1.x script writes `Push(items Clear: false)` to avoid its clearing
  semantics; the 2.0 dialect writes `Push(items)`, which never clears
  (`run.py` `dialect`).
  Reset, result retention, copy-on-write and capacity growth are part of this
  workload. It is not a unique-buffer-only push benchmark.
- `make-seq`, `make-table`: repeatedly construct four-element values from a
  variable. The source is identical but frontend lowering differs; these
  measure the runtime work emitted for that expression, not equal shard counts.

There is no matched table-field mutation benchmark: the needed 2.0 shard is
not implemented. This suite does not add substitute host shards that would
bypass the runtime path under investigation.

## Dialects and platforms

Scripts are generated in the 1.x dialect and translated for 2.0 (`dialect()` in `run.py`): `Var` declarations, lowercase labels, `Push` without `Clear`, and the `do-*` call case as a function (`@fn(Step ...)` called as `Step`; 2.0 has no `Do`). Provenance tolerates a platform without `lscpu` (macOS records `sysctl`'s CPU brand); `--cpu` pinning is Linux only, so runs elsewhere are unpinned and say so in their metadata.

## Output and analysis

- `metadata.json`: commands, binary hashes, source revisions/status, toolchain,
  machine, affinity and selected CMake settings. A source revision alone does
  not prove a supplied binary was built from it; rebuilding is the caller's
  responsibility.
- `scripts/`: exact scripts used, including calibrated counts.
- `calibration.json`: discarded calibration measurements.
- `samples.csv`: raw samples, including explicitly flagged warmups.
- `summary.csv`: median, min, max and median absolute deviation of ns/iteration.
- `slopes.csv`: ordinary least-squares fit of the medians against chain width,
  with intercept and R². Slope units are **ns per source motif**, not generally
  ns/shard. Inspect linearity; allocation thresholds can make a fit misleading.

For a quick semantic smoke check, including zero and long chains:

```sh
python3 bench/vm-execution/run.py ../shards/build/Release/shards \
  --iterations 3 --rounds 1 --samples 1 --warmup 1 \
  --widths 0 2 256 --out target/vm-execution/smoke
python3 -m unittest discover -s bench/vm-execution -v
```

Use `--cases`, `--widths`, `--rounds` and `--samples` for focused reruns.
Committed result directories retain raw samples, metadata and `scripts.sha256`,
not the large regenerable scripts. To reconstruct a script, import `run.script(case, width,
iterations, batches)` using the counts in `samples.csv` and metadata.
