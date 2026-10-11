//! Wires for the instance benchmark (design doc §5), mirroring the 1.x
//! baseline's `shards/tests/bench-instances.shs` with the prototype's shard
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
