# Review: PR #18 — an interior `?` in a path is dropped (supplement)
- Record: 2026-10-24-11f8b5c-stitch-gamer-q7m2
- Author: Stitch coordinating session (gamer), from a progress note of the gpt-6-astra review thread
- Created: 2026-10-24T12:00:00Z
- Base: b8af8d458458ac13613b68a99efc8cd7449eac14
- Reviewed: 11f8b5c225df9a99b47064cf913b4a3af3bde9d0
- Scope: lenient path parsing only. Supplements [2026-10-24-11f8b5c-astra-cw75](2026-10-24-11f8b5c-astra-cw75.md), whose final record omitted this finding from its interim notes.
- Provenance: finding reported statically by the astra thread; reproduced by this session.
- Checks: the built `otter` CLI at the reviewed commit (Linux x86-64), scripts below.

## F1 — P2 A `?` inside a path is silently ignored
- Location: `crates/otter-lang/src/parser.rs`, `path()` and its lenient-mark handling
- Evidence: `"{\"a\": 1}" | FromJson | .a?.b | Log` fails with `Take: .b needs a table, got Int`; the same with `j.a?.b` on a bound `j`. The interior `?` is accepted and has no effect, so the path is strict. `"{}" | FromJson | .a?.b?` gives `none` only because of the trailing `?`.
- Impact: `docs/iteration-and-access.md` §2 allows `?` only directly after a whole path; elsewhere it must be an error. A writer (or a model used to `?.` from other languages) believes `.a?.b` is guarded when it is not.
- Suggested direction: reject a `?` that is followed by another segment, with a located diagnostic and a fix-it moving it to the end (`.a.b?`, which makes the whole path lenient). Test both the step form and the variable form.
