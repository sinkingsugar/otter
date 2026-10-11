# AGENTS.md

Shared working instructions for coding agents in this repository.

## What this is

Otter (Shards 2.0): a new Rust implementation of the Shards runtime, renamed from shards2 on 2026-10-11; the language keeps the name Shards and the `.shs` extension. The 1.x implementation (C++ core with Rust modules) lives at `fragcolor-xyz/shards`, usually checked out at `../shards`. Use it to learn what a feature is for, and for tests and reusable Rust code, but do not copy its runtime architecture. Its author's view: the intent was good, the execution often was not. Do not treat 1.x behavior as correct by default: when "1.x does X", decide whether X is good, choose the better design, and list the difference under "Deviations from 1.x" in `docs/current-state.md`.

Status: the core prototype is done and validates the design. The runtime has **one scheduler**: the iterative, directly resumable engine in `stackless/` (golden path M4). The stackful prototype scheduler was deleted after the M4 gate passed; nothing is written for two backends any more.

## Start a session here

1. Read [README.md](README.md) and [docs/current-state.md](docs/current-state.md).
2. Inspect `git status --short` and `git log -5 --oneline`. Preserve existing work; the status document is a dated checkpoint, not a substitute for inspecting the checkout.
3. Read the design documents relevant to the task below. Current decisions take precedence over historical proposals and benchmark plans.
4. Check [.agent-handoffs/](.agent-handoffs/README.md) for reviews and responses relevant to the task. Use the [review-handoff skill](skills/review-handoff/SKILL.md) to publish findings, address reviews or verify fixes. Discovery alone does not authorize unrelated fixes; implementation claims remain unverified until a verification record supplies evidence.

This repository should be usable without previous chat history or private agent memory. Keep durable decisions and unfinished work in the repository.

## Read first

- `docs/golden-path.md`: the approved plan for the current work (functions, scope, the stackless engine, values). It is the contract for milestones M0 to M7 and overrides older statements in the documents below where they conflict.
- `docs/metaprogramming.md`: the contract for M8 to M10 (compose-time evaluation, flow parameters, code as data and hygienic macros).
- `docs/host-requirements.md`: what a production host needs that 2.0 lacks (objects, services, cleanup on exit, concurrency, host exchange, runtime compilation, the C API, catalog and modules), with proposed decisions awaiting the maintainer.
- `docs/performance-gates.md`: the plan for strict runtime gates (Valgrind instruction and allocation counts per iteration, a wall-clock A/B tool), which land before Track A1.
- `docs/shards-2-compose-split.md`: the design and the source of truth for the core model. Read it before changing core code.
- `docs/prototype-shard-contract.md`: the shard contract (the decisions that replace 1.x's `shards.h`).
- `docs/stackless-experiment.md`: the scheduler experiment and decision (historical: stackful is gone), the shared shard APIs, and the matched benchmarks against 1.x.
- `docs/shard-metadata-and-compose.md`: metadata, argument decoding, structured diagnostics, and the completed catalog work.
- `docs/values-and-types.md`: tables, float vectors, type sets and the acceptance rule.
- `docs/embedding.md`: how a host crate adds its own shards and runs scripts (the guide for downstream projects).
- `docs/ai-first-roadmap.md`: a reference copy of the strategy. The canonical version is in the 1.x repo.

## Layout

- `crates/otter-core`: compose (`compose.rs`), the shard contract (`shard.rs`), the scheduler (`stackless/`: frame arena, engine, mesh), shared lifecycle helpers (`lifecycle.rs`), and the prototype shards (`shards/`).
- `crates/otter-io`: I/O shards (`Http.Get`) on a shared Tokio runtime, following 1.x's HTTP module. Native only.
- `crates/otter-lang`: the language frontend: hand-written lexer and parser with spans, lowering to `WireDef`/`ShardDef` with a source map, and `check`/`run` (`docs/surface-syntax-review.md`).
- `crates/otter-cli`: the `otter` command (`check [--json]`, `run`, `watch`, `describe`, `search`, `catalog`).
- `bench/`: benchmarks matched with 1.x (`shards-1x/`, `http-concurrency/`), compose costs gated in CI (`compose/`), and the authoring eval (`authoring/`, golden path §8; its README says how to run it).
- `examples/esp32`: ESP-IDF firmware embedding the core and frontend on the stackless scheduler; a separate workspace with its own lockfile (`docs/esp32.md`).

## Core rules

- **Compose once, instantiate many.** Shard compose output (`Compiled`) is shared and immutable. Per-instance runtime state (`State`) is separate and small. Never put compose output in per-instance state, and never mutate shared compiled data to change one instance's behavior.
- **Compose is deterministic.** It depends only on inputs declared through `ComposeCtx`. No hidden reads of the filesystem, clock or host state.
- **Device- and connection-bound resources** (GPU objects, sockets, DB connections) are not stored in `Compiled`. They are resolved through services scoped to their owning context.
- **Rust hosts, C++ is consumed.** C/C++ libraries go behind binding crates. Unsafe code needs a documented lifetime contract.

## Writing shards

Choose the shard API by what the shard does:

- **Cannot suspend** (most shards): implement `LeafShard` (`shards/leaf.rs`). "Leaf" means it never suspends, not that it does little work.
- **Waits on async I/O**: implement `AsyncShard` (`shards/async_shard.rs`) once. `start` returns one future per operation, from owned inputs. Spawn I/O through `otter-io`'s shared runtime (`otter_io::runtime::spawn`); never put a runtime or reactor inside a shard, and never block. Race every await of in-flight work against the cancellation token: dropping a Tokio `JoinHandle` alone only detaches the task.
- **Runs nested flows, outside the core** (a retry, a timeout, a container): implement `ControlShard` (`shard.rs`, `docs/embedding.md`): `resume` says which flow to enter with what input, or what to output; the engine runs the flows. `Maybe` is written this way.
- **Suspends or runs nested flows, in the core** (control flow like `When`, `Repeat`, `Match`): implements the full `Shard` contract (`shard.rs`). Its description, compose logic and compiled type live in `shards/mod.rs` or `shards/control.rs`; a composite exposes a `Control` description and the engine (`stackless/engine.rs`) enters its children and owns and resets its continuation (adapters in `stackless/shards.rs`). A directly suspending leaf (like `Pause`) keeps its resume point in `State` and resets it on every exit other than `Suspend`, including errors. Keep this set small and explicit.
- **Lifecycle**: use `lifecycle.rs` (`cleanup_each`, `instantiate_all`) for anything that instantiates or cleans up child flows, so every cleanup is attempted even when one panics.
- **Describing a shard** (`docs/shard-metadata-and-compose.md`): give it a `ShardDesc` with its parameter declarations as a `static`, and read arguments in compose through the decoded `Args` accessors, not by position. Write all prose (summary, help, parameter help) through `shard_doc!`, so the `docs` feature can compile it out for small builds. Never duplicate a name, version or parameter list: attach the implementation to the one description (`ShardType::new(desc).implemented_by::<S>()`), and add the shard to its crate's `CATALOG` list. Report compose errors as structured `Error::Diagnostic`s.
- **Tests**: acceptance tests live in `tests/prototype.rs`, `tests/trampoline.rs`, `otter-io/tests/http.rs` and `otter-lang/tests/lang.rs`; the ESP32 firmware reruns the core and frontend suites in QEMU, so new behavior gets a test there, with device-sized fixtures where a case would exceed the device's memory.

## Scope

The prototype milestone is complete. Next is porting the language front end and the first real modules. Port incrementally, keep the suite passing natively, on WASI and on ESP-IDF, and propose larger scope increases (graphics, physics, browser wasm) to the user instead of starting them. The gfx/physics 1.x baseline (design doc §5) comes before graphics porting.

## Commands

- `cargo check --workspace`
- `cargo test --workspace`
- `cargo fmt --all` and `cargo fmt --manifest-path examples/esp32/Cargo.toml --all` before committing (2-space indent, see `rustfmt.toml`)
- `cargo clippy --workspace --all-targets -- -D warnings`
- docs off: `cargo test -p otter-core -p otter-io --no-default-features --test metadata --test catalog` and `cargo test -p otter-lang --no-default-features`
- release nesting: `cargo test --release -p otter-lang --test lang nesting_up_to_the_limit`
- TLS: `cargo clippy -p otter-io --all-targets --features rustls-ring -- -D warnings`
- wasm lint: `cargo clippy -p otter-core -p otter-lang --target wasm32-wasip1 --lib --tests -- -D warnings`
- hot code: `python3 scripts/hot-asm.py --check bench/hot-asm/aarch64-apple-darwin.txt` (on Apple Silicon) summarizes the disassembly of the VM loop and the engine step; run it with `--compare REV` before and after any change to `inline.rs` or `stackless/engine.rs`, and refresh the baseline when the change is intended, saying why in the commit (`docs/runtime-performance-overview.md`, lessons 6 and 7). CI's `hot-asm` job (macOS) fails on any difference. Native builds use `.cargo/config.toml`'s block alignment; compare builds only with the same flags
- compose costs: `cargo run --release -p otter-lang --example bench_compose -- --check bench/compose/baseline.txt` (CI's `bench` job; after an intended allocation change, regenerate with `--write` and say why in the commit, see `bench/compose/README.md`)
- instructions per iteration: `python3 bench/instructions/run.py --check bench/instructions/baseline.txt` (Linux with Valgrind; CI's `instructions` job, exact). Locally, write a baseline at the base revision and check the change against it; after an intended change, commit the baseline CI's job uploads and say why (`bench/instructions/README.md`). `--allocations-only` runs natively on any OS
- wasm tests: `cargo test -p otter-core --test prototype --test metadata --target wasm32-wasip1 --no-run` and `cargo test -p otter-lang --test lang --target wasm32-wasip1 --no-run`, then run each emitted test `.wasm` with `node scripts/run-wasi.mjs <path>`. Install the target with `rustup target add wasm32-wasip1` if needed. Benchmark examples are native-only; do not use `--all-targets` for wasm.

The toolchain is pinned in `rust-toolchain.toml`. CI runs fmt, clippy and tests on Linux and macOS, clippy for the `rustls-ring` build, the suite on wasm (Node WASI), the compose cost check, and the instruction gate (Linux, Cachegrind). A separate workflow links the ESP32 firmware for three chips and boots each in Espressif's QEMU (`scripts/esp32-qemu.sh`) when the core, frontend or example change; it needs no local run, but when `otter-core` or `otter-lang` gain or change a dependency, refresh `examples/esp32/Cargo.lock` (`cargo update -w` in `examples/esp32`), since the firmware builds with `--locked`.


## Git

Golden-path work (`docs/golden-path.md`) happens on the `golden-path` branch, with a draft pull request to `main` so CI runs on every push; other work goes on `main`. Commit after the full check set above passes locally, push, and watch the CI run (`gh run watch`); CI runs on pushes to `main` and on pull requests. Review findings from Astra are addressed in follow-up commits with regression tests.

## Private projects

Do not name or describe private downstream projects anywhere in this repository: docs, code, tests, records or commit messages. Refer to them generically ("an external host"). This repository is meant to be public.

## Keep the handoff current

When a milestone or decision changes, update `docs/current-state.md` and the relevant design document. Keep this file for stable working rules, and the README for orientation. Record what is implemented, what remains, and what was actually verified; distinguish reported CI results from local checks. Avoid copying test counts or benchmark tables into multiple handoff files. `.github/workflows/ci.yml` is the executable reference for CI commands.

A new session should not need temporary files from a previous review. Regression tests belong in the repository; `/tmp` reproductions are supplementary evidence only. Keep `CLAUDE.md` as an entry point to these shared instructions rather than a second copy.
