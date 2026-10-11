# Embedding Otter and Writing Host Shards

How a Rust program (the host) adds its own shards and runs Shards scripts on them. The complete example, a host crate with a leaf shard, a blocking shard, a catalog and a script, is [`crates/otter-cli/tests/embedding.rs`](../crates/otter-cli/tests/embedding.rs). It is a test, so it stays correct; copy from it.

## 1. Crates

- `otter-core`: values, types, the shard traits, compose, and the scheduler (`Mesh`).
- `otter-lang`: source to wire definitions, `check` and `run`.
- `otter-io` (native only): the shared Tokio runtime, `spawn` for async work and `spawn_blocking` for blocking work.

Depend on them by path or git. Host shards live in the host's own crate, and nothing in this repository needs to know about them.

**Build with the same flags.** Native builds here align branch targets (`.cargo/config.toml`: `-C llvm-args=-align-all-nofallthru-blocks=4` for aarch64 and x86_64), and the runtime's measured speed assumes it: without it the VM's speed depends on where unrelated code lands, up to 40 percent either way ([runtime overview](runtime-performance-overview.md), lesson 7). A host's own workspace does not inherit that file; copy the `[target...]` entry into the host's `.cargo/config.toml`. A release build of `otter-core` without it prints a warning (shown for path and workspace dependencies).

## 2. A host shard

Every shard has one static description (`ShardDesc`). Its name, version, help, parameters, input and output come from there, and the decoder, catalog and documentation all read it. Prefix host shard names with a namespace (`Host.Reading`) so they cannot collide with core names; `Catalog::new` rejects duplicates, aliases included.

Declare `effects: Effects` and `lifetime: Lifetime` from `otter_core::signature` in the description. Effects describe potential suspension, I/O, time, randomness and unknown behavior; use `Effects::NONE` only for operations with none of these. `ShardDesc::undocumented` defaults to unknown effects and lifetime. This is a trusted host contract. Stateless describes semantic lifetime: temporary state retained during a suspended operation does not make a shard stateful; state remembered across invocations does.

Compose records mesh reads through `ComposeCtx::read_var` (also used by `Operand`) and writes through `mark_initialized`. A read-modify-write must use both. `var` is only a binding lookup and records no read. Use the compose helpers for child flows and wires so their effects and mesh access are included. `CompiledWire::signature()` exposes exact composed types and inferred mesh access; `ShardDesc::signature()` exposes native metadata through the same owned-or-borrowed view.

Choose the trait by what the shard does:

| The shard | Implement | Notes |
|---|---|---|
| Returns at once (reads, computes, converts) | `LeafShard` | "Leaf" means it never suspends, not that it does little work. |
| Does slow or blocking work (a long scan, a blocking library call, human-paced input) | `AsyncShard`, with `otter_io::runtime::spawn_blocking` | The work runs on the blocking pool. Only the waiting instance suspends; other wires keep running. |
| Waits on async I/O | `AsyncShard`, with `otter_io::runtime::spawn` | Race every await against the cancellation token. |
| Runs nested flows: a retry, a timeout, a lock, a UI container | `ControlShard` | Its flows are parameters; the engine runs them, suspensions included. See below. |

### Control flow

A shard that runs flows the script passes it implements `ControlShard` and is attached with `ShardType::new(desc).controlled_by::<S>()`. Declare each flow parameter with `Forms::FLOW` and compose it in `compose` with `ctx.compose_flow(flow, input_type)` (or `compose_flow_conditional` for a flow that may not run, so what it assigns is not definitely assigned after the shard): the flow composes against the caller's scope, reads and writes the caller's variables, and its effects become the shard's at that site. `flows(compiled)` returns the composed flows in a fixed order.

The engine runs the flows; the shard decides. `resume(compiled, state, ctx, input, completion)` is called when the shard is reached (`completion` is `None`) and again with each flow's outcome, `Ok(output)` or the error it failed with. It returns `ControlStep::Enter(index, input)` to run flow `index` with that input, or `ControlStep::Complete(value)` to finish; returning an error fails the shard, and returning an error it received propagates it. A flow may suspend: the instance yields, and `resume` is called again only when the flow finishes. A `Stop`, `Restart` or `Return` inside a flow propagates without calling `resume`. `State` (a `Default` type) holds the progress of one activation, such as an attempt count or a start time; the engine sets it back to its default whenever the shard completes or a signal passes through, and drops it on cleanup, cancellation while a flow is suspended included. So every exit (success, error, `Stop`, `Return` passing through, cancellation) drops what it holds, and it may hold a guard: a value whose `Drop` releases a resource or schedules its release (rolls back a transaction through the database service, dismisses a dialog through the host) and never blocks. `Test.Guarded` in `crates/otter-lang/tests/lang.rs` checks each exit.

The core's `Maybe` is written this way (`shards/control.rs`), as is the `Test.Retry` example in `crates/otter-lang/tests/lang.rs`. The full `Shard` contract's `control` hook remains the core's own.

A script can also write control flow itself: a function parameter declared `Flow(input: T output: U)` takes a block, which the body runs with `Run` ([metaprogramming.md §3](metaprogramming.md#3-m9-flow-parameters)).

### Parameters

Declare parameters with `ParamDecl::new(name, help, forms, types, requirement)`: accepted forms (`Forms::LITERAL`, `VARIABLE`, `FLOW`, ...), types (`TypeName`s) and requirement (`Required`, `Optional`, `Default(...)`, `Variadic`). When a `TypeName` list cannot say the type (`[Int]`, `Float4 | None`, a table with given keys), declare the full type instead: `ParamDecl::new_typed(name, help, forms, requirement, || Type::seq(Type::int()))`. It needs no `TypeName` list; the catalog derives the type code. The decoder enforces forms, requirements and literal types before compose runs.

For a value that can be a literal or a variable (a `Target: pid` the script sets at runtime), use `Operand::compose_arg` in compose. It resolves the variable, reports unknown or possibly-uninitialized ones, and checks the variable's type against the declaration (the full type, or the type list), all as located errors. Then call `Operand::get(ctx)` at activation. For a parameter declared `Requirement::Optional`, use `Operand::compose_optional_arg`, which returns `None` when the script did not give it; `compose_arg` is for required and defaulted parameters.

### Compose

Compose runs once per wire shape, and its result is shared by every instance. It must be a pure function of the arguments, the input type and what it reads through `ComposeCtx`:

- **Never touch the host in compose** (process memory, files, devices, the clock). Do that at activation.
- **Return precise output types.** A fixed record is `Type::fixed_table([...])`, so `r.key` composes to that field's type and a typo is a compose error with suggestions. A value that may be unknown is `Type::union([T, Type::none()])`; at runtime it is an explicit `none`, never a guess. Every key of a fixed table is present at runtime.
- **Input types are enforced from the description.** `InputDesc::Types(&[...])` is checked before your compose runs, with a located `input-type-mismatch`; do not repeat it. When a type list cannot say the input (a record with given keys, `[Int]`), declare the full type with `InputDesc::Typed(record_type)`, a `fn() -> Type`, and a record missing a key fails `check` instead of failing at runtime. `InputDesc::Any` leaves the input to your compose. A shard that produces a value without using its input declares `InputDesc::Ignored` and accepts any input type, so it can start a statement anywhere.
- **Produced values are checked against the compose output type** in debug builds (and in release with otter-core's `output-checks` feature): a leaf or async shard whose value drifts from the type it declared, for example a missing table key, fails the instance with a message naming the shard.
- **Report problems as structured diagnostics** (`Error::Diagnostic(Diagnostic::new(Phase::Compose, kind, code, message).shard(..).param(..))`). The frontend adds file, line, column and the occurrence path.

### Activation

- **Leaf shards** return `Flow::Next(value)`. Runtime failures are `Error::Activation(message)`. `Maybe` can catch them; otherwise they end the instance.
- **Async shards** start one operation per activation from owned inputs: no borrows of frames or of `ctx`. The adapter polls it on later ticks.
- **Blocking work cannot be interrupted from outside.** `spawn_blocking` passes a cancellation token: check it at safe points or wire it to the library's own cancel hook. Cancelling the instance cancels the token. Never leave external state half-changed when stopping early.
- **Read and build values through the accessors**, not the storage: `Var::as_str`, `Var::as_seq` (a `&[Var]`) and `Var::as_table` give borrowed views; a `Table` has `get`, `len`, `contains_key` and `iter`/`keys`/`values` in sorted key order. Build tables with `Table::builder().with("key", value).build()`, `collect()` from `(key, Var)` pairs, or `Var::table([...])`; derive a changed table with `table.into_builder()`, which takes the storage when it is not shared. These build map tables (sorted entries). A fixed table type carries an interned key `Shape`; compose turns literals into struct tables of their shape and reads literal keys by slot, and a host that returns one record shape many times can do the same with `Table::with_shape(shape, values)` (values in the shape's sorted key order) or convert a built value once with `into_struct`. Do not intern a shape per value with unbounded key sets: shapes live in the type registry, which never frees ([values-and-types.md](values-and-types.md) §2.1). Strings, sequences and bytes are `Var::string("..")`, `Var::seq(vec)` and `Var::bytes(&data)`, or `Var::from_string`, `from_seq`, `from_bytes` and `from_table` for an `Arc` or `Table` already held; match them by reference and take storage out with `into_seq` and `into_table` ([values-and-types.md](values-and-types.md) §2). Float vectors are `Var::float2(x, y)`, `float3`, `float4` (f32 components, wrapped in `Float2`/`Float3`/`Float4`, which deref to the array).
- **Keep per-instance state in `State`**, not in `Compiled`. Resources a script holds (a connection, a session) are objects; what owns and pools them is a service (below).

### Objects and services

A host resource a script holds is an **object** ([`otter_core::object`], docs/host-requirements.md H1). Implement `Object` for the Rust type, naming a static `ObjectType` (`Module.Name`, e.g. `Db.Connection`), and list the type in the crate's catalog next to its shards: `Catalog::new(&[CATALOG]).and_then(|c| c.with_objects(&[OBJECTS]))`. Scripts then write the name in signatures (`@fn(Query input: Db.Connection ...)`) and compose checks it; a shard declares it with `TypeName::Object(&CONNECTION)` or `Type::object`. Return one with `Var::object(value)`; read it with `input.as_object::<T>()`, or keep its `ObjectRef` (`input.object_ref()`, an `Arc`). Objects compare, hash and print (`Db.Connection#4`) by identity. They have no literal: a constant holding one is `object-in-constant`, so an object never lands in compose output. Dropping the last value holding one drops it, wherever it was held (locals, mesh variables, sequences, tables). Objects are `Send + Sync`; one with thread affinity asserts its thread on use.

A **service** owns and pools what objects refer to: the database behind connections, a model cache, a client pool ([`otter_core::service`], H3). The host inserts it before running, `mesh.services_mut().insert(db)` or, with a session, `session.set_services(services)` (every revision's mesh gets them, through either reload). A shard looks it up at activation, `ctx.services().get::<Db>()`, never in compose. When the host inserts none, the module falls back to a default of its own (process-wide, built once); nothing inserts into the map while a mesh runs.

**Cancellation is not quiescence** (H6). Cancelling an instance drops its pending future and fires its token, but a worker running blocking code finishes later. So:
- an operation owns `Arc`s of every resource it touches (`object_ref().clone()`, `services().get_arc::<T>()`), taken in `start` before anything is spawned: releasing the instance then cannot free a resource still in use;
- a resource with exclusive use (a chat context, a transaction) is leased from its service and returned when the worker finishes, not when the instance is cancelled;
- a long native call gets a cancellation bridge where the library has one (llama.cpp's abort callback, `sqlite3_interrupt`), wired to the token.

`crates/otter-lang/tests/lang.rs` (`host_session`) opens a session object through a service, uses it from scripts, and checks it is released on success, error, `Stop`, `Return`, between loop iterations and on cancellation.

## 3. The catalog and scripts

Build one catalog from the core list and the host's list:

```rust
Catalog::new(&[otter_core::shards::CATALOG, HOST_CATALOG])
```

Add `otter_io::CATALOG` if scripts use `Http.Get`. Then:

- **Check without running:** `otter_lang::check(Source::new(path, text), &catalog, &defines)`. It returns the 1.x `{ok, file, diagnostics}` JSON envelope (`to_json()`), and `otter_lang::render` prints a diagnostic for humans.
- **Run:** `Program::load(source, &catalog, &defines)`, then `program.run()`. The report has each entry wire's outcome, plus failures of spawned instances.
- **Script arguments:** `defines` maps `name` to a string, read as `@name` in scripts.
- **Files:** `@include("lib.shs")` and `@read("data.txt")` (or `@read("blob.bin" bytes: true)`) read files while the program loads, relative to the file naming them. `Program::load` and `check` read the filesystem (`FsFiles`); `Program::load_with`, `check_with` and `Session::set_files` take any `Files`: `FsFiles { include_paths }` searches more directories after the including file's (the CLI's `-I`), and `MemoryFiles` holds files by name, for a host without a filesystem (a device carrying its scripts in flash) and for tests. A host's own `Files` implements `resolve` (the file a path names, pushing every name it looks at, found or not, to `looked`) and `contents` (a file's bytes now, by that name). `Program::files` and `Session::files_read` list what a program read; `FileWatcher` reloads when any name its last load looked at changes, compared by exact bytes through the session's `Files`: an edit, a file created where a missing include was looked for, or one created earlier in the search order. A file is included once per program however often it is named. Diagnostics name the file they are in.
- **Logging:** `Log` writes to standard output; values print as text: whole floats without `.0`, other floats exact (1.x rounded to six digits). A host shard can log with `otter_core::log::emit` (or `emit_line`, with the instance). To receive lines as they are written, give the session a stream: `session.set_log_sink(Some(otter_core::log::Sink::stream(|line| ...)))`; each `log::Line` carries its text and the instance that wrote it (`Session` and the mesh map instances to wires), and arrives on the thread that wrote it, including `otter_io` tasks, so the callback must not block. `otter_core::log::capture` collects the lines a run logs instead, including lines logged by work it started through `otter_io` on other threads, which is useful in host tests.

The language is the 1.x syntax with the changes in [surface-syntax-review.md](surface-syntax-review.md) and the deviations listed in [current-state.md](current-state.md). The ones scripts hit most:

- `And`/`Or` become `All(...)`/`Any(...)`.
- `;` comments are rejected.
- Int and Float mix in arithmetic and comparisons.
- `f"..."` strings and `t.key` / `s.0` paths are supported.
- `Maybe` without `else` passes its input through.
- `@include` includes a file once, wherever it is named again (1.x needed `Once: true`; there is no `Fallback:`); `@read` reads at load time, never while the program runs.
- Code goes in wires on a mesh run by `@run(mesh fps: n)`; loose code runs as the `root` wire when there is no `@run`.
- Named, reusable code is a function: `@fn(Scale input: Float output: Float params: {factor: Float} { Math.Multiply(factor) })`, called like a shard (`3.0 | Scale(factor: 2.0)`). A function sees only its input, its parameters and the mesh variables it declares in `uses:`/`mutates:`; `stateful: true` gives it one persistent instance per call site (`Keep` allowed); `pure: true` is checked. `Return` ends the function with its input. Stateless functions may recurse (`Mesh::set_max_call_depth` bounds the nesting; exceeding it fails the instance with `recursion-limit`).

## 4. Testing a host

Test host shards through scripts, as `embedding.rs` does: `Program::load`, then `run()`, with `log::capture` for output. Use `check` for the compose errors your shards report. Run blocking shards against fakes of the host where possible; keep live runs for what only the real host can show.

## 5. Warm sessions and hot reload

Use `otter_lang::Session` when the host owns its event loop and long-lived
services. Keep connections and other expensive resources in the host and
look them up during shard activation. The same API runs on native, WASI and
ESP-IDF.

There are two replacement modes:

- **`reload_preserving`** keeps unchanged callers, locals, `Keep` and
  `Once` state, mesh values and suspended execution. Edited function bodies
  are selected at their next call (golden path §11). This is what
  `otter watch` uses.
- **`reload`** explicitly restarts the whole program on a fresh mesh. All
  script and mesh state, including host-declared mesh-variable schemas, is
  discarded. Host services owned outside the mesh survive either mode.

Both modes parse, lower and compose the candidate before committing. A
rejected edit returns its source and located diagnostics; the current
execution continues unchanged. Compilation is synchronous and pauses the
host loop on that thread.

### Preserving a caller while editing a function

```shards
@fn(Step input: None output: Int params: {} { 10 | Log })
@wire(main {
  Keep(counter 0)
  Inc(counter) | Log
  Step
} looped: true)
@mesh(m)
@schedule(m main)
@run(m fps: 30)
```

Changing `10` to `20` in `Step` keeps `main` running: its counter continues,
its `Keep` state is retained, and its next call to `Step` runs the edited
body. A call site selects the newest accepted body when it enters; an
invocation already in flight finishes on the body it started with, and its
own calls, entered after the edit, select the newest bodies. Callers whose
own definition did not change keep their compiled body and state, whatever
their callees did.

A candidate body is admitted only if its input, output, parameters and
`stateful` flag match what the callers were composed with, and its inferred
effects and mesh access fit inside what they were admitted with; otherwise
the edit is rejected with `reload-incompatible`, naming the function and
the change, before any running instance changes. A stateless function may
change its locals freely: every invocation gets the new frame layout, and
the caller never moves.

A stateful function keeps `Keep` slots that match the new body by name and
type (editing `Keep(n 0)` to `Keep(n 10)` keeps the value of `n`;
reordering `Keep`s keeps both); slots that changed type or disappeared
reset, and so does native shard state inside the body, since no shard
declares a compatibility contract yet. Resets happen at the call site's
next entry. Whether a reset is acceptable is the host's policy
(`ResetPolicy`): a `Session` rejects such edits by default
(`reload-resets-state`), and `otter watch` applies them, since running
watch is the opt-in. `Session::reload_report` (and the `report` on
`WatchEvent::Reloaded`) lists what was retained, reset and restarted, by
name.

Other lifetime rules:

- Unrelated wires keep their execution, including pauses and pending I/O.
  Changed scheduled roots restart; removed schedules are cancelled.
  Retained instances keep scheduler order; new/restarted entries append.
  Reordering `@schedule` alone does not reorder retained instances.
- Unchanged stateful functions retain their state too. Their `Once` blocks
  do not repeat merely because another function was edited.
- `Spawn` remains a static compiled dependency. Editing its target can
  restart the code that owns that dependency. Spawned instances are detached:
  compatible ones survive independently, while changed/removed ones are
  cancelled. A spawned wire that calls an edited function selects the new
  body at its next call like any other caller.
- Entry roots cancelled by a reload report `Cancelled`, including spawned
  children. Failed entries stay stopped until the next accepted reload, and
  then run again.

`Session::with_mesh` accepts an idle, host-configured mesh. Preserving reload
keeps its mesh-variable frame and schema unchanged, so values survive even
when a scheduled root restarts. Scripts access declared mesh variables with
the usual reads and updates; a function reaches them only through its
declared `uses` and `mutates`. Creating/removing/changing the type of a mesh
variable during preserving reload is not supported. To reset such a session,
create a fresh configured mesh and session; `reload` alone starts from the
default empty mesh schema.

`tick` returns every newly finished entry and child outcome once and retires
the records. A session keeps no outcome history, only bounded entry status.
At `iterations`, remaining work is cancelled and reported in that tick.
An accepted reload resets the revision's tick budget and updates its `fps`;
`frame_interval()` exposes the suggested delay, with pacing owned by the host.
`stop` cancels everything and releases the mesh. Dropping the session also
cancels work, but cannot return cleanup errors.

`Session` drives a `Mesh` directly; a host configures the mesh it starts
from through `Session::with_mesh`. Compiled artifacts remain immutable:
body selection belongs to a mesh, and each active call site retains an
`Arc` to the body it selected. Candidate caches are fresh per revision. Old
code remains only while referenced by retained roots or call sites, not in
a cumulative revision history. The process-wide type registry still interns
types for the process lifetime.

Cancellation on root replacement, stop, or full restart drops pending
futures, but does not join detached blocking workers or undo external
effects. Such operations must cooperate with cancellation; a host requiring
strict worker quiescence must enforce it in its service before admitting new
work. Preserving an in-flight invocation avoids restarting that call, but
does not solve every host-side cancellation race.

Offline tests in `crates/otter-cli/tests/embedding.rs` verify warm service
reuse, cancellation on full reload, and pending-operation completion across
preserving reload. The shared frontend suite verifies caller counters,
unchanged `Once` state, deep call boundaries, mesh values, incompatible
edits, state resets under both policies, cleanup failures, and retained
spawned children.
Hosts can use the same pattern with recorded inputs to test without live I/O.

### Watching a file

```sh
cargo run -p otter-cli -- watch live.shs
cargo run -p otter-cli -- watch live.shs key:value
```

The CLI polls contents every 100 ms and requires two identical samples before
trying a revision. Atomic saves and same-size edits work. Invalid source,
incompatible changes or read errors preserve the current execution and are
reported once until observed contents/error change. A watcher stays alive
after completion, the iteration limit or runtime failure; failed wires are
explicitly reported as stopped until the next successful reload/restart.
`fps` controls ticking; without it the watcher uses a 16 ms interval and
continues checking edits even when the script requests a very low frame rate.

Each accepted edit prints what it retained, reset and restarted. Enter `r`
to validate the current file and explicitly restart all script execution in
the same process. Enter `q`, send Ctrl-C, or close stdin to
cancel the execution and exit cleanly. Successful shutdown does not mean
every revision ran successfully.

Embedding hosts can reuse the same watcher through `otter_lang::FileWatcher`.
It owns content comparison, the two-sample save check, rejected-revision
suppression, preserving reload and tick pacing. It installs no keyboard,
signal, logging or async-runtime handlers. For a blocking host loop:

```rust
use otter_core::Catalog;
use otter_lang::{FileWatcher, Session, WatchControl, WatchEvent};
use std::collections::HashMap;
use std::sync::mpsc;

let catalog = Catalog::new(&[otter_core::shards::CATALOG]).unwrap();
let mut session = Session::new(); // or Session::with_mesh(...)
let (commands, input) = mpsc::channel::<WatchControl>();
// Hand `commands` to your UI/input thread. Send Restart or Stop as needed.
FileWatcher::new("live.shs").run(
    &mut session, &catalog, &HashMap::new(),
    || input.try_recv().unwrap_or(WatchControl::Continue),
    |event| match event {
        WatchEvent::Rejected { source, diagnostics } => {
            for d in diagnostics { eprint!("{}", otter_lang::render(&d, &source)); }
        }
        WatchEvent::ReadError(error) => eprintln!("{error}"),
        WatchEvent::Reloaded { finished, .. }
        | WatchEvent::Tick(finished)
        | WatchEvent::Stopped(finished) => {
            for f in finished { println!("{}: {:?}", f.wire, f.outcome); }
        }
    },
);
```

`Tick` fires after every session tick, even with no outcomes. `Reloaded` fires
after successful installation, before that revision's first tick. `Rejected`
carries the rejected source for diagnostics; `ReadError` leaves the session
running. `Restart` validates even unchanged/rejected file contents immediately,
bypassing save stability. `Stop` cancels the session and emits `Stopped`.
For a host that already owns an event loop, call `FileWatcher::poll` instead:
it performs due reads/reloads/ticks without sleeping and returns the suggested
delay. That host calls `Session::stop` on shutdown. Both APIs run file reads,
compilation and callbacks synchronously on the driving thread.

This watches one source file. Dependency watching, asynchronous compilation,
general state migration, cross-revision cache reuse and a network serving
protocol remain deferred. An embedding host can trigger either reload mode
from its own watcher or command channel.

### Execution limits (M4)

`Mesh::set_max_call_depth(n)` limits nested named invocations; the default is 256 on native/WASI and 32 on ESP-IDF. Exceeding it reports an activation diagnostic with code `recursion-limit`. Anonymous control blocks do not count as named calls. The compose nesting limit remains separately enforced while compose still recurses. A preserving revision retains the mesh's configured runtime limit. The trampoline resumes the active leaf directly and owns child state centrally; LeafShard and AsyncShard implementations need no changes.

### Compose-time evaluation limits (M8)

`#( ... )` runs a pipeline while the mesh composes ([metaprogramming.md §2](metaprogramming.md#2-m8-compose-time-evaluation)). `Mesh::set_eval_limits(EvalLimits { fuel, depth, value_bytes, output_bytes })` sets its budgets; the defaults are small enough for the device (`EvalLimits::default()`), and a preserving revision keeps them. A result cached under larger limits is not accepted under smaller ones. Only the core shards on the audited list in `compose_time.rs` run at compose time: a host shard is never eligible, whatever its effects, and `#( )` reaching one is a `not-compose-time` diagnostic. `LeafCtx::meter()` is `None` at run time; it exists for the listed shards, which charge their work to it.
