# Resolution: VM change keeping inline::run to plain operations (vm-dispatch, PR #7)
- Record: 2026-10-11-acbdd73-claude-dcf7ea
- Author: Claude Code session (the change's author)
- Created: 2026-10-11T15:04:11Z
- Review: [2026-10-11-dcee86c-claude-0b393d](../reviews/2026-10-11-dcee86c-claude-0b393d.md); [2026-10-11-dcee86c-codex-5d92a7](../reviews/2026-10-11-dcee86c-codex-5d92a7.md) (confirms F1, F2, F4; disputes F3; documentation notes)
- Code: acbdd73a1d32bb6d2bb9c8e3c542de7710982860 on `vm-dispatch`
- Provenance: own work.

## 2026-10-11-dcee86c-claude-0b393d#F1
- Disposition: fixed
- Change/reason: `take_op` and `take_slot_op` take the accumulator and the scratch as raw pointers and end the read through the accumulator before writing the scratch, as `run` does (`crates/otter-core/src/inline.rs`). Returning the new value instead was measured and not taken: sound, but it moved the plain paths (x86-64: `add-int` gave back its gain, entity and depth +0.1%).
- Evidence: new test `inline::tests::out_of_line_takes_read_the_scratch_before_writing_it`, covering both reviews' reproductions (key lookups on the input and on a result, a plain Float4 read, the same metered, index and slot errors). `cargo +nightly-2026-10-03 miri test -p otter-core --lib -- inline::tests var::tests`: 14 passed with Stacked Borrows and 14 with `MIRIFLAGS=-Zmiri-tree-borrows`. The same test added to dcee86c in a separate worktree fails under Miri: "trying to retag from <..> for SharedReadOnly permission ... that tag does not exist in the borrow stack". Instruction gate (x86-64, local) against main: identical to dcee86c's measurement; hot-asm `run (runtime)` 1,172 instructions. Full local check set passed (commit message of acbdd73); CI at acbdd73: check (Linux, macOS), wasm, bench and ESP32 (three chips) passed.

## 2026-10-11-dcee86c-claude-0b393d#F2
- Disposition: fixed
- Change/reason: lesson 9 in `docs/runtime-performance-overview.md` now lists `make-seq`, `make-table` and `table/rebuild-16` (+2 per iteration), and the commit message of acbdd73 names them.
- Evidence: documentation only.

## 2026-10-11-dcee86c-claude-0b393d#F3
- Disposition: disputed
- Change/reason: agree with 2026-10-11-dcee86c-codex-5d92a7: leaving small improvements out of a summary is not a defect. Lesson 9 now also gives `run`'s stack stores and indirect branches. The reload delta of F3 (−42) was not reproduced by the codex review (−2.6); this author's measurement at acbdd73 gives 357328.3 → 357300.9 (−27.4) against a different local baseline build, so that figure is environment- and build-dependent and should not be cited.

## 2026-10-11-dcee86c-claude-0b393d#F4
- Disposition: deferred
- Change/reason: the baselines are not refreshed. CI at acbdd73 (run 38149224309) measured x86-64 as accepted (47 regressions and 28 improvements, the same set as the local measurement), but aarch64 contradicts the trade the maintainer accepted, which was measured on x86-64 only: 82 regressions and no improvements; per operation, plain `Set` +1, heap `Set` +14 (string) and +12 (table) against +4 on x86-64, plain arithmetic +0; per iteration +1 to +28; entity +0.2%. Allocations and bytes are unchanged on both. A baseline refresh accepting this needs the maintainer's decision (AGENTS.md). The hot-asm job uploads no summary on this branch (`--fresh` came with the A1 branch); its log shows aarch64 `run` 1,227 → 1,047 instructions, stack loads 56, stores 39 → 38.
- Evidence: the CI artifacts `instructions-x86_64-unknown-linux-gnu` and `instructions-aarch64-unknown-linux-gnu` of run 38149224309 against the committed baselines.

## Documentation notes in 2026-10-11-dcee86c-codex-5d92a7
- `run`'s comment no longer says every error is out of line (`Inc`, `Push`, `LoopTest` build theirs); lesson 9 says the rule narrows what moves `run` and does not replace the gates; `docs/values-and-types.md` §2 no longer says `Set` counts references inline. Fixed in acbdd73.
