# Shards language reference (current syntax)

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

Shard names start with an uppercase letter. Parameters go in parentheses, positionally or by name: `Repeat({...} Times: 3)`, `Log("label")`. A parameter can be a literal, a variable, or a **flow** in braces `{...}`, as each shard's catalog entry lists under `forms`. A flow receives the shard's input unless the shard says otherwise. Some shards have short aliases (`Add` for `Math.Add`); the catalog lists them.

## Variables

Variable names are lowercase and may contain `-` (`max-speed`), so `a-1` is a name, not a subtraction.

| Write | Meaning |
|---|---|
| `value = x` | declare an immutable variable `x` |
| `value >= x` | declare a mutable variable `x` |
| `value > x` | assign to an existing mutable variable |
| `value >> xs` | append `value` as one element to the mutable sequence `xs`, declaring it if it does not exist (same as `Push(xs)`) |
| `x` | read a variable: outputs its value |
| `x.key`, `xs.0`, `t.a.1` | read a table key or sequence index |

Assignments pass their input through. A variable declared inside a flow is visible after it only when every path declares it.

```shards
0 >= total
[3 8 1] = xs
0 >= i
Repeat({ xs | Take(i) | Math.Add(total) > total  Inc(i) } Times: 3)
total | Log   // logs 12
```

## Control flow

- `If({predicate} {then} {else})`, or labeled: `If(Predicate: {...} Then: {...} Else: {...})`. The predicate flow outputs a Bool.
- `When({predicate} {action})`, `While({predicate} {action})`, `Repeat({action} Times: n)`, `Repeat({action} Until: {condition})`, `Repeat({action} Forever: true)`.
- `All(a b)` and `Any(a b)` combine conditions (Bool variables, literals or flows), with short-circuit; `Not` negates a Bool.
- `Match([1 {"one"} 2 {"two"} none {"other"}] Passthrough: false)`: runs the first case equal to the input; a `none` case matches anything.
- `Maybe({action} {fallback} Silent: true)`: runs the fallback flow if the action fails.
- `Once({...})` runs its flow only on the first activation of the wire instance.
- `Stop` ends the current wire instance.

## Wires, meshes and scheduling

Without `@run`, code at the top level runs once, as the wire `root`. With `@run`, the top level only declares: put code in wires and schedule them. Named wires are declared with `@wire` and run inline (`Do`), spawned (`Spawn`) or scheduled on a mesh.

```shards
@wire(helper { x | Math.Add(1) > x })        // a named wire
@wire(ticker {
  Once({0 >= n})                             // state kept across iterations
  Inc(n) | Log
} Looped: true)                              // one iteration per tick
@mesh(main)
@schedule(main ticker)
@run(main Iterations: 3)                     // run the mesh for 3 ticks (logs 1, 2, 3); FPS: 60 sets the rate
```

- `Do(wire)` runs another wire inline, sharing the caller's variables, and outputs the wire's output.
- `Spawn(wire)` starts a new independent instance of a wire with the current input as its input.
- A looped wire yields to the scheduler after each iteration, so it runs one iteration per tick. `Pause` suspends until the next tick, so a `Pause` inside a looped wire makes that iteration take an extra tick. `Pause(0.5)` waits at least half a second.
- `@run` runs until every scheduled wire finishes, or for `Iterations` ticks.

## Running

`otter check file.shs` reports problems with their location; `otter run file.shs` runs the program. `Log` writes its input as one line (`Log("label")` writes `label: value`). After the log, `otter run` prints each top-level or scheduled wire's result (`root: 36`); these result lines are not part of a task's expected output.
