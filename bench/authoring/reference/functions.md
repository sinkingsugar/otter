# Otter language reference (functions syntax)

A program is a pipeline of **shards**. Each shard receives the previous shard's output as its input and produces an output. `|` separates shards and is optional: `1 | Log` and `1 Log` mean the same.

```shards
// Line comments start with //.
7 | Math.Add(5) | Math.Multiply(3) | Log   // logs 36
```

## Values

- `Int` (`42`, `-3`), `Float` (`1.5`, `2.0`, `1e3`; write `1.0`, never `1.`), `Bool` (`true`, `false`), `String` (`"text"`), `none`.
- Sequences: `[1 2 3]`. Commas are optional.
- Tables: `{name: "ann" age: 41}`. Reading a missing key with a constant key (`t.missing`, `Take("missing")`) is a compile error. `Take` with a key in a variable outputs the value or `none` (`Int | None` here), so narrow it with `ExpectInt` and friends before arithmetic.
- Float vectors: `[1.0 2.0] | ToFloat2`, `[1.0 2.0 3.0] | ToFloat3`.
- A literal on its own is a shard that outputs that value.
- Arithmetic on two Ints gives an Int, and integer division truncates (`7 | Math.Divide(2)` is `3`); an Int mixed with a Float gives a Float. Convert with `ToFloat` and `ToInt`.
- `f"{name} is {age | Math.Add(1)}"` builds a string; each `{...}` holds a pipeline. Plain `"..."` strings keep braces literal.

## Shards and parameters

Shard names start with an uppercase letter. Parameters go in parentheses, positionally or by name, and labels are lowercase: `Repeat({...} times: 3)`, `Log("label")`. A parameter can be a literal, a variable, or a **flow** in braces `{...}`, as each shard's catalog entry lists under `forms`. A flow receives the shard's input unless the shard says otherwise. Some shards have short aliases (`Add` for `Math.Add`); the catalog lists them.

## Variables

Variable names are lowercase and may contain `-` (`max-speed`), so `a-1` is a name, not a subtraction.

| Write | Meaning |
|---|---|
| `value = x` | declare an immutable variable `x` |
| `value \| Var(x)` | declare a mutable variable `x` |
| `value \| Update(x)` | assign to an existing mutable variable |
| `value \| Push(xs)` | append `value` as one element to the mutable sequence `xs` (declare it first: `[] \| Var(xs)`) |
| `Keep(x 0)` | declare persistent mutable state `x` with a literal initial value, kept across iterations (only at the top level of a wire or a stateful function) |
| `x` | read a variable: outputs its value |
| `x.key`, `xs.0`, `t.a.1` | read a table key or sequence index |
| `\| .key`, `\| .0.name` | the same on the pipeline value: `rows \| Take(0) \| .name` |
| `j.data.3?`, `\| .a.b?` | a lenient path: `none` when a step cannot apply (wrong kind, missing key, index out of range); for parsed JSON (`Any`) |
| `value = {a b: x}` | bind several names from a table's fields (`a`, and `x` from `b`); the value passes through |
| `value = [first _ third]` | bind a sequence's elements by position; `_` skips one |

Assignments pass their input through. A name cannot be declared twice while visible (no shadowing); a variable declared inside a flow is visible only inside it. `input` is reserved: it names the entry value of the enclosing function or wire.

```shards
0 | Var(total)
[3 8 1] = xs
xs | ForEach(x { x | Math.Add(total) | Update(total) })
total | Log   // logs 12
```

## Control flow

- `If({predicate} {then} {else})`, or labeled: `If(predicate: {...} then: {...} else: {...})`. The predicate flow outputs a Bool.
- `When({predicate} {action})`, `While({predicate} {action})`, `Repeat({action} times: n)`, `Repeat({action} until: {condition})`, `Repeat({action} forever: true)`.
- Iteration over a sequence or a table: `xs | ForEach(x i {...})` (the names come first, value then index, both optional; a table gives value then key), `xs | Map(x {x | Math.Multiply(2)})`, `xs | Filter({IsMore(2)})`, `xs | Reduce(acc x {x | Math.Add(acc)} init: 0)`. The body's input is the element, so `rows | Map({.name})` works; `_` skips a name (`ForEach(_ i {...})`). There is no `$0`.
- `All(a b)` and `Any(a b)` combine conditions (Bool variables, literals or flows), with short-circuit; `Not` negates a Bool.
- `Match([1 {"one"} 2 {"two"}] default: {"other"})`: runs the first case equal to the input. `Match` must cover every value of its input type, or have `default:`; `default: {}` outputs the input.
- `Maybe({action} {fallback} silent: true)`: runs the fallback flow if the action fails.
- `Once({...})` runs its flow only on the first activation of the wire or stateful function instance.
- `Stop` ends the current wire instance. `Return` ends the enclosing function (or wire iteration) with its input as the output.

## Functions

A function is declared at the top level with its signature and called like a shard, by name, with its parameters as labeled arguments. It sees only its input, its parameters and what it declares; it never sees the caller's variables.

```shards
@fn(Scale input: Float output: Float params: {factor: Float} {
  Math.Multiply(factor)
})
3.0 | Scale(factor: 2.0) | Log   // logs 6
```

- `input:` and `output:` are types (`Int`, `Float`, `String`, `Bool`, `None`, `Any`, `[Int]`, `{x: Int y: Int}`, `Int | None`). `input: None` means the function ignores its input; `output: None` means it outputs nothing, whatever its body ends on. `params: {}` declares no parameters; a literal instead of a type (`params: {step: 1}`) is a default.
- Inside the body, a parameter is an immutable variable of the same name, and `input` is the value the call received.
- A function starts with fresh variables on every call. `stateful: true` makes each call site keep its own instance, so `Keep` and `Once` work inside: `@fn(Counter stateful: true input: None output: Int params: {step: Int} { Keep(n 0) n | Math.Add(step) | Update(n) })` logs 1, 2, 3 when called repeatedly from the same place.
- Mesh variables (declared by the host) are visible in a function only when listed in `uses: [name]` (reads) and `mutates: [name]` (writes).
- A function may call itself, directly or through other functions (a stateful one may not); the depth of nested calls is limited by the host.

## Blocks as parameters

A parameter declared `Flow` takes a block `{...}`, which the function runs with `Run(name)`, as often as it likes. The block belongs to the caller: it reads and updates the caller's variables.

```shards
@fn(Twice input: None output: None params: {action: Flow(input: None)} {
  Run(action) Run(action)
})
0 | Var(n)
Twice(action: { Inc(n) })
n | Log   // logs 2
```

- `Flow(input: T output: U)`: `Run(action)` gives the block its input (of type `T`; with `input: None` the block receives nothing) and outputs the block's output (of type `U`; with `output: None` the block's output is discarded and `Run` outputs nothing). Without `output:`, `Run` outputs its own input.
- If the block fails, `Run` fails with the same error; `Maybe` around `Run` (or around the call) catches it, and logs what it caught unless given `silent: true`. The block may `Pause`. A block cannot call a `stateful` function: it starts fresh on every run.
- A block cannot `Return` (it would end the function that runs it), and a flow parameter can only be run or passed on to another call (`Other(action: action)`), never stored in a variable.

## Compose-time values

`#( ... )` runs a pipeline once, while the program is checked, and stands for its result as if the literal had been written by hand. It can go wherever a literal can, including parameters that take only literals.

```shards
@fn(Square input: Int output: Int params: {} { = x  x | Math.Multiply(x) })
@const(size #( 4 | Square ))                 // a named constant, read as @size
#( 3 | Square ) | Log                        // logs 9
@size | Log("size")                          // logs size: 16
```

- The pipeline starts with no input and may use literals, constants and functions, but not runtime variables, `Log`, `Keep`, `Pause`, `Time.Now` or other shards with side effects.
- `@const(name value)` takes a literal or a `#( ... )`.

## Wires, meshes and scheduling

Without `@run`, code at the top level runs once, as the wire `root`. With `@run`, the top level only declares: put code in wires and schedule them. Named wires are declared with `@wire` and spawned (`Spawn`) or scheduled on a mesh.

```shards
@wire(ticker {
  Keep(n 0)                                  // state kept across iterations
  Inc(n) | Log
} looped: true)                              // one iteration per tick
@mesh(main)
@schedule(main ticker)
@run(main iterations: 3)                     // run the mesh for 3 ticks (logs 1, 2, 3); fps: 60 sets the rate
```

- `Spawn(wire)` starts a new independent instance of a wire with the current input as its input.
- A looped wire yields to the scheduler after each iteration, so it runs one iteration per tick. `Pause` suspends until the next tick, so a `Pause` inside a looped wire makes that iteration take an extra tick. `Pause(0.5)` waits at least half a second.
- `@run` runs until every scheduled wire finishes, or for `iterations` ticks.

## Running

`otter check file.otr` reports problems with their location; `otter run file.otr` runs the program. `Log` writes its input as one line (`Log("label")` writes `label: value`). After the log, `otter run` prints each top-level or scheduled wire's result (`root: 36`); these result lines are not part of a task's expected output.
