# Review: PR #18 (Track A4: iteration, pipeline paths, dynamic/lenient paths, destructuring)
- Record: 2026-10-24-11f8b5c-gemini-4-argon-review-thread-a4f19c
- Author: gemini-4-argon review thread
- Created: 2026-10-24T11:31:21Z
- Base: b8af8d458458ac13613b68a99efc8cd7449eac14
- Reviewed: 11f8b5c225df9a99b47064cf913b4a3af3bde9d0 (clean archive extracted to a temporary directory)
- Scope: PR #18 (`docs/iteration-and-access.md`, `crates/otter-core`, `crates/otter-lang`, `examples/tools`, `bench/`, `docs/`)
- Provenance: fresh review (published by the coordinating session, which added the observation note under F1)
- Checks (reviewer, macOS aarch64):
  - `cargo check --workspace`, `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`: passed
  - docs-off tests (`otter-core`/`otter-io`/`otter-text` metadata and catalog, `otter-lang --no-default-features`): passed
  - `cargo test --release -p otter-lang --test lang nesting_up_to_the_limit`: passed
  - rustls-ring clippy, wasm clippy, the five WASI test binaries under `node scripts/run-wasi.mjs`: passed
  - `python3 scripts/hot-asm.py --check bench/hot-asm/aarch64-apple-darwin.txt`: matched (`run`: 765 instructions, `engine steps`: 1204)
  - `bench_compose --check bench/compose/baseline.txt`: passed
  - `python3 bench/instructions/run.py --check bench/instructions/aarch64-unknown-linux-gnu.txt --allocations-only`: passed (100 workloads)
  - `cargo fmt --all --check` and the ESP32 example's fmt check: passed
  - targeted CLI repros with the built `otter`

## F1 — P2 Iterating an empty fixed table (`{}`) types the element as `Never`, and `compose_flow` resets the body's input to `None`
- Location: `crates/otter-core/src/shards/iterate.rs:212-216`, `crates/otter-core/src/compose.rs:1728-1732`, `crates/otter-core/src/shards/iterate.rs:395-407`
- Evidence: `iterable` computes a table's element type as `Type::union(keys.chain(rest))`; for `{}` (no keys, no rest) that is `Type::never()`.
  1. `compose_flow_declaring(action, element, ...)` with `Never` hits `compose_flow_at_depth`'s divergence rule (`ty == Type::never()` → input `None`), while the bound name is declared `Never`. Observed with `otter check`: `{} | Map(v { v | Add(1) })` fails with "Math.Add needs Int, ... got None"; `{} | Map({ IsNone })` and `{} | Filter({ IsNone })` pass because the body's input silently became `None`.
  2. `Reduce` without `init:` on `{}` gets `acc_type == Never`, so its output is `Never` and the enclosing flow marks itself diverged (`compose.rs:1810`), as if `Reduce` were `Stop`.
  3. `filtered` turns `{}` into `{...: Never}`; a following `Map` again gets a `Never` element.
  - Observed by the publishing session (Linux x86-64, same commit): `{} | Map({IsNone})` composes and runs; `{} | Map(v {v | Add(1)})` fails at compose at `Math.Add`.
- Impact: `Never` is reserved for control-flow divergence; using it as an uninhabited element type gives confusing diagnostics and spurious divergence in the enclosing flow.
- Suggested direction: type an empty fixed table's element as `Any` (as `[]` is `[Any]`), or reject iterating a statically empty fixed table at compose; keep `filtered` on `{}` as `{}` or `{...: Any}`.

## F2 — P3 `Op::Rebuild` undercounts compose-time metering for map tables
- Location: `crates/otter-core/src/inline.rs:1464-1476`
- Evidence: under `C::METERED`, `Op::Rebuild` charges `items.len() * size_of::<Var>()`, but `iterate::rebuild` on a table builds `(Arc<str>, Var)` entries (larger per entry), and for `Filter` the kept values are cloned into the result without a `meter.nest` charge (their depth cannot exceed the input's, so only the size is undercounted).
- Impact: minor accounting discrepancy in `#( ... )` compose-time budgets when mapping or filtering tables.
- Suggested direction: charge the actual entry size when the source slot is a `Var::Table`, or pass the meter into `iterate::rebuild`.
