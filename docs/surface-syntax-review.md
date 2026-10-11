# Surface Syntax Review for the 2.0 Frontend

**Status:** review 2026-10-04; §5 agreed the same day. The parser (1), `;` comments (5), `And`/`Or` (9) and the top level (10) were decided explicitly as proposed; the rest is adopted as written.
**Scope:** the 1.x grammar (`../shards/shards/lang/src/shards.pest`) read with an agent as the primary author. The question is what the 2.0 parser accepts, rejects and reports, before it is ported. The language model itself (pipes, wires, sub-flows, named parameters) is not reopened: [ai-first-roadmap.md](ai-first-roadmap.md) §3.3 settled it, and nothing here contradicts that.

## 1. Method

- Read the 44-rule pest grammar and the reader (`read.rs`).
- Ran each suspected trap through the 1.x binary (`build/Release/shards new`, shards at the local checkout).
- Took the friction an agent actually hit while writing the scripts of an external host project. These are the only real-use evidence so far.
- Counted how often each construct appears in the 166 1.x test scripts (`shards/tests/*.shs`), to estimate the cost of changing it.

## 2. What already works for agents (keep)

- **Pipes, sub-flows in braces, `Name: value` parameters and `@wire`** have good priors (shell pipes, Elixir, keyword arguments, decorators). The roadmap's "do not Pythonize" stands.
- **`|` is optional between shards** (`1 Log("x")` runs like `1 | Log("x")`). A forgotten pipe does not change the meaning, which suits generated code.
- **Commas are whitespace**, so `[1, 2]` and `[1 2]` are both accepted (JSON prior, no cost).
- **Case carries the role:** `Upper.Case` is a shard, `lower-case` is a variable, `Type::Value` is an enum. Models follow this reliably.
- **Strict numeric types** (`1.5 | IsMore(0)` is a compose error, "Cannot compare Float with Int") catch real mistakes; it needs a repair hint, not a relaxation.

## 3. Findings

Severity is about agent authoring: **high** means silent wrong behavior, or an error an agent cannot act on.

| # | Finding | Evidence | Severity |
|---|---|---|---|
| 1 | **`;` starts a comment.** C/JS habits silently drop code: `1 \| Log("a"); 2 \| Log("b")` logs only `a`, with no error. | Probe. 27 test files use `;` comments at line start, 1 after code. | High |
| 2 | **`And`/`Or` are flow control, not boolean operators.** Outside a condition block, a false `And` exits the enclosing flow without an error. | Real use: it silently ended a script's main loop on the first match. Probe: `ForEach({... false \| And \| Log("never")})` skips the rest without a message. | High (semantic) |
| 3 | **Parse errors name grammar rules.** `expected EOI, AssignOp, or Pipeline`; an unclosed `{` is reported at the next `)` as `expected AssignOp, Pipeline, or Params`, without pointing at the brace that is open. pest stops at the first error. | Probes. | High (repair loop) |
| 4 | **A lowercase shard name becomes a variable read.** `log("x")` reports `Get (log): Could not infer an output type`, not "unknown shard log, did you mean Log". | Probe. | Medium |
| 5 | **Number forms fail confusingly.** `1e3` parses as `1` followed by the variable `e3`; `1.` and `.5` are syntax errors. Models write scientific notation. | Probe: `Get (e3): Could not infer...`. | Medium |
| 6 | **`-` is part of identifiers**, so `a-1` is the variable `a-1` (`Get (a-1): Could not infer...`). Kebab-case names are idiomatic and worth keeping; the error needs to say what happened. | Probe; real scripts use names like `max-yd`. | Medium (diagnostic) |
| 7 | **Access paths cannot mix keys and indices.** `t.a.1` and `s.0.a` are syntax errors (`TakeTable` takes only keys, `TakeSeq` only indices). Real scripts fell back to `Take("units") \| ExpectSeq`. | Probes; real use. | Medium |
| 8 | **`{}` is an empty table, everywhere.** Where a flow is expected, `When({true} {})` fails with `Parameter Action not accepting this kind of variable: {}`. | Probe. 4 test files use `{}` as an empty table on purpose. | Low-medium |
| 9 | **Two assignment syntaxes.** `= x` (Ref), `>= x` (Set), `> x` (Update) and `>> x` (Push) beside the word forms. The roadmap already made word forms canonical; the operators are in 137 of 166 test files. | Roadmap §3.3; counts. | Low (settled) |
| 10 | **Two spellings of none** (`none`, `null`), and a `~` number prefix. | Grammar; `null` in 6 files, `~` in 2. | Low |
| 11 | **Top level mixes declarations and execution.** A file declares wires and meshes and also runs code, and `@run(...)` is a statement whose result can be piped. Root-level code ran in the CLI's spin loop (7.99 s of CPU over 8 s of `Pause`, measured in real use). | Real use. | Medium (semantic) |
| 12 | **Statements chain values implicitly.** Each statement's input is the previous statement's output, so a source shard placed mid-script got a non-None input and failed to compose. | Real use. | Low (2.0 descriptors fix it: an `Ignored` input accepts anything) |
| 13 | **Unknown keys on open tables read as none.** `{a: 1} \| Take("b")` gives none silently. | Probe. 2.0 types table literals as fixed tables, so the typo fails at compose ([values-and-types.md](values-and-types.md)). | Fixed by the type work |

## 4. The parser: hand-written, not a pest port

`current-state.md` proposed porting the pest grammar. Findings 3, 7 and 8 change that recommendation:

- **Compatibility comes from the language, not the parser generator.** The conformance scripts need the same syntax accepted, not the same `.pest` file.
- **Agent-quality errors need control pest does not give:** naming the unclosed `{` and where it opened, reporting several errors per `check`, recovering at statement and brace boundaries, and messages in language terms ("missing `}` for the flow opened at 1:9") instead of rule names.
- **Every node needs a span** for the source side table and the diagnostic occurrence paths, and a formatter (canonical word forms, roadmap §3.3) wants a lossless syntax tree that keeps comments.
- **Size:** the grammar is small (literals, identifiers, `(`/`[`/`{` nesting, `|`, `@`, `#`), so a lexer plus a recursive-descent parser is on the order of a thousand lines, about what `read.rs` already is, without the 1.x runtime coupling.

The 1.x AST shapes are still a useful reference for what to produce.

## 5. Decisions

**Parser**

1. **Write the parser by hand** (lexer plus recursive descent) in `otter-lang`. Spans on every node, error recovery, multiple diagnostics per check, and messages that name language constructs. The 1.x grammar is the reference for what is accepted.

**Accepted syntax** (what the 2.0 parser accepts beyond the 1.x behavior)

2. **Paths mix keys and indices:** `t.a.1`, `s.0.a`.
3. **Numbers:** accept exponents (`1e3`, `2.5e-3`). Reject `1.` and `.5` with a fix ("write 1.0", "write 0.5"). Drop the `~` prefix.
4. **`{}` resolves by position:** where the parameter declares the flow form, `{}` is an empty flow; elsewhere it is an empty table. The decoder already knows each parameter's forms.

**Removed or diagnosed**

5. **`;` comments are rejected** with a diagnostic that offers `//`. The conformance batch is rewritten mechanically. The alternative is accepting `;` only at the start of a line, which keeps old files working but leaves two comment syntaxes.
6. **A lowercase name followed by `(`** is reported as a probable shard call, with the closest shard name. A name like `a-1` or `e3` that resolves to nothing is reported as an unknown variable, with a hint that `-` belongs to names and that subtraction is `Math.Subtract`.
7. **One spelling of none:** `none`. Reject `null` with a fix.
8. **Operators** (`=`, `>=`, `>`, `>>`) are accepted, because most real scripts use them. Diagnostics, docs and a later formatter use the word forms.

**Semantics** (compose-level, not grammar)

9. **`And` and `Or`** are allowed only where a condition is evaluated (predicate flows of `If`, `When`, `While`, `Repeat Until:`). Elsewhere they are a compose error that points to the condition-block form. Boolean combinators that do not exit the flow can be added if needed.
10. **Top level is declarations** (`@wire`, `@mesh`, `@schedule`, `@define`); `@run` is the entry point and its result is not piped. Loose root code is either rejected or wrapped by the host into a wire on a mesh, so it never runs in a spin loop.
11. **Source shards** declare an `Ignored` input and compose with any input type (finding 12).

## 6. Measuring it

The roadmap's benchmark (§3.3, decision 3: tasks × syntax variants × models, first-pass `check` success) is still the way to settle taste with data. A smaller version fits this slice once `check --json` exists: replay the real authoring mistakes (findings 1-7) as `check` cases and require that each produces a located diagnostic with a usable fix. That turns this review into regression tests.

## 7. Beyond the traps: design fixes

§3-§5 fix traps. These five go further, into the design. All five were agreed on 2026-10-04 as problems to fix; the designs below are the plan. Where a change adds syntax, the §6 measurement still checks it.

Examples come from real scripts. Pipelines inside `[...]`, table values and parameters need no parentheses: `|` binds tighter than the space between elements. Verified on 1.x: `["bite " x | Math.Subtract(y) " s"]` has three elements. The canonical form omits the parentheses.

1. **Conditions.** `If({far | And | can-point} ...)` reads as a pipe. Add `All`, `Any` and `Not`:
   - `All` and `Any` take one or more conditions, evaluated left to right with short-circuit. Each condition is a Bool variable, a Bool literal, or a flow that outputs Bool, so the example becomes `If(All(far can-point) ...)`.
   - `Not` negates a Bool input.
   - This needs variadic positional parameters in `ParamDecl`. `All` and `Any` evaluate flows, so they are control-flow shards with both backend implementations.
   - **Implemented.** `And`/`Or` are not shards in 2.0 (changed from the earlier plan to accept them inside conditions): `And` and `Or` exit the enclosing flow, and supporting that inside predicates needs a predicate-scoped return. Instead, both are a construct error that gives the exact rewrite (`If({a | And | b} ...)` becomes `If(All(a b) ...)`) and suggests `All`/`Any`. This supersedes §5.9's compose-error plan.
2. **String interpolation.** Add `f"cast {cast-n}: bite {bite-at | Math.Subtract(cast-at)} s"`:
   - Each `{...}` holds a pipeline, and `{{` / `}}` write literal braces.
   - The plain `"..."` string keeps braces literal, because 1.x strings carry shader and JSON text full of braces.
   - The frontend lowers it to the existing `[...] | String.Format`, so no runtime change is needed. The `f` prefix follows Python's f-strings, the strongest prior. **Implemented** (lowered to `Seq.Make` + `String.Format`).
3. **Labeled flows.** For shards with more than one flow parameter, the canonical form names them: `If(Predicate: ... Then: ... Else: ...)`, `Repeat(Action: ... Until: ...)`. Docs, catalog examples, diagnostics and the formatter use it. The decoder still accepts positional arguments. The flow names follow 1.x: `When`, `While`, `Once` and `Repeat` call their flow `Action` (renamed from 2.0's earlier `Body`, 2026-10-04).
4. **Invisible input.** The dataflow model stays; tooling makes it visible:
   - `check --json` can report the inferred input and output type of every shard, keyed by occurrence path, so a tool can show the type after each line.
   - The formatter puts one pipeline per line.
5. **Output rules in diagnostics.** A type mismatch on a shard's input says where that input came from: the producing shard, and any shards it passed through on the way (`When` passes its input through). Compose knows the previous shards and their `OutputDesc`, so this is a core change. **Implemented** (`input_from` in diagnostics, [shard-metadata-and-compose.md](shard-metadata-and-compose.md) §6).
