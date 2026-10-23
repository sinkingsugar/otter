# Metaprogramming: compose-time evaluation, flow parameters, code as data

Status: **agreed design** (Opus, Astra, Fable, Gemini, Giovanni; 2026-10-08). M8 and M9 are implemented and reviewed, every finding verified (§2.5, §3.5); M10 follows the host priorities ([host-requirements.md §7](host-requirements.md#7-priorities-2026-10-11): after or alongside objects, services and concurrency), its §4.1 cases supplied by Giovanni. This document is the contract for M8 to M10, the way [golden-path.md](golden-path.md) is for M0 to M7; golden path D1, §9 and §12 point here.

Before starting, read `current-state.md` and the latest records in `.agent-handoffs/`. Do not assume older review findings are still open: the M5 to M7 findings, frame caching, `Var` alignment and fresh wire locals are all closed there.

The rename (the working name is Otter) is a **separate** change. It is not part of these milestones, and none of the work here should wait for it or anticipate it.

## 1. The ladder

Three rungs, built in order. Each one is useful alone, and each one is reused by the next.

| Rung | Milestone | What it is | Typical use |
|---|---|---|---|
| Compose-time evaluation | M8 | `#( ... )` runs a pure pipeline during compose and inlines its value | constants, lookup tables, derived sizes |
| Flow parameters | M9 | a block passed into a function or shard, run with `Run` | `Retry`, `Timed`, locks, UI containers, `ForEach` |
| Code as data | M10 | `Code` values, `Quote`, `$` splices, `@macro` | binding forms, generated declarations, schema-driven code |

Rule for the primer and for reviews: **prefer the lowest rung that works.** A plain function, then a flow parameter, then a macro. Macros never stand in for missing higher-order flows.

Reading rule: **`#( )` and `@` happen at compose time; nothing else does.** `@fn`, `@const` and `@macro` declare, `@name` reads a define or a constant, `@Name(...)` expands a macro, and `#( ... )` evaluates. A call without either always runs at runtime.

The surface is already reserved by the frontend: the parser accepts `#( ... )` (lowering reports "evaluation while loading is not supported yet") and `@Name(...)` (lowering reports "templates and builtins are not supported yet"), and the lexer reads `$name` as an identifier. These milestones give those forms their meaning; check the current lexer and lowering before adding new tokens.

`$` followed by a digit (`$0`, `$1`) and `$i` are 1.x's implicit loop variables of `ForEach` and `Map`. 2.0 does not port them (they shadow each other in nested loops, and nothing declares them): loops name what they bind, value first, `seq | ForEach(x i {...})` and `tbl | ForEach(v k {...})`, and the body's input is the element ([iteration-and-access.md §4](iteration-and-access.md#4-iteration), which supersedes the earlier `{key value}` entries and `Seq.Enumerate`). From M8 on, `$<digit>` is a lexer error with help saying the element is the block's input; models trained on 1.x will write it. `$<letter>` keeps its current meaning until M10 (§4.3).

## 2. M8: compose-time evaluation

### 2.1 Semantics

- `#( pipeline )` in a value position composes and runs `pipeline` during compose, and the step becomes the resulting literal. With a `Square` that squares its input, `#( 4 | Square )` becomes `16`. (`#( Square(4) )` is a different call: it passes a parameter, so it only works for a function that declares one.)
- **Eligibility is inferred** from the compose analysis, transitively over everything the pipeline reaches: no effects (no IO, time, randomness, suspension or `unknown`), no `Keep` or stateful function, no mesh access, and every native shard on the compose-time-safe list below. Helpers need no annotation; `pure: true` stays what it is today, a promise that compose checks. A violation is a located `not-compose-time` diagnostic naming the offending shard or function and the reason.
- **Native shards are eligible only from an explicit, audited list.** An empty effect set is not enough, and unknown or missing metadata means not eligible. A shard on the list must, while it runs at compose time:
  - charge fuel in proportion to its work (per element, per byte), not once per dispatch, so one long native operation cannot escape the fuel budget;
  - check the per-value size limit **before** it allocates a result, so an oversized value is a diagnostic rather than an allocation failure.

  Flat code charges fuel on its own instructions too (§2.2). An allocation is charged a unit of fuel per byte, so the fuel also bounds everything an evaluation allocates; no single allocation, and no value a shard reads through (compares, prints), exceeds the value limit; and a value built during an evaluation nests at most 64 levels, so nothing that reads or drops it recurses deeper. Frames have a fixed number of slots at a capped depth.
- Its input is `None`. It can read other compose-time values (literals, `@const`s, defines), never runtime locals. A runtime local inside `#( )` is `unknown-variable`, with help saying `#( )` cannot see runtime values.
- `@const(name value)` declares a program-level constant. `value` is a literal or a `#( )`. It is read as `@name`, sharing the namespace of host defines (a clash is `duplicate-binding`). With `ignore-redefined: true` (1.x's `@define(... IgnoreRedefined: true)`) it is a default instead: a script argument of the same name (`otter run -D name=value`) replaces it, read as the default's type (an Int, Float or Bool default parses the argument, which is a string, and an argument that does not parse is `script-argument-type`); the default must be a number, string or bool literal.
- **The result** is checked against the pipeline's composed output type, then inserted with its exact literal type, as if it had been written by hand: a pipeline with output `Any` that evaluates to `42` becomes the `Int` literal `42`. Only values that can be written as literals qualify: no host objects, and no `Code` before M10 (`code-outside-macro`). Floats round-trip exactly: the literal is the value's bits, NaN (with its payload) and the infinities included. A finite value prints (`expand`, `check --json`) in the shortest form that reads back to the same bits; NaN and the infinities print as `NaN`, `inf` and `-inf`, which have no source literal yet.
- **Cycles:** a `#( )` that reaches the function whose body contains it, directly or through other functions, and `@const`s that read each other through `#( )`, are `compose-time-cycle`, located at the site that closes the cycle. M10's macro phase cycles use the same class.
- **Failures:** a runtime error inside the evaluation (`Assert`, an index out of range, division by zero) is a `compose-time-error`. Its path carries a `PathStep::Evaluation` with the `#( )` site, followed by the path to the failing shard (M10's `PathStep::Expansion` is its twin), so it is located at that shard, also inside a function the evaluation called (§2.5). Failures are not cached; only successes are.
- `@Name(...)` where `Name` is a function, not a macro, is `not-a-macro`, with help pointing at `#( Name(...) )`. There is exactly one way to evaluate at compose time (`#( )`) and one way to expand (`@Name(...)`); the two never alias.
- **Deviation from 1.x:** 1.x's `#( )` ran arbitrary shards while loading, effects included. Here it is pure, because compose must be deterministic and depend only on declared inputs. Uses that need outside input, like embedding a file's contents, use `@read("file")`, which reads the file while the program loads, before compose, and records it as a dependency of the program (so a watcher reloads on its change); the value reaches compose as a literal, so compose stays pure.

### 2.2 Execution and budgets

- It runs on the real engine in a temporary instance with no mesh. There is no second interpreter.
- Purity does not imply termination, so every evaluation has budgets, all host-configurable on the compose context:
  - **Fuel:** engine dispatches plus VM instructions plus the proportional charges of native shards (§2.1). The flat-code paths (`Jump`, `LoopTest`, `VmCall`) consume it too, otherwise `Repeat(forever: true)` would run unmetered.
  - **Depth:** the call-depth limit, passed through `ComposeCtx` (at runtime it is the mesh's `max_call_depth`; compose has no mesh to read it from).
  - **Value size:** the per-value limit native shards check before allocating (§2.1).
  - **Output size:** the encoded size of the resulting value, checked once more at the end.
- Exceeding a budget is a located `expansion-budget` diagnostic, never a hang. Defaults should be small enough for the device (the ESP32 firmware composes on device).

### 2.3 Caching and reload

- Results are cached in the existing compose cache, keyed by the composed pipeline's content and its recorded dependencies (the functions it reaches, at their body revisions). Source spans and provenance are not part of the key.
- A cache entry records the fuel, the peak depth, the largest value and the output size its evaluation used. A hit is checked against the **requesting** compose context's limits, so a host with smaller limits (a device) never accepts a result it could not have computed itself; it gets the same `expansion-budget` diagnostic.
- The result is part of the caller's body, so an edit to a function it reached recomposes the caller, recorded like `Dep::Inlined`.
- The compose cache lives in memory, per process. It is never persisted or shared across targets, so float results that differ between targets (native, wasm, the ESP32's soft float) never meet. If the cache is ever persisted or shared, the target becomes part of the key.

### 2.4 Gate

- `#( )` reaching `Keep`, mesh access, IO, `Time.Now`, `Pause` or a native shard not on the safe list is a located `not-compose-time` error, also when it happens indirectly through another function.
- A repeated evaluation is a cache hit (asserted on the cache, not timed); a hit is rejected by a context with smaller limits.
- An infinite loop ends with `expansion-budget`, both inside a framed call and inside flat code; so does a loop that grows a value past the value limit, before the allocation.
- `compose-time-cycle` through a function and through two `@const`s; `compose-time-error` with the `#( )` site in its path.
- An `Any`-typed pipeline evaluating to an `Int` composes as an `Int` literal; float results round-trip (NaN, infinities, shortest printing).
- `$0` is a lexer error with the 1.x help.
- Under preserving reload, an edit to a function reached from `#( )` updates its sites.
- One authoring-eval task needs a compose-time value.

### 2.5 Implementation (2026-10-08)

- **Representation.** The frontend lowers `#( ... )` to `ParamValue::Eval(flow)` wherever a literal can stand: a pipeline step (a `Const` whose value is evaluated), an argument that takes a literal (native shards and function calls), an element of a sequence or table literal (the literal becomes the evaluation of the one `Seq.Make` or `Table.Make` that builds it), and `@const`. Compose (`compose/evaluate.rs`) evaluates such arguments before it decodes the shard's arguments, so the value goes through every check a written literal does. A runtime constructor holding an evaluated element hoists it like a computed value. Match case values and parameter defaults must be known while lowering and reject `#( )`.
- **Constants.** A constant without a `#( )` is lowered once to a value that every read shares, so a lookup table read in many places costs nothing per read. Compose keeps it shared: the walks over a value (converting its tables to struct form, its type, `admits`, equality) visit storage held in several places once, and the cache key hashes at most the first 64 nodes of a literal (`Var::hash_prefix`), so a constant built from copies of another costs compose what it holds, not what it expands to, at every read. One holding a `#( )` is lowered at each read, under the reading occurrence, so a diagnostic inside its pipelines points into its definition; each constant is lowered once before any body to report its problems once. Constants built from each other multiply at every level, so both kinds are bounded: a value constant built from others by its size as text, within the default value limit (1 MiB natively, 4 KiB on the device; `expansion-budget` at its definition), and constants holding a `#( )` by the source-map nodes their reads add in total, 32,768 natively and 128 on the device (`expansion-budget` at the read that passes it). Nodes, not source bytes, because each node is a lowered step with its location, about a hundred bytes; a source bound let a 4 KiB budget take hundreds of KB. Their cycles are found while lowering (`compose-time-cycle` at the read that closes them); a function cycle is found at compose, at the call inside the evaluation that reaches a function whose body is being composed.
- **Eligibility** is recorded per node in `Analysis::not_compose_time` (the first native shard reached that is not on the list), next to effects, lifetime and mesh access, so cached bodies carry it. The list is `compose_time::SAFE`: core shard types by identity and audited version. It excludes `Keep`, `Once`, `Log`, `Maybe` (it logs what it catches), `Time.Now`, `Pause`, `Spawn`, `Stop`, `Probe` and `Return` (refused inside `#( )` itself; a function the evaluation calls may still return). Host shards are never eligible.
- **Metering.** `compose_time::Meter` holds the budgets of one evaluation. The engine charges a unit per step that starts work and records the depth at each entry; the VM's metered form is a separate instantiation of `inline::run` (`VmCalls::METERED`, a constant), charging every instruction and checking `Push` and the constructors, so runtime code pays one `Option` check per engine step and per constructor run, nothing per instruction. Measured A/B against `fc7c6bf` (release, aarch64, five alternating rounds): a flattened loop 6.43 against 6.70 ns per iteration, an engine-dispatched `Match` 43.0 against 43.0 ns, the entity tick 88 against 88 µs: no regression beyond noise.
- **Defaults** (`EvalLimits::default()`): native fuel 10,000,000, depth 256, value 1 MiB, output 64 KiB; ESP-IDF fuel 8,000 (so an evaluation allocates at most about 8 KB of the device's heap, below the classic ESP32's low-water of 9.7 KB over the acceptance suites, CI at `f7d3b25`), depth 32, value 4 KiB, output 2 KiB. A shard charges a unit per byte it allocates and per 16 bytes it reads through; `Match` charges each case it compares by the smaller side's size. Measuring a value (its size, its nesting) walks it with one open iterator per level, so the walk's own memory follows the nesting, not the element count.
- **Cache.** Results live in the compose cache by pipeline, revalidated by what they read and checked against the requesting limits; like the rest of the compose cache they are never evicted. The caller records the reached functions as `Dep::Inlined` and the usage as `Dep::Evaluation`, which is valid only under limits that cover it.
- **Failure locations.** A failed evaluation is located by running it again: it is deterministic, so it fails at the same shard. The pipeline is composed again in a fresh cache whose flows record where each instruction comes from in its definition (`ComposeCache::locating`, `flow::Origins`: one entry per shard, the code a flattened composite or an inlined call brought hanging below the shard that brought it, and the parameter that holds a child flow), then run under a locating meter that records each level the failure passes through: the frame and instruction that failed (the VM reports the failing instruction, also inside a callee it runs on a kept frame), each frame the failure leaves, and a child flow a composite runs inside its own step; a composite that handles a failure clears the record. A `compose-time-error` turns that record into the path from the pipeline to the shard that failed. Everything else pays nothing: flows composed normally record no origins (`CompiledFlow::origins` is `None`, one word), and the engine's step is compiled twice (`steps::<LOCATE>`), so the runtime and successful evaluations run none of the recording. An earlier version kept origins in every flow; it cost the classic ESP32 1.5 KB of its lowest free heap and was replaced. Measured against `068f128` (release, aarch64, minimum of five or seven interleaved runs): `bench_instances`, `bench_depth` and `bench_tables` within ±4% in both directions across rounds (run-to-run spread); `bench_compose` within its baseline; the frontend memory fixtures' compose peaks 16 to 520 bytes higher (`examples/frontend_memory.rs`). Budget diagnostics (`expansion-budget`) stay located at the `#( )`.

## 3. M9: flow parameters

### 3.1 Semantics

- A parameter declared `Flow(input: T output: U)` takes a block, and `Run(action)` runs it inside the callee and outputs its result. The input is `Run`'s input, checked against `T`.
- A bare `Flow` takes a block whose output is ignored: `Run` passes its input through, like `Sub`. `Flow(input: T output: U)` is the form that returns a value.
- The block is **lexical to the caller**. It composes against the caller's scope at the call site and reads the caller's locals: `Retry(times: 3 { Http.Get(url) })` sees `url`. `Update` inside it writes the caller's variable. Its bindings are block-scoped like any other block.
- A flow **cannot escape the call**: it can't be bound with `Var`, `=` or `Keep`, returned, passed to a mesh variable or stored in a collection. It can only be run or passed on as a `Flow` argument to another call. So the caller's frame always outlives it, and there are no closure lifetimes. A violation is a located `flow-escapes` diagnostic.
- It may suspend. The caller's frame is suspended underneath the callee anyway, so no lifetime changes.
- **Effects are per call site:** the effects and mesh access of a call are the callee's plus those of the blocks passed at that site. `Retry` around a suspending block suspends; around a pure block it adds nothing. A `pure` function may take a `Flow` and stays pure only at sites whose blocks are pure.
- Stateful callees: a block passed to a `stateful` function is entered fresh each time it runs. A block cannot contain `Keep` (it is not a wire or function top level), a `Once` (`once-in-stateless`), or a call to a stateful function (`stateful-call-in-stateless`, 2026-10-10: the callee's instance survived between runs of one `Run` but not across two `Run`s); call it around the block instead.
- **Control inside a block:** a `Return` that would leave the block is a located `control-in-flow` error (it would have to unwind the callee). A `Return` inside a function the block calls is fine; that function consumes it. `Stop` and `Restart` are allowed: they target the root wire wherever they run, and already cross function frames (test J).

### 3.2 Implementation

**Both designs, with one semantics.**
- **Entering a caller-frame block defines the semantics:** a flow value is the caller's frame handle plus the block's compiled flow, and `Run` enters a frame that executes it against the caller's locals. One callee body for every site, which is what the body cache and reload selection already assume. Frames already carry a `parent` handle, so this is a small extension of how child flows run today.
- **Specialization is an optimization:** the existing inliner places a straight-line block inside an inline-eligible callee at that call site, recorded with `Dep::Inlined` like any inlined body. Without it, a callee containing `Run` is opaque to the VM paths at every site, and a `ForEach` written as a function would take an engine step and a locals switch per element.

Document how body caching, `Dep` recording and reload selection treat a function with flow parameters.

### 3.3 Native shards

In the same milestone, host crates get a supported way to run nested flows, which has been impossible since stackful was deleted:
- A native shard can declare a `Flow` parameter.
- A public continuation interface (`Control::Custom` plus a `ControlShard` trait: given its continuation state, the input and an optional completion, return `Enter(child)` or `Complete(result)`) lets it enter that flow and receive the result, on the same `Enter`/`Complete` channel as the core composites.
- Port one existing core composite through it to prove it is complete.
- `embedding.md` describes it as the host extension point for control flow. Its current sentence implying hosts can use the full `Shard` contract for control flow is corrected in this milestone.

### 3.4 Gate

- `Retry` and `Timed` written as `@fn` with flow parameters. `Retry` stops on the first success and, after the last attempt fails, fails with that attempt's error. Tested for: repeated execution, a block that suspends, a caller local updated from inside the block, cancellation while the block is suspended (every state cleaned once), and an error caught by a `Maybe` around the call.
- A native composite with a `Flow` parameter, defined in a crate other than `otter-core` (a test crate is fine), passes the same cases.
- The inlined and the framed paths behave the same: caller mutation, errors and cancellation, each tested on both.
- A benchmark of `ForEach` written as a function against the native loop. Record the ratio first; set a threshold afterwards.
- A `Return` leaving a block is `control-in-flow`; a `Return` inside a function the block calls, and `Stop` inside a block, work.
- Each escape attempt in §3.1 is a located `flow-escapes` error.
- Per-site effects: the same function is pure at one site and suspending at another, visible in `check --json`.
- Authoring eval: tasks using `Retry` and `Timed`, with no new error class among first-pass failures.

### 3.5 Implementation (2026-10-09)

- **Declaring.** A parameter's type `Flow` (any input; `Run` passes its input through), `Flow(input: T)` or `Flow(input: T output: U)` (`Run` outputs the block's output, which `U` must accept) makes it a flow parameter (`FunctionParam::flow`, `FlowType`). `input: None` means the block ignores its input, like a function's: `Run` accepts any input and passes `none`; `output: None` means its output is discarded and `Run` outputs `none` (added after the authoring eval, where a model declared it so for a block whose output it did not need). A flow parameter is not a local of the body; its name is reserved there (`duplicate-binding`), and reading it in any way is `flow-escapes`, which covers binding it, assigning it, returning it, storing it in a collection or a mesh variable, and passing it as a value.
- **Calling.** A call passes a block (`{...}`, or any pipeline, which becomes one) or, inside a function, one of its own flow parameters by name (`BlockArg::Forward`). The block composes at the call site against the caller's scope with the parameter's input type (`ComposeCtx::compose_block`), as a region that may run any number of times; its output is checked against `U`. Inside it, `Return` is `control-in-flow`, and `Once` and calls to stateful functions are refused (`ComposeCtx::in_flow_argument`); `Keep` already needs a top level. A native shard's flow argument naming a flow parameter becomes `{ Run(name) }` in lowering, so `When(cond action)` works. The call site's analysis includes its blocks', so effects and mesh access are per site, as `check --json` shows; the body's own analysis has none of them.
- **Running.** `Run` is a composite (`Control::Run`). At entry it resolves its block through the running invocation: the invocation frame's parent is its caller, stopped at the call, whose node holds the block (following `Forward`s up the callers), so calls store nothing for it. A block of leaf code runs right there, on the caller's locals; any other gets a frame (`Code::Block`, scoped to the caller's locals) that the Run node keeps for the next run of the same block. Native state inside it lives for one run, reset at exit like a stateless invocation's. Its frames, suspension, `Stop` and errors go through the engine like any child's, and cancellation cleans them up once. A function with flow parameters is never a VM call.
- **Inlining.** A body that is straight-line apart from its `Run`s (`CompiledFunction::block_leaf`) is inlined at a site whose blocks are straight-line too, under the usual limits (stateless, no `Keep`, no call site, at most `INLINE_BUDGET` instructions, plus copies of a block run more than once within the same budget): each `Run` is replaced by its block's code, which already addresses the caller's slots (`Flat::inline_blocks`). A `Run` without an output saves and restores its input around the block, unless the next instruction replaces the accumulator anyway. The blocks' nodes count as part of the inlined call (`Lowered::inlined`), and the callee is recorded with `Dep::Inlined` as before.
- **Caching and reload.** The body is composed once per input type, without its blocks, so it is shared by every site as before; the blocks are the caller's code and part of the caller's cache key and body. A caller whose blocks changed recomposes like any edited code; an edit to the callee follows §11 (`Dep::Function`, or `Dep::Inlined` when inlined). A rebased instance keeps a Run's cached block frame (it holds its call node), and the next run rebuilds it for the new node.
- **Locating failures.** A failure inside a block is located where the call wrote it: under the call's shard and its parameter, also through forwarding and inlining (`failure_path` goes back to the level that ran the call).
- **Native composites.** `ControlShard` (public, `shard.rs`) with `ShardType::controlled_by`: `resume(compiled, state, ctx, input, completion)` returns `ControlStep::Enter(flow, input)` or `Complete(value)`, sees each flow's output or error, and lets `Stop`, `Restart` and `Return` pass through; its `State` resets to its default whenever it completes. The engine sees it as `Control::Custom`, runs leaf flows in place, and enters the others as frames; what it carries stays out of the engine step (a host composite's child input waits in its state). `Maybe` is ported to it; `tests/lang.rs` defines `Test.Retry` with it. Its state costs every composite's continuation one word (56 to 64 bytes).
- **Authoring eval** ([results](../bench/authoring/results/2026-10-09-m9-tasks43-44/README.md)): tasks 43 (`Retry`) and 44 (`Timed`), three trials each of Haiku and Sonnet. Every reply writes the block parameter and `Run` as the primer shows; task 44 passed 6/6 on the first check, task 43 3/3 (Sonnet) and 1/3 (Haiku, whose other two replies retry correctly but leave the caught error logged). No first-pass failure is of a class M9 introduced; 12 of the 13 compose errors are a function declared `output: None` whose body ends on a value (the M5 rule), which suggested letting `output: None` discard a function's output as it does a block's; done 2026-10-10 (`CompiledFlow::discard_output`, one instruction appended when the body ends on a value).
- **Measured** (release, aarch64; ns per element, a sequence summed into a caller variable, `examples/bench_flow_params.rs`), against a `While` written in place (10.5 to 12.8 ns): a function with the length as a parameter, inlined with its block, 1.02 to 1.16× (the most at width 8); the same function counting the sequence itself (`Count` has no VM form, so the call keeps its frame and `Run` runs the block in place) 3.7 to 4.2× at widths 8 to 256. No threshold yet (§3.4). Against the parent commit, interleaved: the entity tick 89.4 against 91.1 ns per instance, `bench_depth` 34.2 to 36.6 against 35.0 to 36.9 ns, `bench_tables` within ±3 percent, every VM suite cell between −8.5 and +2.4 percent. `inline::run` is unchanged; the engine step (`Engine::activate`) is 5% larger (4,878 instructions, 336 stack loads and 263 stores, against 4,641, 313 and 245). Bisecting it found two versions that cost more and were replaced: one carried a host composite's child input through the step (5,181, 406 and 396), one carried the output of a block run in place (`bench_depth` 2 to 4 percent slower); both values now stay in the out-of-line helpers.

## 4. M10: code as data

### 4.1 Start condition

M10 starts only when M9 has left **three real cases that flow parameters cannot express**, recorded in this section with links. Expected kinds: generated declarations (records or functions from a schema), constructs that introduce bindings, structural rewrites. `Retry` does not count, M9 covers it. Without three cases, M10 waits.

### 4.2 Representation

`Code` is its own type, distinct from `Flow` (runnable behavior) and from `CompiledFlow` (internal, never exposed to scripts). It has a structured, inspectable representation: every node is a table with an explicit kind, so macros **read** code with ordinary `Take`, `Match` and sequence shards. **Producing** code is different: only the `Code.*` constructors create or modify nodes (for example `Code.Invoke`, `Code.Literal`, `Code.With(node field value)`), and they validate what they build.

```
Ident    opaque: name readable, lexical context hidden
Read     {kind: "read"    id: Int  name: Ident}
Invoke   {kind: "invoke"  id: Int  shard: Ident  args: [Code]  labels: {..}}
Bind     {kind: "bind"    id: Int  form: String  name: Ident}   ; =, Var, Update, Keep
Block    {kind: "block"   id: Int  items: [Code]}
Literal  {kind: "literal" id: Int  value: Any}
```

- `id` indexes the frontend's source map, so spans and provenance live outside the value. Generated nodes get fresh ids that record the macro definition and the call site.
- **An identifier is not a string, and its lexical context cannot be forged.** Scripts read `name` from an `Ident`; they create one only through `Quote` or `Code.Ident("tmp")`, which gives it the current expansion's context. There is no way to set or copy a context number.
- **A matching shape is not proof of code.** `Code` values come only from the parser, `Quote` and the `Code.*` constructors. A table that merely has the same keys is an ordinary table (`Table.Make` cannot produce `Code`), and data that looks like an invocation stays data inside a `Literal`.
- **Nominal identity must be recorded here.** Shapes are interned by their keys, so a reserved key list alone cannot tell a code node from a look-alike table. Choose the mechanism (for example a registry flag on code shapes that `Table.Make` and host builders never set, or a distinct `Var` variant), state it in this section, and test that a look-alike is rejected.
- Hosts and tools get an encode/decode pair for a versioned data form (JSON included). Decoding validates kinds, field types, child structure and identifiers, and reports structured diagnostics; it gives decoded identifiers user context.
- The parser's internal AST may stay as it is. What matters is that parsed source and built `Code` reach the same validated model.

### 4.3 Quote and splice

A macro earns its place by doing what a function cannot, such as introducing bindings in the caller. A destructuring macro binds each listed name to the field of the same name:

```shards
@macro(Fields params: {value: Code names: [Code]} {
  // for each name, a binding `= name` of `Take(name)` on the value
  ...
})

@Fields(point [x y])   // binds x and y in the caller
```

The body is left open on purpose: it is written against the real `Code.*` shards and run as a test when M10 lands, and the sketch here is replaced by that code. It exercises the rule that names arriving through `$` bind in the caller (§4.5). Any `Retry` written as an example must stop on the first success and say what happens after the last failure.

- `Quote({ ... })` yields the `Code` of its block, unevaluated.
- `$x` inside a quote is a splice, decided by `x`'s static type:
  - `Code`: inserted as one node.
  - `[Code]`: its items inserted where a sequence of items is expected (a block's statements, an argument list, a sequence literal).
  - anything else: inserted as a `Literal`: a `times` parameter holding `3` splices as the literal `3`.
- `$x` outside a quote is `splice-outside-quote`.
- `$` is a splice **only inside `Quote`**; it does not become a general sigil. M10 decides between contextual `$` splices that depend on quote depth and explicit `Unquote`/`Splice` forms, including how to quote a literal `$`-prefixed identifier. `$<digit>` is already an error (§1).

### 4.4 Macros

- `@macro(Name params: {...} {...})` declares a pure function whose output is `Code`. Parameters of type `Code` (or `[Code]`) receive their arguments **unevaluated**, as code. Other parameter types receive compose-time values, as in M8. This is why `@macro` is its own declaration: the frontend must know how to read the arguments before it composes anything.
- It is invoked only as `@Name(...)`. Calling it without `@` is `macro-call-at-runtime`.
- **Phases:** macro definitions compose before the code that uses them. A macro body may call pure functions and other macros, but may not depend on an expansion of itself (an acyclic phase graph; a cycle is `compose-time-cycle`, as in M8).
- Pipeline: parse, compose macro definitions, expand `@Name(...)` sites (repeatedly, outermost first, within the M8 budgets per expansion and a total expanded-size limit per compose, since nested expansions can multiply code size while each stays within its own budget), lower, then ordinary compose. Expanded code goes through every check hand-written code does: scope, definite initialization, types, effects.
- Caching and reload follow M8. A cached expansion still gets **fresh identifier contexts at every use**, so two sites expanding the same macro never share introduced names.

### 4.5 Hygiene

Hygiene is defined by binding behavior. The contract:
- Names introduced inside a `Quote` or by `Code.Ident` cannot capture or be captured by the caller's names.
- Names arriving through `$` keep the bindings they had where they were written.
- Function, shard and helper names in a macro resolve in the macro's definition context, not the call site's.
- Nested quotes and nested expansions keep all three properties.

Locals resolve by name and lexical context. Since 2.0 forbids shadowing, the remaining collisions (same name, same context) are already `duplicate-binding`. An expansion id is a fine implementation of contexts, but the contract above is what the tests pin.

### 4.6 Diagnostics and tooling

- `PathStep::Expansion { macro, call_site }`: an error in generated code shows the generated operation, the macro definition and the call site.
- `otter expand file.otr` prints the expanded program as readable source, renaming introduced names only where they would print ambiguously. `--json` adds lexical contexts and provenance, which printed names alone cannot show.

### 4.7 Gate

- **Vertical slice first:** quote a block, inspect it, change a literal, expand, compose and run the changed result.
- The three cases from §4.1, each as an acceptance test.
- Hygiene, one test per rule in §4.5: a macro-local `tmp` beside the caller's `tmp` composes clean; a spliced caller name binds to the caller's variable; a helper resolves in the definition context; a nested expansion keeps all three; two sites of one cached macro get distinct introduced names.
- The `Fields` macro of §4.3 binds in the caller, and its spliced value expression keeps its caller binding.
- A `Retry` macro and the M9 `Retry` function behave the same: same results, effects, error handling (a `Maybe` around each) and cleanup on cancellation. Both stop on the first success. Identical compiled code is not required, since M9 may share one callee body. This checks that expansion adds nothing; it is not evidence that macros are needed.
- A type error inside an expansion shows all three locations.
- A runaway expansion, and nested expansions past the total limit, end with `expansion-budget`.
- Decoding malformed code data, and a forged `Ident` or look-alike table, are rejected with structured diagnostics.
- Device: expansion happens during compose, so the instance runtime gains nothing. The frontend suite with the macro tests still fits the classic ESP32's heap (device-sized fixtures where needed).

## 5. Not in this plan

- **Runtime `eval`.** No language shard composes code while a program runs. Hosts can compose from decoded `Code` through the existing APIs and reload admission. A `Mesh.Spawn(code)` shard is a separate decision for later.
- **Escaping closures.** Flows that outlive their call (stored, returned, sent through the mesh).
- Models keep writing text. The baseline had zero parse errors, so emitting code as data gains them nothing.
- Unhygienic escape hatches, reader macros, user-defined syntax, splicing into identifier positions.
