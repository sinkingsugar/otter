# Otter

Otter is a new Rust implementation of the [Shards](https://github.com/fragcolor-xyz/shards) runtime: Shards 2.0. The language is still Shards (`.shs` files); Otter is the runtime, the frontend and the `otter` command. Renamed from shards2 on 2026-10-11; older records and benchmark results use the old names.

**Status:** core prototype and language frontend working (2026-10-07): the compiled/state split on one directly resumable (stackless) engine, real async I/O (`Http.Get`), a hand-written frontend with `check --json`, hot reload, wasm (WASI) and ESP32 (QEMU) builds. Functions (`@fn`, golden path M5) and recursion (M7) are in, with private frames and state-preserving reload of edited bodies; values are 32 bytes with f32 vectors and struct tables (M6). Compose-time evaluation and flow parameters (M8, M9) followed. **Next:** the priorities in [`docs/host-requirements.md`](docs/host-requirements.md) §7. Current status is in `docs/current-state.md`.

## Starting work

Start with [AGENTS.md](AGENTS.md) for working rules and [docs/current-state.md](docs/current-state.md) for settled decisions, review status and the next step. These are the entry points for a fresh session launched from this directory.

## Why a new implementation

In Shards 1.x, each shard instance holds its parameters, its compose output and its runtime state together. Running the same wire N times (spawned entities, server connections, `Expand` items) rebuilds and recomposes everything N times. 2.0 compiles a wire once into a shared, immutable artifact, and each instance owns only a small runtime state. Changing that shard contract is deep enough to justify a new core rather than migrating 1.x in place.

C++ libraries are still used, but called from Rust; in 1.x, C++ is the host.

## Documents

- [`docs/golden-path.md`](docs/golden-path.md): the executable contract for functions, scope and the stackless engine (M0-M7).
    28	- [`docs/shard-metadata-and-compose.md`](docs/shard-metadata-and-compose.md): shared descriptions, catalog, argument decoding, and diagnostics; implemented for the full core catalog and `Http.Get`.

- [`docs/shards-2-compose-split.md`](docs/shards-2-compose-split.md): the core design, the 1.x audit behind it, what carries over from 1.x, and the validation plan.
- [`docs/prototype-shard-contract.md`](docs/prototype-shard-contract.md): the shard contract the prototype implements.
- [`docs/stackless-experiment.md`](docs/stackless-experiment.md): the scheduler experiment and decision (historical: the stackful scheduler was deleted after golden path M4), shared shard APIs (`LeafShard`, `AsyncShard`), real I/O, and matched benchmarks against 1.x.
- [`docs/values-and-types.md`](docs/values-and-types.md): tables, float vectors and type sets for the frontend slice.
- [`docs/embedding.md`](docs/embedding.md): embedding Otter in a Rust host and writing host shards.
- [`docs/surface-syntax-review.md`](docs/surface-syntax-review.md): the 1.x grammar reviewed for agent authoring, and the proposed parser decisions.
- [`docs/ai-first-roadmap.md`](docs/ai-first-roadmap.md): the overall strategy (reference copy; canonical in the 1.x repo).

## Results so far

Matched prototype benchmarks against 1.x (instance creation, memory per instance, steady state, resume depth, HTTP concurrency) are in `docs/stackless-experiment.md` and `docs/shards-2-compose-split.md` §5, with their limits. They support the instance-sharing design; they are not full-application performance claims.

The [runtime performance overview](docs/runtime-performance-overview.md) covers the latest release measurements and optimization headroom. Compose-selected builtin execution brings hot arithmetic chains near 1.x parity; some collection operations remain roughly 3–4× slower. The engine scales flat with instance count, and since golden path M4 resuming a suspended leaf costs the same at any nesting depth. The [VM execution report](docs/vm-execution-benchmarks.md) retains the full before/after measurements.

```sh
cargo test --workspace                                                        # acceptance tests
cargo run --release -p otter-core --example bench_instances -- 1000          # instance benchmark
cargo run --release -p otter-core --example bench_depth                      # resume cost vs depth
cargo run --release -p otter-core --example bench_async                      # polling vs notification
bench/shards-1x/run.sh path/to/1.x/shards                                     # the same benchmarks on 1.x
bench/http-concurrency/run.sh path/to/1.x/shards                              # HTTP concurrency, 1.x vs 2.0
```

## Crates

- `crates/otter-core`: the runtime core: compose, the scheduler (`stackless/`: frame arena, engine, mesh), lifecycle helpers, and the prototype shards (`LeafShard`/`AsyncShard` implementations, plus control flow).
- `crates/otter-io`: I/O shards (`Http.Get`) on a shared Tokio runtime, following 1.x's HTTP module. Native only; TLS via the `rustls-ring` or `native-tls` feature.
- `crates/otter-text`: text shards that need third-party crates: regular expressions (`Regex.Match`, `Regex.Search`, `Regex.Replace`, on the Rust `regex` engine), hashes (`Hash.Sha2-256`, `Hash.Sha1`, `Hash.Md5`, `Hash.Blake2-128`/`-256`, `Hash.Blake3`, `Hash.XXH3-64`/`-128`), `UUID` and `NanoID`. Native and wasm.
- `crates/otter-lang`: the language frontend: parser, lowering to wire definitions with a source map, `check`/`run`, and host-driven `Session` execution with state-preserving reload of edited functions.
- `crates/otter-cli`: the `otter` command: `cargo run -p otter-cli -- check --json file.shs`, `run`, `watch`, `describe`, `search`, `tool`. File watching and warm host sessions are covered in [the embedding guide](docs/embedding.md#5-warm-sessions-and-hot-reload), tools a host loads and calls in [its §6](docs/embedding.md#6-tools).

## Development

ESP32, ESP32-S3 and ESP32-C3 firmware builds use ESP-IDF and the stackless
runtime. See [the ESP32 build guide](docs/esp32.md) for the example, toolchains,
CI artifacts and hardware-validation limits.

```sh
cargo check --workspace
cargo test --workspace
cargo fmt --all
cargo clippy --workspace --all-targets
```

The toolchain is pinned in `rust-toolchain.toml`.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this work, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
