//! Benchmark workloads, shared by the examples and the instruction gate
//! (`docs/performance-gates.md`).
//!
//! At the top level, the wires for the instance benchmark (design doc §5),
//! mirroring the 1.x baseline's `shards/tests/bench-instances.shs` with the prototype's shard
//! subset: state set up in `Once`, vector math, a conditional, and a call
//! into a function that suspends. Not ported, because the prototype has no
//! equivalent yet: the 1.x entity's sequence/table literals, `Take`, `Match`
//! and `Regex.Match`.

use crate::compose::WireDef;
use crate::function::FunctionDef;
use crate::shards::defs::*;
use crate::types::Type;
use crate::var::Var;

/// Mesh variables the wires expect, as `(name, initial value, mutable)`:
/// `n` (instances to spawn), `ready-count` (instances that reached their
/// first activation) and `iterations` (completed entity loop iterations, to
/// verify the work done).
pub fn mesh_vars(n: i64) -> Vec<(&'static str, Var, bool)> {
  vec![
    ("n", Var::Int(n), false),
    ("ready-count", Var::Int(0), true),
    ("iterations", Var::Int(0), true),
  ]
}

/// The functions the wires call: `Think` takes the entity's energy, suspends
/// midway, and returns the new energy.
pub fn functions() -> Vec<FunctionDef> {
  vec![
    FunctionDef::new("Think", Type::float(), Type::float()).body(vec![
      add(val(Var::Float(0.5))),
      declare("energy"),
      pause(),
      when(
        vec![is_more_equal(val(Var::Float(100.0)))],
        vec![konst(Var::Float(0.0)), update("energy")],
      ),
      get("energy"),
    ]),
  ]
}

pub fn wires() -> Vec<WireDef> {
  vec![
    WireDef {
      name: "entity".into(),
      looped: true,
      flow: vec![
        keep("energy", Var::Float(0.0)),
        keep("pos", Var::float3(0.0, 0.0, 0.0)),
        once(vec![inc("ready-count")]),
        get("pos"),
        add(val(Var::float3(0.1, 0.0, 0.0))),
        update("pos"),
        get("energy"),
        call("Think", vec![]),
        update("energy"),
        inc("iterations"),
      ],
    },
    WireDef {
      name: "spawner".into(),
      looped: false,
      flow: vec![repeat(vec![spawn("entity")], var("n"))],
    },
  ]
}

/// The suspension-depth workload (`bench_depth`, the M4 gate): `depth`
/// nested functions, each calling the next, the deepest running `leaf`.
pub mod depth {
  use crate::compose::WireDef;
  use crate::function::FunctionDef;
  use crate::shard::ShardDef;
  use crate::shards::defs::*;
  use crate::types::Type;
  use crate::var::Var;

  /// `Level0` to `Level{depth}`; every level declares the mesh counter
  /// `resumes` it reaches through its callees (golden path §3.1).
  pub fn functions(depth: usize, leaf: Vec<ShardDef>) -> Vec<FunctionDef> {
    let level = |n: usize, body: Vec<ShardDef>| {
      FunctionDef::new(&format!("Level{n}"), Type::none(), Type::any())
        .uses(&["resumes"])
        .mutates(&["resumes"])
        .body(body)
    };
    let mut functions = vec![level(depth, leaf)];
    for n in 0..depth {
      functions.push(level(n, vec![call(&format!("Level{}", n + 1), vec![])]));
    }
    functions
  }

  /// The progress leaf: each `Pause` completes on resume, then `While`
  /// counts a resume and starts a new `Pause`.
  pub fn progress_leaf() -> Vec<ShardDef> {
    vec![while_(
      vec![konst(Var::Bool(true))],
      vec![inc("resumes"), pause()],
    )]
  }

  /// The looped `root` wire calling `Level0`.
  pub fn wires() -> Vec<WireDef> {
    vec![WireDef {
      name: "root".into(),
      looped: true,
      flow: vec![call("Level0", vec![])],
    }]
  }
}

/// The table storage workloads (`bench_tables`, docs/values-and-types.md
/// §7): small fixed records built and read through the VM.
pub mod tables {
  use std::sync::Arc;

  use crate::compose::WireDef;
  use crate::shard::ShardDef;
  use crate::shards::data::TABLE_MAKE;
  use crate::shards::defs::*;
  use crate::shards::values::EXPECT_INT;
  use crate::var::Var;

  pub fn key(i: usize) -> String {
    format!("k{i:02}")
  }

  /// `Table.Make` of `n` keys reading the locals `v0..vn`.
  pub fn table_make(n: usize) -> ShardDef {
    let keys = Var::from_seq(Arc::new((0..n).map(|i| Var::string(&key(i))).collect()));
    let mut params = vec![val(keys)];
    params.extend((0..n).map(|i| var(&format!("v{i}"))));
    ShardDef::new(&TABLE_MAKE, params)
  }

  fn literal_table(n: usize) -> Var {
    Var::table((0..n).map(|i| (key(i), Var::Int(i as i64))))
  }

  /// The VM cases by name: each a loop body over the locals `wire` declares.
  pub fn cases() -> Vec<(&'static str, Vec<ShardDef>)> {
    let take_key = |k: &str| take(val(Var::string(k)));
    vec![
      // Read a retained 16-key record by a literal key.
      (
        "const-take-16",
        vec![get("t"), take_key("k07"), update("acc")],
      ),
      // Read it by a key held in a variable.
      (
        "var-key-take-16",
        vec![
          get("t"),
          take(var("key")),
          ShardDef::new(&EXPECT_INT, vec![]),
          update("acc"),
        ],
      ),
      // Build a record from locals and read one field.
      (
        "make-take-4",
        vec![table_make(4), take_key("k02"), update("acc")],
      ),
      (
        "make-take-16",
        vec![table_make(16), take_key("k07"), update("acc")],
      ),
      // Rebuild while the previous record is retained by `t`.
      ("rebuild-16", vec![table_make(16), update("t")]),
      // Two same-shape constructors in one segment: the second reuses the
      // first's unique storage.
      ("chain-16", vec![table_make(16), table_make(16)]),
    ]
  }

  /// A wire `bench` that declares `v0..v15`, `acc`, `t` and `key`, then
  /// repeats `body` `iterations` times.
  pub fn wire(body: Vec<ShardDef>, iterations: i64) -> WireDef {
    let mut flow: Vec<ShardDef> = (0..16)
      .flat_map(|i| [konst(Var::Int(i)), declare(&format!("v{i}"))])
      .collect();
    flow.extend([
      konst(Var::Int(0)),
      declare("acc"),
      konst(literal_table(16)),
      declare("t"),
      konst(Var::string("k07")),
      declare("key"),
      repeat(body, val(Var::Int(iterations))),
    ]);
    WireDef {
      name: "bench".into(),
      looped: false,
      flow,
    }
  }
}
