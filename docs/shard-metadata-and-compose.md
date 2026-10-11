# Shard Metadata and Compose Design

**Status:** Agreed (2026-10-04) after review by Astra, Claude and the maintainer; review refinements are folded in below. **Implemented** (§9): the first slice (§7) for `Http.Get`, `Add` and `When`, then extended to every shard in the core catalog.
**Scope:** Self-documentation, argument decoding, and the division of responsibility between shared validation and each shard's compose function. This is not a plan to reproduce the 1.x composer or parameter machinery one for one.

**Proposal:** give each shard one static description that powers discovery, documentation, and argument decoding. Let the shard's compose function own context-dependent validation and specialization. Preserve useful verification guarantees from 1.x while choosing a simpler Rust implementation.

Related: [shard contract](prototype-shard-contract.md), [core design](shards-2-compose-split.md), and [AI roadmap](ai-first-roadmap.md), especially its discover → read → check loop. The runtime has one scheduler since golden path M4 (see the last section).

## 1. Current implementation

As inspected at `76f6f56`:

- `ShardType` in `crates/otter-core/src/shard.rs` contains a name, implementation version, and compose entry points for the backends. It has no parameter schema, help, examples, or input/output descriptions.
- Compose propagates output types through the flow. Individual shard compose functions check their supported inputs and parameters, resolve bindings, and compose children. `ComposeCtx` provides dependency recording and definite-initialization tracking.
- Parameters arrive as a positional `Vec<ParamValue>`; arity checks, defaults, and decoding are handwritten. `ParamValue` distinguishes literals, variable references, wire references, and nested flows.
- There is no agent-facing shard catalog or equivalent of the 1.x `docs`, `enumerate`, and `search` interface. Rust source comments alone do not provide that interface.
- Compose errors are strings. The language front end and its source-located structured diagnostics have not been ported.

The runtime prototype validates the compiled/state architecture. It does not establish parity with the 1.x type system or composer.

## 2. Responsibilities

| Layer | Owns |
|---|---|
| Static shard description | Identity, help, parameter names and forms, defaults, broad type constraints, examples, availability |
| Shared argument decoder | Positional/named argument mapping, duplicate/unknown/missing arguments, defaults, declared form and literal-type checks |
| Shard compose | Relationships between inputs and parameters, binding resolution, nested flow semantics, precise output types, specialization |
| Compose engine | Type propagation, scopes and slots, dependency recording, cache validation, diagnostic context |

Do not make the engine infer arbitrary semantics from metadata. Do not make every shard rewrite argument lookup and defaults either.

The same parameter definition must drive decoding and documentation. A separate handwritten schema beside handwritten decoding would create two competing descriptions of the API.

## 3. One static description per shard

Proposed minimum contents:

- Stable name, implementation version, short summary, and longer help.
- Parameters in positional order: name, required/optional status, explicit default when present, accepted argument forms, value-type constraint, and help. Optional without a default is distinct from a default of `None`.
- Broad input constraints and an output description. Output may be fixed, passthrough, or dependent on compose; dependent output must not be presented as an exact static type.
- Small examples. Initially these can be wire definitions in tests; textual examples can become executable when the front end lands.
- Supported targets. Discovery must not advertise a shard as runnable on a target where its implementation is unavailable.

Argument form and value type are distinct. A string literal, a reference to a String variable, and a wire name are not interchangeable merely because their source spelling might contain text. Declarations must state which forms are accepted. A literal-only parameter must not silently acquire support for variable references.

Metadata is shared by `LeafShard`, `AsyncShard` and the control-flow shards. **The description lives on `ShardType` itself, in one place**; the implementation is attached to it and never carries its own name, version or documentation.

**Implementation presence is derived, not declared:** a `ShardType` knows whether an implementation is attached, and composing a described shard without one is a `not-implemented` error. Only genuine target restrictions (for example, `otter-io` being native-only) are declared.

A runtime catalog aggregates the shards linked into that runtime and rejects duplicate identities. Reading it must not compose programs, instantiate shards, start reactors, or perform network/file I/O. **Registration is explicit:** each crate exposes a list of its shard types, and a runtime builds its catalog from those lists. No linker-section or global-constructor registration: it is less predictable on wasm and in static builds, and 1.x's registration side effects are what we are moving away from.

Deferred: which variables a shard declares or writes (1.x exposed these to tools as exposed variables). It is part of a shard's public contract and useful tooling metadata, but it is not in the first slice; exact bindings remain compose results.

## 4. Argument decoding and compose

Conceptual pipeline, not a proposed Rust signature:

```text
source arguments + shard description
    → decoded immutable arguments
    → shard compose(arguments, ComposeCtx)
    → compiled node + precise output type
```

The decoder applies declared defaults and preserves unresolved references. Compose resolves those references through `ComposeCtx`, so bindings, types, mutability, and dependencies remain part of the existing checking and caching model. Variable values are read at activation, not captured by compose.

Shared helpers should cover common tasks such as “resolve an operand of this type,” “require a mutable binding,” and “compose a predicate returning Bool.” Each shard explicitly invokes the semantic operations it needs. Special rules remain ordinary Rust code.

Two declarations of a requirement must not drift. For a simple accepted-type set, use the declared constraint for both introspection and validation. For relational constraints, publish a conservative description and let compose decide the actual case. For example, “supports Int, Float, Float3” does not imply that every pair of those types can be added.

**Named arguments change `ShardDef`:** its arguments become a list of optionally named values, which touches the definition helpers and the cache key.

**Cache normalization is deferred.** The cache key is the raw wire definition, so equivalent spellings (positional or named, an omitted default or the same value spelled out) may produce separate cache entries. That is safe, only a miss. Normalizing them is an optimization for later, not part of the first slice's acceptance. Runtime variable values stay outside the key.

**Implementation version:** identity and version stay on the shared description, and a semantic schema/default change bumps the version; help-only edits need not. No new invalidation machinery now: the current compose cache is process-local (in memory, per mesh), so a stale artifact cannot survive a rebuild. Persisted or shared compiled artifacts (a future `.sho`, a disk cache) will need a broader compatibility design.

## 5. Concrete examples

### Http.Get

Describe the current prototype's actual contract:

- `url`: required; String literal or variable reference resolving to String.
- `timeout`: optional; Int literal, default 10 seconds. Compose checks that it is positive. Variable timeouts are not currently supported.
- Input is ignored; output is String. Help explains non-success responses, timeout, and cancellation behavior.
- Native implementation; browser support remains separate work.

The decoder handles names, positions, defaults, and accepted argument forms. Compose resolves the URL binding and validates the timeout. Runtime code performs the request. A documentation query must not construct an HTTP client or start Tokio.

### Add

Describe one required operand, accepting a literal or variable reference, and the supported numeric types. Compose checks the exact input/operand combination and returns the resulting type. Help explains the same-type requirement and overflow behavior. Do not duplicate that relational rule in a second generic type-inference system.

### When

Describe predicate and body as nested flows, with passthrough output on normal completion. Compose requires a Bool predicate and applies conditional initialization rules to the body. Early exits require explicit control-flow semantics; a simple output descriptor must not pretend to prove all return paths.

This example tests that self-documentation can describe a composite without moving its flow semantics into a metadata interpreter.

## 6. Discovery and diagnostics

Build one catalog API usable in-process and by the future CLI. It should support a compact index, full per-shard descriptions, and basic name/summary search. Serve JSON with a versioned schema and symbolic type descriptions; internal type handles are not portable documentation identifiers.

Discovery metadata is a candidate filter, not proof that a suggested shard composes in a particular context. Precise checking still runs compose. The existing 1.x CLI behavior is a useful reference for this interface, not a requirement to preserve its implementation.

Start a structured diagnostic envelope with phase, stable error code, shard, optional parameter, message, and structured expected/actual information where applicable. Source location can be absent for Rust-built wires; the loader supplies it later. Shared decoding and compose helpers should emit the same format, rendered as either human text or JSON.

**Preserve the 1.x `shards check --json` field names and meanings where practical** (`shards/lang/src/check.rs`): `phase`, `severity`, `kind`, `message`, `file`/`line`/`column`, `shard`, `actual`/`expected` (`{name, basic_type}`), `param_index`, `did_you_mean`, `candidates`. The `skills/shards/` skill and the repairability harness (`shards/tests/check/`) depend on that schema. Fields 2.0 does not compute yet (`did_you_mean`, `candidates`) follow the existing convention of being omitted when empty; implementing those engines is not a prerequisite. New information goes in new fields (for example a fine-grained stable `code` and the parameter name) rather than by changing existing ones. Schema compatibility alone does not make the whole harness portable before the CLI and front end exist.

Source context must belong to the program use site: reusing a compiled artifact must not report the first caller's filename or location. Cache semantic results separately from use-site diagnostic context.

**Occurrence paths (implemented 2026-10-04).** A compose diagnostic carries `path`, root first: `{"wire": name}` starts a wire definition (the composed root, a `Do` sub-wire, a spawned wire), `{"shard": index, "name": ...}` indexes the current flow, and `{"param": name}` enters the preceding shard's nested flow or wire reference (for example `caller/1:Do/wire/sub/1:Add`). Compose adds the steps as the error leaves each flow and wire (`ComposeCtx::compose_flow`, `compose_inline`, `compose_wire`); the parameter is found by the identity of the nested flow or the name of the referenced wire in the decoded arguments. The steps index wire definitions, not source, so the same definition reached from two call sites has the same steps below its `Wire` step. The frontend keeps the spans in its own table, keyed by wire and steps. Compose errors are not cached, so a path never belongs to an earlier caller.

**Input provenance (implemented 2026-10-04).** A mismatch on a shard's own input (kind `input-type-mismatch`, or code `variable-type-mismatch`) carries `input_from`: the shard in the same flow that produced the value, or `"flow-input"` for the first shard of a flow, plus the shards in between that pass their input through (`OutputDesc::Passthrough`). The message ends with the same information, e.g. "(the input comes from 0:Const, through 1:Set, 2:When, which pass their input through unchanged)". This is fix 5 of [surface-syntax-review.md](surface-syntax-review.md) §7.

## 7. First implementation slice

Prove the design on `Http.Get`, `Add`, and `When` before converting the full subset:

1. Add static descriptions and an explicit catalog assembled from available shards.
2. Implement shared argument decoding, including named arguments as a Rust-level API even before the parser is ported. Give shard compose checked accessors; defer a derive macro until a concrete need appears.
3. Make the three shards consume that decoder and expose their descriptions through index/detail JSON.
4. Introduce structured errors for decoding and those shards' compose checks.
5. Keep every implementation on the shared descriptions and semantic compose functions.

Acceptance:

- Missing, unknown, duplicate, wrong-form, and wrong-type arguments produce useful diagnostics. Omitted defaults and explicit defaults produce equivalent decoded values and can reuse compose output.
- Changing a referenced variable's value needs no recompose; changing a compose-relevant binding/type invalidates reuse.
- Catalog defaults and accepted argument forms are the definitions actually used by decoding. Context-dependent combinations still fail in compose when appropriate.
- Enumerating/documenting `Http.Get` requires no instance, reactor, or request. Unavailable backend/target combinations are reported clearly.
- Examples and invalid cases exercise the runtime with matching semantic results. Existing acceptance tests continue to pass.
- `When`'s published description is produced from the same parameter declarations its decoder uses. A composite is where drift would show first, so the implementation should make drift structurally difficult: documentation and decoding both read **the same declarations**, rather than relying only on a test to keep two copies aligned.

This slice does not require full type-system parity, recursive types, a new parser, effect enforcement, or a general schema language. Broader compose work should follow an explicit list of guarantees we want, with conformance tests; preserving every 1.x implementation detail is not the objective.

## 8. Decisions (agreed 2026-10-04)

1. Static metadata owns the public argument contract, while compose owns context-dependent semantics.
2. Shared decoding consumes that same metadata; no independent handwritten defaults or argument schemas.
3. Start with explicit descriptions and checked accessors, deferring code generation rather than choosing a macro system up front.
4. Expose both static descriptions and precise compose diagnostics, with dynamic output types described honestly.
5. The three-shard implementation slice comes before broader porting.

Review refinements, folded in above: one description on `ShardType` (§3); derived backend availability (§3); explicit per-crate catalog lists (§3); named arguments and the `ShardDef` change (§4); deferred cache normalization and a process-local cache note (§4); 1.x diagnostic field names (§6); the `When` acceptance check (§7); variable declarations/writes deferred (§3).

Still open: the ergonomic Rust API for decoded arguments, the catalog registration mechanism, and the smallest reusable type-constraint representation. The first slice should resolve these without building a second programming language inside metadata.

## 9. First slice: what was built (2026-10-04)

- **Descriptions** (`crates/otter-core/src/describe.rs`): `ShardDesc` on every `ShardType`, with summary, help, parameter declarations (name, accepted forms, value types, required/optional/default), input and output descriptions, and target restrictions. Undescribed shards use `ShardDesc::undocumented` and keep positional arguments; named arguments to them are rejected.
- **Structural drift prevention:** the implementation is attached with `ShardType::new(desc).implemented_by::<S>()`, and attaching one whose name or version differs from the description **fails to compile** (a const assertion; covered by a `compile_fail` doctest). `LeafShard`/`AsyncShard` take their identity from their `DESC`; `When`'s implementation takes its identity from `WHEN_DESC`. Each parameter list is a single `static`, read by both the decoder and the catalog.
- **Decoding** (`args.rs`): positional and named arguments (`Arg`, `ShardDef::with_args`), defaults, accepted forms and literal types, with diagnostics for missing, unknown, duplicate, wrong-form, wrong-type, positional-after-named and too many arguments. Compose reads the result through checked accessors on `Args`.
- **Diagnostics** (`diagnostic.rs`): `Error::Diagnostic`, with 1.x field names, `basic_type` using 1.x `SHType` codes, a fine-grained `code`, and `param`; `did_you_mean`/`candidates` omitted. Decoding errors are phase `construct`; the three shards' compose checks are phase `compose` (`input-type-mismatch`, `predicate-not-bool`, `invalid-argument-value`, `wrong-variable-type`). A described shard without an implementation is a `not-implemented` diagnostic.
- **Catalog** (`catalog.rs`): `Catalog::new(&[otter_core::shards::CATALOG, otter_io::CATALOG])` from explicit per-crate lists, rejecting duplicates; `index_json`, `describe_json` and `search`, under the schema `otter-catalog/1`. Describing `Http.Get` does not start its runtime (tested).
- **Small builds:** the `docs` cargo feature (on by default; forwarded by `otter-io`). Without it, all prose written through `shard_doc!` compiles to empty strings; the argument contract is unchanged. CI runs the metadata tests both ways.

**Review fixes (Astra, after `6d14265`):**

- Variable-resolution errors are structured: `ComposeCtx::read_var` reports `unknown-variable` and `possibly-uninitialized` diagnostics (for every shard that reads variables), and shards reading a declared parameter add it with `Error::with_param`.
- Defaults follow the declared contract: one `check_params` function rejects a default that the parameter's own forms/types would reject, and duplicate parameter names. `ShardType::new` runs it at compile time (an invalid declaration does not compile; `compile_fail` doctest), and the decoder runs it for descriptions used directly (`invalid-declaration`).
- Diagnostic types map their 1.x `basic_type` from the full type description (`[Int]` is `56`), independently of the smaller `TypeName` used in descriptions.

**All shards described (2026-10-04).** The remaining 15 shards in the core catalog (`Const`, `Set`, `Update`, `Get`, `Inc`, `IsLess`, `IsMoreEqual`, `Once`, `Repeat`, `While`, `Do`, `Pause`, `Spawn`, and the test shards `Probe` and `Request`) have descriptions derived from what their compose actually accepted: their forms, literal types, defaults (`Pause`'s `Seconds` defaults to `0.0`, its previous behavior when omitted), and flow and output behavior. Their compose logic reads decoded arguments by name; context-dependent checks stay in compose and are now structured diagnostics (binding existence, mutability and type for `Set`/`Update`/`Inc`/`Get`, the input/operand relation for the comparisons, an Int binding for `Repeat`'s `Times`, a `Pause` duration that is finite, non-negative and representable (`Duration::try_from_secs_f64`), unknown and recursive wires for `Do`/`Spawn`, which attribute only those reference errors to their `Wire` parameter and leave diagnostics from inside the called wire with the shard that raised them, `Probe` modes, a non-negative `Request` delay). No shard in the core crate reports plain-string compose errors any more. Verification:

- every declared parameter of every shard accepts exactly its documented forms (a generic test over the catalog);
- the full behavior suite (`tests/prototype.rs`, which builds every shard's arguments through the definition helpers) passes unchanged through the decoder;
- each shard's compose-time checks have a test;
- deliberately narrowing one declaration (`Repeat`'s `Times` to literals only) fails both the compose checks and a behavior test.

Variable declaration/write metadata and front-end work remain separate, as agreed.

Tests: `crates/otter-core/tests/metadata.rs` (natively and on wasm) and `crates/otter-io/tests/catalog.rs`, plus named-argument and compose-diagnostic cases in `crates/otter-io/tests/http.rs` against the local server. Not done, as planned: cache normalization, variable declaration/write metadata, and the CLI. Describing the other shards was done afterwards (above).

## Golden path M3: signatures and inference

`signature::Signature` is the common descriptor for native descriptions and owned script metadata. It carries name/revision, input/output, ordered parameters with types and typed literal defaults, lifetime, typed mesh reads/writes, effects, docs and an optional source location. Owned names and docs use `Cow`, without leaking allocations. Native generic outputs retain their passthrough/same-as-input/dynamic relationship; compiled wire signatures have exact types.

Every shipped core shard declares effects and lifetime. `Keep` and `Once` require persistent state; temporary suspension state is stateless. `Time.Now` reads time, `Pause` suspends and reads time, `Log` performs I/O, and `Http.Get` suspends, performs I/O and uses a timeout. Effects are conservative potential effects: `Maybe` includes logging even when a particular call is silent, and `Spawn` includes the spawned body's effects. `Spawn` itself and the lifecycle tracer `Probe` require no state remembered across invocations. Undeclared host metadata stays unknown. These are trusted declarations, not a sandbox. The `pure` check and script declarations land in M5.

Compose unions effects, required lifetime and mesh access through anonymous flows, computed operands, `Do` and cached `Spawn` targets. Binding lookups alone do not imply reads; read-modify-write operations record both access modes. Immutable compiled analysis stores semantic occurrence paths only. The frontend prefixes each call site's path and resolves source spans independently, so cached bodies cannot retain another source file's locations.

`describe` JSON includes the common `signature`, and catalog summaries include effects and lifetime. `check --json` adds `wires`: each successful root has inferred effects, lifetime requirements, mesh access and an `occurrences` array with semantic path, exact input/output types, effects, lifetime and source span/line/column when available. Nested and generated lowering occurrences are included. A function call's occurrence also has `call`: its `path` (`inlined`, the callee's code and its blocks in place; `vm`, the callee's straight-line body in the VM on a frame the site keeps; `framed`, an engine frame, and a frame per block run unless the block is leaf code) and, unless inlined, the `reason` (the first shard without a VM form in the body or a block, `stateful`, recursion, the inlining budget), from the same decision compose makes (`compose::inline_plan`, 2026-10-10). Compose records it only when asked (`Mesh::record_call_sites`, which `check` sets), so compiled flows elsewhere carry none of it. Failed roots report diagnostics without claiming complete analysis. The wire's inferred lifetime requirement may be stateless even though the process signature itself is stateful. Existing diagnostics fields are unchanged.

## Golden path M4: one scheduler (2026-10-07)

With the stackful scheduler deleted, a `ShardType` carries one implementation, attached with `implemented_by::<S>()`. The catalog's `backends` field is gone from the index and from `describe` (every listed shard runs on the one engine; `targets` still says where); the `backend-unavailable` diagnostic became `not-implemented`, reported when a described shard with no implementation is composed. Compose is no longer generic over a backend: `ComposeCtx<'_>`, `CompiledWire` and `CompiledFlow` have no type parameter, and the frontend's `check`, `Program::run` and `Session` take no mesh type.
