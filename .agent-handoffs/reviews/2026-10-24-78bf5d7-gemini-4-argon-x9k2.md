# Review: PR #19 (`catalog-defaults`: `Expect*(default:)`, `First`, `Last`, `String.PadStart`, `String.PadEnd`)
- Record: 2026-10-24-78bf5d7-gemini-4-argon-x9k2
- Author: gemini-4-argon review thread
- Created: 2026-10-24T21:54:00Z
- Base: fe9fc4864673b298fe10b5967cfb19e01a66e1f2
- Reviewed: 78bf5d7de08eca4b79fc20b021fbe36dfd9451ec (clean archive in a temporary directory)
- Scope: `crates/otter-core/src/shards/{values,checks,seqs,text,mod}.rs`, `crates/otter-core/src/compose_time.rs`, `crates/otter-lang/tests/lang.rs`, `examples/tools/{flights,notes-search}/*.otr`, `bench/authoring/reference/functions.md`, `bench/instructions/*.txt`, `docs/{current-state,iteration-and-access}.md`
- Provenance: fresh review (published by the coordinating session)
- Checks (macOS aarch64):
  - `cargo test --workspace`; clippy (native, rustls-ring, wasm); `cargo fmt --check` (workspace and ESP32 example); docs-off tests; release `nesting_up_to_the_limit`; the five WASI suites under Node: passed
  - `python3 scripts/hot-asm.py --check bench/hot-asm/aarch64-apple-darwin.txt`: matches
  - `bench_compose --check bench/compose/baseline.txt`: within the baseline
  - `python3 bench/instructions/run.py --allocations-only --check bench/instructions/aarch64-unknown-linux-gnu.txt`: 100 workloads, no regression
  - CLI repros (`check`, `run`, `describe`): union narrowing (`{a: Int} | Int`, `[Int] | String`, `[Int] | [String] | None`), `Expect*(default:)` widening when the default does not fit the narrowed member, `First`/`Last` on empty, Any, union and none, padding on multi-byte and combining characters: as specified

No findings within scope.

## Verified areas
- **Kind check vs `Type::admits`:** every `Expect*` target type admits a value iff its variant matches; `Expect*Seq` checks every element's variant, identical to the old typed check since every element kind is a scalar or `{...: Any}`. Narrowing a union by the outer tag is sound because the input type guarantees membership.
- **`default:` acceptance and output type:** without `default:` the strict leaf is unchanged; literals are checked with `admits`, variables with `accepts`; the output widens to the target type when the default does not fit the narrowed input.
- **`First`/`Last`:** static, `Any` and union typing as specified; distinct errors for an empty sequence and a non-sequence; `default:` covers both.
- **Padding:** `with` is exactly one Unicode scalar (compose error for a constant, activation error for a variable); a width at or below the length returns the input unchanged; overflow and `try_reserve_exact` failure are handled. (This reviewer read the compose-time metering as charged; see [astra-f8c2#F1](2026-10-24-78bf5d7-astra-f8c2.md) for the uncharged no-padding path.)
- **Docs:** the 1.x deviations are recorded in `docs/current-state.md`.
