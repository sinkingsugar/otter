# Iteration and access (A4)

**Status:** decided 2026-10-23 (the maintainer approved the scope; the binding syntax was delegated and is decided here). This is the contract for Track A4 in [host-requirements.md §7](host-requirements.md#7-priorities-2026-10-11) and H12's iteration item. It overrides the "a table's entries arrive as `{key value}`" and `Seq.Enumerate` statements in `metaprogramming.md` §1 and `current-state.md`.

## 0. Why

Scripts today have no loop over a collection and no convenient read of nested data. The tool examples show the cost (`examples/tools/flights/search-flights.otr`, `examples/tools/notes-search/search-notes.otr`):

```
0 | Var(i)
Repeat({
  rows | Take(i) = row
  Inc(i)
  row | At(i: 0) | At(i: 9) | IntOr(default: 0) = total   // At, IntOr: hand-written helpers
} times: (rows | Count))
```

- There is no `ForEach`, `Map`, `Filter` or `Reduce` (and `Seq.Enumerate`, named in the docs, does not exist).
- A path works only on a variable (`row.title`); the pipeline value needs `| Take("x") | Take("u")`.
- `Take` refuses `Any` at compose, so every step into parsed JSON needs `ExpectTable`/`ExpectSeq` between the `Take`s, and a path through a union (`Int | {c: Int}`) fails.
- There is no way to bind several fields at once.

After this milestone the same loop reads:

```
rows | ForEach(row i {
  row.0.9? | IntOr(default: 0) = total
})
```

Four parts, landed in this order (each is useful alone and has its own tests): §1 paths on the pipeline value, §2 dynamic paths, §3 destructuring, §4 iteration.

## 1. Paths on the pipeline value

- A pipeline step may be a bare path: `.key`, `.0`, and chains `.a.b.1`. It reads from the step's input: `x | .a.b` is `x | Take("a") | Take("b")`. It is allowed wherever a shard may stand in a flow: after `|`, as a statement, as a flow's first step (`Map({.name})`), in a predicate (`Filter({.age | IsMore(17)})`).
- Segments are a name (the `Ident`/`Upper` forms a variable path accepts) or a non-negative integer, with no spaces, exactly as in a variable path (`t.a.1`). It lowers to the same `Take` chain (`take_path`).
- `.5` in a value position (an argument, a sequence or table element) stays the existing `number-form` error with its `0.5` fix-it. In step position it is a path. A diagnostic for `.5` used as a step where a float was clearly meant is not needed: the types catch it.
- No path in argument position (`Foo(x: .a)`): an argument has no pipeline input. That is a parse error with help ("a path step reads the pipeline input; bind it first or use a variable path").

## 2. Dynamic paths

`Take` (and so every path, variable or step) composes on input that may be a collection:

- **Static types keep today's rules and code.** A Seq, a table (fixed or open) or a vector input composes exactly as now, with its fast path; a fixed table's unknown key is still a compose error.
- **`Any` input**, or **a union with at least one member `Take` applies to**, composes to a run-time check. The output type is `Any` for `Any`, and for a union the union of what each applicable member gives (a key no member can have is a compose error; a union with no applicable member is the existing `input-type-mismatch`). `1 | Take(0)` stays a compose error.
- **At run time** a value of the wrong kind fails with an error naming the segment and what arrived: `Take: .a needs a table, got Int`. When the step comes from a path, the diagnostic carries the whole path (`j.data.a`). A missing key on a table gives `none` (the rule open tables already have); an index out of range fails (as now).
- **Lenient paths:** a `?` right after a path (variable or step) makes the whole path lenient: `j.data.3.0?`, `x | .a.b?`. Any segment that cannot apply (wrong kind, `none`, missing key, index out of range) makes the path output `none`, and the rest is skipped. The output type is the strict path's type `| None`. `?` is accepted only directly after a path, with no space; elsewhere it is the existing `unexpected-character` error. This replaces the hand-written `At` helper; `| IntOr(default: 0)`-style defaults stay script functions for now (a core `Or`/default shard is a possible follow-up, not in this milestone).
- **Lowering:** a lenient path is one shard, not a chain, so the skip needs no control flow: `Take` gains a hidden form taking the whole key list (`keys:` plus `lenient: true`, or a separate internal `TakePath` shard; the implementer picks, but it must not appear in the catalog as a second public way to write a path). A strict multi-segment path may lower to the same form when that is no slower.
- **Performance:** the static paths do not change (instruction gate and hot-asm identical for existing workloads). The dynamic check is a warm helper or `slow_op` work, never in `inline::run`'s fast cases. Add gate workloads for a dynamic path and a lenient path.

## 3. Destructuring

- `= {a b}` binds the immutable names `a` and `b` to the input's fields `a` and `b`. `= {a: x b: y}` binds `x` and `y` to the fields `a` and `b`; both forms mix (`= {title hit: snippet}`).
- `= [a b c]` binds the first three elements of a sequence by position; `_` skips one (`= [_ code _ name]`). A shorter sequence fails at run time (`= [a b c]: needs 3 elements, got 2`); elements past the pattern are ignored.
- Patterns are flat: no nesting (`= {a: [x y]}` is a parse error with help to bind in two steps). Only `=` takes a pattern; `Var`, `Update`, `Push` and `Keep` do not.
- Every name follows the existing binding rules (no shadowing, `duplicate-binding`, definite assignment). The field reads follow §2: a fixed table's unknown key is a compose error, an `Any` or open table gives `none` for a missing key, and a value that is not a table (or not a sequence) fails at run time naming the pattern.
- `=` with a pattern passes its input through, as `= x` does.
- `_` is a new token, valid only inside a `[...]` pattern and as a loop name (§4); elsewhere it stays an error.
- Lowering: bind the input to a temporary once, then one read per name (`take_path` on the temporary), so it costs what writing it out costs.

## 4. Iteration

### 4.1 The forms

```
seq | ForEach(x {body})               // x: the element
seq | ForEach(x i {body})             // i: the index (Int, from 0)
tbl | ForEach(v k {body})             // v: the value, k: the key (String), in key order
seq | Map(x {body})                   // a sequence of the body's outputs
tbl | Map(v k {body})                 // a table with the same keys, values the body's outputs
seq | Filter(x {predicate})           // the elements for which the predicate outputs true
tbl | Filter(v k {predicate})         // the entries for which it does
seq | Reduce(acc x {body} init: 0)    // fold; the body's output is the next acc
seq | Reduce(acc x i {body} init: 0)
```

- **Names are positional, value first, then the position**: the element (or a table's value), then the index (or a table's key), as JavaScript's `forEach((value, index))` and `Map.forEach((value, key))`. For `Reduce` the accumulator comes first (`reduce((acc, value, index))`). One rule for sequences and tables, no flag changes it.
- **Names are optional**: `ForEach({body})` binds nothing. `_` skips a name: `ForEach(_ i {...})`. More names than the form takes is a compose error.
- **The body's input is the element** (the value, for a table), whether or not it is named, so `Map({.name})` and `rows | ForEach({= {title hit} ...})` work with §1 and §3. Naming is for nested loops and for reading the element after the pipeline has moved on.
- Names are immutable, block-local bindings of the body, declared per iteration; the no-shadowing rule applies (a nested loop must pick another name).
- `ForEach` passes its input through. `Map`'s output is `[T]` for the body's output `T` (a table input gives an open table of `T`). `Filter`'s output has its input's type; its predicate must output `Bool` (compose error otherwise, as `When`'s). `Reduce` needs `init:` or uses the first element (then an empty sequence fails at run time and the accumulator's type is the element type); with `init:` the accumulator's type is `init`'s and the body's output must be accepted by it; the output is the last accumulator.
- **Input types:** a sequence, a table (fixed or open; a fixed table's value type is the union of its field types), or `Any`/a union containing a collection (§2's rule: checked at run time, element `Any` or the union). A `String` is not iterable in this milestone (`String.CodePoints` or `String.Split` first).
- **The collection is read once**, before the first iteration: the loop iterates that value, so mutating the variable it came from inside the body (`Push`, `Update`) does not change the iteration (copy-on-write; no extra copy when the body does not mutate).
- **Control:** a `Stop`, `Restart` or `Return` in the body propagates, as in `Repeat`. There is no `Break`/`Continue` in this milestone (a `While` or a `Return` from a function covers early exit); note it as a follow-up if the ports want it.
- **Suspension:** the body may suspend (`Pause`, `Http.Get`, `Wait`); the loop resumes at the same element, as `Repeat` does.
- `$0` stays the existing lexer error, its help now pointing at `ForEach(x {...})`.

### 4.2 Implementation

- Core control shards with `ShardDesc`s in the catalog (described, searchable, with examples in their help), composing their body with the names declared through `ComposeCtx::declare_local` in a block scope.
- **Flattened into the VM like `Repeat`** when the body is VM-eligible: a counted loop over the snapshot's length with the element loaded into the bound slot (and the block's input) each iteration; `Map` and `Filter` push into an output under construction; tables iterate their sorted entries by index. Otherwise the engine path, with the continuation in `State` reset on every exit other than `Suspend`.
- The frontend parses names before the flow argument as declarations (like `Var(i)`), not as reads: a bare name or `_` in those positions of these four shards.
- Performance: add gate workloads (`ForEach` sum, `Map` over 256 Ints, `Filter`, `Reduce`, a table `ForEach`) and compare with the hand-written `Repeat`+`Take` loop; target parity with it. Existing workloads unchanged.

## 5. Acceptance

- Tests in `otter-lang/tests/lang.rs` (frontend: paths, `?`, patterns, names, diagnostics with spans) and `tests/prototype.rs` (core shards, suspension inside a body, nested loops, mutation during iteration, errors and cleanup).
- Device: the core and frontend suites still pass on the ESP32 firmware (device-sized fixtures where needed).
- Ports: `search-flights.otr`, `cheap-dates.otr`, `flights.otr` (drop `At`, `Last` where paths cover them) and `search-notes.otr` rewritten with `ForEach`/`Map`, paths and patterns; their offline tests pass unchanged.
- Docs: `current-state.md` (Next, Implemented, Deviations: no `$0`, the value-first order, lenient paths), `metaprogramming.md` §1 (no `Seq.Enumerate`; table entries as `v k` names), the primer/authoring material if it shows loops, and `host-requirements.md` H12 (decided).
- The full check set in `AGENTS.md`, the instruction gate and hot-asm (no change for existing workloads, or the difference explained and approved).

## 6. Deviations from 1.x

- 1.x's `ForEach`/`Map` exposed the element as `$0` (and a table's key and value as `$0`/`$1` of a pair); 2.0 names them explicitly, value first, and the body's input is the element.
- 1.x's `Take` on a table took a key, on a sequence an index, and a `Take` on `Any` was checked only at run time with no compose-time help; 2.0 keeps static checks for static types and checks at run time only for `Any` and unions, with the path in the error, and adds lenient `?` paths (1.x had none).
