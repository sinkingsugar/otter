# Values and Types for the Frontend Slice

**Status:** §2-§4 implemented 2026-10-04 (`var.rs`, `types.rs`, `describe.rs`); §5 follows with the frontend. See [current-state.md](current-state.md).
**Scope:** the value and type additions the frontend slice needs before real scripts can lower onto the core: tables, the missing float vectors, and type sets. It extends contract §3 (types) and §9 (values) in [prototype-shard-contract.md](prototype-shard-contract.md); it does not settle the final `Var` layout, which still waits for measurements.

## 1. Why now

Both reviews of the prototype (2026-10-04) named the value and type model as the largest untested part, and the scripts of the first external host confirm it. They depend on:

- **fixed-shape tables** whose `t.key` access composes to the field's type, and which reject an unknown key at compose (host shards return fixed records, and scripts pass table literals to host shards);
- **Float2, Float3 and Float4** values (screen points, positions, poses);
- **"value or none"** fields: an unknown reading is an explicit none, never a guess.

Almost every 1.x test script also uses tables. Lowering source onto a model without them would either stall the frontend or force rushed type decisions, so these land first, as explicit work.

## 2. Values

`Var` gains:

| Variant | Payload | Notes |
|---|---|---|
| `Float2` | `Float2([f32; 2])` | Changed from `[f64; 2]` in golden path M6: all float vectors are f32 (1.x's `Float2` was two f64). |
| `Float3` | `Float3([f32; 3])` | |
| `Float4` | `Float4([f32; 4])` | 1.x layout: four 32-bit floats; the wrapper is 16-aligned. |
| `Table` | `Table` (opaque) | String keys only, in key order. Hosts use its accessors ([embedding.md](embedding.md) §2). |
| `Bytes` | `Arc<[u8]>` | Added 2026-10-09 with `@read("f" bytes: true)`: the type `Bytes` (1.x type code 51), equality and hashing by content, printed `@bytes(` and two hex digits a byte `)` (no source literal reads it back yet). `Count` gives its length, `ToHex` its hex, `BytesToString` reads it as UTF-8 (failing otherwise) and `StringToBytes` gives a string's bytes; all four are compose-time eligible. |

The vector payloads are newtypes that deref to their array (`v[0]`, `v.iter()`, `v.map(..)`), with `From` in both directions and `Var::float2(x, y)` constructors; a vector result is computed in f64 and rounded to f32 once. `Var` is 32 bytes with a `u8` tag at offset 0 (RFC 2195 layout), aligned to 16 on every target (what `Float4` needs; alignment 32 was measured within noise in time and reversed for its size cost in every frame, instruction and step holding a value, see [`bench/values`](../bench/values/README.md)); `Float4` sits at offset 16. `Option<Var>` and `Operand` use the tag's spare values and stay 32 bytes; `Step` and `Flow` carry a value and a tag and are 48. `tests/values.rs` pins the size, alignment and offset.

The variants holding storage (`String`, `Seq`, `Table`, `Bytes`) wrap it in `ManuallyDrop`, and `Var`'s hand-written `Drop` and `Clone` own it, as 1.x's `destroyVar`/`destroyVarSlow` do: a plain value is dropped or copied inline (one tag check), storage is released or counted out of line. A derived drop grows with every variant that owns storage; past LLVM's inlining budget every drop of a number became a call, and adding `Bytes` that way cost the VM suite 10 to 88 percent (2026-10-09). The VM's `Set` is the one place that counts references inline (`var::assign_copy`), since there the copy is the operation. Build these variants with `Var::string`, `Var::bytes`, `Var::seq` (a `Vec`), or `Var::from_string`, `from_bytes`, `from_seq` and `from_table` for storage already held; match them by reference (the payload derefs to its `Arc` or `Table`), and move storage out with `into_seq` and `into_table`. A shared sequence pushed to is copied once, bitwise with the references counted afterwards, with room to grow (`var::seq_mut`).

### 2.1 Table storage (golden path §7.3)

```rust
pub struct Table(TableRepr);
enum TableRepr {
  Struct { shape: Shape, slots: Arc<[Var]> }, // keys known at compose
  Map(Arc<Vec<(Arc<str>, Var)>>),           // sorted, binary search
}
```

- **`Shape`** is an interned sorted key list in the type registry (`Shape::new(keys)`, `keys()`, `index_of(key)`, `registered()`); equal key sets intern to one handle, so comparing shapes is comparing handles. A fixed table type carries its shape (`TableType::shape`).
- **Struct at runtime for every fixed table type.** Argument literals are converted when they are decoded (`Var::into_struct_tables`, nested tables included), `Table.Make` builds a struct table of its compose-time shape, and `Take` with a literal key on a fixed table compiles to an indexed read (`TakeCode::Slot`, the VM's `take-slot`). A map table that a fixed type admits has exactly the type's keys, so its sorted entries are the slots and the indexed read is valid on both representations.
- **Map for everything else:** open tables, and values hosts build with `TableBuilder`, `collect` or `Var::table` (the builder keeps a sorted vector; inserting in key order appends). Hosts that pass one shape many times can build struct tables with `Table::with_shape(shape, values)` or convert once with `into_struct`; neither is applied at the mesh boundary, since interning per value would grow the registry with unbounded key sets.
- **Admission** (`Type::admits`) of a struct value by a fixed type compares the shape handles first (no key lookups), then checks every slot's type; the slot check runs in every build, since `set_var` and `spawn` admit host values with it (a review finding against the plan's "slot checks behind `output-checks`"). A map value is checked key by key.
- **Equality, hashing, printing, iteration and `type_of`** are the same for equal contents in either representation (`tests/values.rs` pairs them). Equality, `type_of`, `Type::admits` and `into_struct_tables` visit a sequence or table held in several places within one value once (by storage address, for storage shared elsewhere), so a value built from copies of one value costs what it holds; parameter values in compose cache keys hash only their first 64 nodes (`Var::hash_prefix`), which stays consistent with equality. Copy-on-write: a same-shape constructor overwrites slots it uniquely owns; `into_builder` takes unique map storage and copies shared storage.

Measured ([`bench/values`](../bench/values/README.md), release, Apple M-series): building a 16-key record from locals and reading one field went from 614 ns and 4 allocations to 123 ns and 1; a literal-key read on a retained record from 80 ns to 42 ns; deriving a changed 64-key host table from 487 ns to 246 ns.

Decisions:

- **Table keys are strings.** 1.x allows any value as a key; nothing in the scripts we target needs that, and string keys keep tables cheap to hash, compare and print. Non-string keys can be added later as a separate key kind without changing string-keyed code.
- **Key order is sorted.** Both representations iterate in key order, giving deterministic iteration, printing, equality and hashing, so tables can appear in parameter literals, which are part of the compose cache key. 1.x insertion order is not preserved; scripts that depend on it are out of scope.
- **Copy on write.** Like `Seq`, a table is shared behind an `Arc`; a shard that changes it copies only when shared.
- **Identity equality** (bitwise floats) stays the rule for `PartialEq`/`Hash`, as for the other variants. The language's `Is` is a separate comparison.
- Integer vectors, `Color` and objects remain deferred until a ported script needs them.

## 3. Types

`TypeDesc` gains `Float2`, `Float4`, `Table(TableType)`, `Union(Vec<Type>)` and `Never`.

### 3.1 Tables

```rust
pub struct TableType {
  /// Known keys and their value types, sorted by key.
  pub keys: Vec<(Arc<str>, Type)>,
  /// The value type of any other key; `None` for a fixed table.
  pub rest: Option<Type>,
}
```

One description covers the three shapes in use:

- **Fixed (struct) table:** known keys, `rest: None`. Every key is present; reading a key that is not listed is a compose error. This is the 1.x `fixedStructTable` behavior hosts rely on.
- **Homogeneous table:** no known keys, `rest: Some(T)`. `Table` with `rest: Some(Any)` is "any table".
- **Mixed:** known keys plus a `rest` type.

A key whose value may be missing is typed as a set with `None` (`Float4 | None`), not as an optional key. Every listed key is present at runtime, so an unknown value is an explicit none, never a guess.

A table literal's type (`Var::type_of`) is the fixed table of its keys.

### 3.2 Unions

`Union` (named `Set` until 2026-10-05; renamed so "set" stays free for a possible runtime set value). Unions are canonical at construction (`Type::union`): nested unions are flattened, members sorted by a structural order (`structural_cmp`: stable across runs, unlike registry indices, and unambiguous, unlike printed forms, which can coincide for distinct tables) and deduplicated; a one-member union is that member, a union with `Any` is `Any`, and `Never` members drop out. Equal unions intern to the same handle.

`Never` is the type of something that produces no value: `Stop`, and a flow that reaches it. Every type accepts it, so a branch or predicate ending in `Stop` fits; compose still checks the unreachable shards after it.

A sequence literal with mixed element types now types as `[A | B]` instead of `[Any]`.

### 3.3 Acceptance

`Type::accepts(expected, actual)` is the one subtyping rule, used for parameter literals, variable updates and shard input checks. Exact equality is still used where identity matters (cache keys).

- `Any` accepts everything; `Any` as an *actual* type is accepted only by `Any` (an untyped value must be narrowed, as with 1.x `Expect*`). `Never` as an actual type is accepted by everything.
- Checking a runtime value against a type uses `Type::admits(&Var)`, which interns nothing (`value.type_of()` would intern every new shape in the never-freed registry). `Spawn`, `set_var` and `Expect*` use it.
- Expected set: some member accepts the actual type. Actual set: every member is accepted.
- `[E]` accepts `[A]` when `E` accepts `A` (values are immutable or copy-on-write, so covariance is sound).
- Expected table `X` accepts actual table `Y` when:
  - every key of `X` is in `Y`, with an accepted type (a key only covered by `Y.rest` may be missing at runtime, so it is rejected);
  - every other key of `Y` is accepted by `X.rest`, and fails if `X` is fixed;
  - if `Y` has a `rest`, `X.rest` exists and accepts it.
- Otherwise, the descriptions must be equal.

### 3.4 Static names

`TypeName` (const-friendly names in shard descriptions) gains `Float2`, `Float4`, `Seq` (any sequence) and `Table` (any table). `TypeName::matches` uses `accepts`. Diagnostics report 1.x `SHType` codes: Float2 11, Float4 13, Seq 56, Table 57; a set reports -1, with its full printed form in `name`.

### 3.5 Printing

`Float2`, `Float4`; `[T]` for sequences; `{a: Int b: String}` for a fixed table (keys that are not plain names are quoted: `{"two words": Int}`, also when printing values), `{a: Int ...: Float}` with a rest type, `{a: Int ...}` when the rest is `Any`, `{...}` for any table; `A | B` for sets, parenthesized inside a sequence element or table value when needed for clarity (`[(Int | None)]`).

## 4. The type registry

The process-wide registry stays (scoped registries are deferred), with two changes from the second review:

- **No copying under the lock.** Descriptions are allocated once and never freed (the registry already kept them for the process lifetime); `Type::desc()` returns `&'static TypeDesc` instead of cloning a description that may now hold a key list.
- **Readers do not serialize.** The registry is behind an `RwLock`: compose-time type checks and printing take a shared read lock; only interning a new description writes.

A lock-free read path is a later option if compose profiling shows the read lock.

## 6. Numbers mix

Agreed 2026-10-05. 1.x refused `Int` with `Float` in arithmetic and comparisons, which forced workarounds like `Math.Add(0.0)` and `IsMore(0.0)` against `IsMore(0)` in real scripts. In 2.0:

- **Arithmetic** (`Math.Add`, `Math.Subtract`, `Math.Multiply`, `Math.Divide`):
  - Int with Int stays Int. Overflow and division by zero are activation errors, and division truncates (`5 | Div(2)` is 2).
  - Int with Float, in either order, gives a Float (`5 | Div(2.0)` and `5.0 | Div(2)` are 2.5).
  - A float vector with a number, in either order, applies to each component.
  - Vectors of one size work per component; different sizes are a compose error.
  - Compose computes the result type from the two operands.
- **Comparisons** (`IsLess`, `IsMore`, `IsMoreEqual`, `IsLessEqual`) take Int and Float mixed, compared by value. `Is`, `IsNot` and `IsAny` treat `1` and `1.0` as equal.
- **Not changed:** variables keep their type, so a Float variable does not accept an Int through `Update`, and parameter literals still follow the declared types.

## 7. Table storage (decided) and value sets

Raised in review (2026-10-05) against 1.x, whose tables are a `flat_map` over a `stable_vector` so `cloneVar` could copy into an existing destination without allocating. Decided in golden path M6 (§2.1 above, [`bench/values`](../bench/values/README.md)):

- **Copy semantics stay copy-on-write.** Passing a table or assigning it only bumps a reference count; 1.x's main reason for `cloneVar` does not exist here.
- **Storage:** struct tables (interned shape, one slot per key) for fixed types, a sorted flat vector for the rest; `BTreeMap` is gone. A same-shape constructor segment overwrites the slots it uniquely owns (1.x's allocation-free steady state without copy semantics), and `Take` with a literal key resolves to a slot at compose.
- **The benchmark** (small fixed records, same-shape rebuilds, retained snapshots followed by a rebuild, growing dynamic tables; time and allocations) is `crates/otter-core/examples/bench_tables.rs`; its before/after numbers are recorded once. The `Var` layout (32 bytes, §2) was decided alongside.
- **Stable references to table entries** (a 1.x requirement) are not needed by value reads or compose-resolved field access; do not add them without a use.
- **A runtime set value** (removed from 1.x in 2024) waits for a ported script that needs one. Its membership rules must be settled first: whether `1` and `1.0` are one element, how NaN behaves, and iteration order. `Var`'s bitwise identity equality is deliberately not the language's equality, so it cannot be used as is.

## 8. Not in this step

- How the frontend lowers `t.key` and `s.0` (the reading shard computes the key's type from a fixed table; that shard lands with the frontend).
- Shards over tables (`Take`, `Count`, table literals with variables inside). These follow with the frontend's first shards.
- Recursive types, objects, integer vectors, bytes, the final `Var` layout.
