//! What core enforces for host shards (defined here with the public API
//! only, as an external crate would): declared input types, and in debug
//! builds (or with the `output-checks` feature) that produced values fit
//! the shard's compose output type.

use otter_core::args::Args;
use otter_core::compose::ComposeCtx;
use otter_core::describe::{InputDesc, OutputDesc, Params, ShardDesc, Targets, TypeName};
use otter_core::instance::{InstanceCtx, LeafCtx};
use otter_core::shards::leaf::{LeafShard, leaf_type};
use otter_core::{Composed, Flow, Mesh, Result, ShardDef, ShardType, Type, Var, WireDef};

/// Declares a fixed record with `name: String` but produces an Int there:
/// a shard that drifted from its declared type.
struct Drifted;

const DRIFTED_DESC: ShardDesc = ShardDesc {
  name: "Host.Drifted",
  version: 1,
  summary: "",
  help: "",
  params: Params::Declared(&[]),
  input: InputDesc::Types(&[TypeName::Int]),
  output: OutputDesc::Dynamic("{name: String}"),
  targets: Targets::All,
  aliases: &[],
  effects: otter_core::signature::Effects::UNKNOWN,
  lifetime: otter_core::signature::Lifetime::Unknown,
};

impl LeafShard for Drifted {
  type Compiled = ();
  type State = ();
  const DESC: ShardDesc = DRIFTED_DESC;

  fn compose(_: &Args, _: &mut ComposeCtx<'_>) -> Result<Composed<()>> {
    Ok(Composed {
      compiled: (),
      output: Type::fixed_table([("name", Type::string())]),
    })
  }

  fn instantiate(_: &(), _: &mut InstanceCtx) -> Result<()> {
    Ok(())
  }

  fn activate(_: &(), _: &mut (), _: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    Ok(Flow::Next(Var::table([("name", input.clone())])))
  }
}

static DRIFTED: ShardType = leaf_type::<Drifted>();

/// Takes a record with `addr` and `guid` (a full input type), and passes
/// it through.
struct Record;

fn record_type() -> Type {
  Type::fixed_table([("addr", Type::int()), ("guid", Type::seq(Type::int()))])
}

const RECORD_DESC: ShardDesc = ShardDesc {
  name: "Host.Record",
  version: 1,
  summary: "",
  help: "",
  params: Params::Declared(&[]),
  input: InputDesc::Typed(record_type),
  output: OutputDesc::Passthrough,
  targets: Targets::All,
  aliases: &[],
  effects: otter_core::signature::Effects::UNKNOWN,
  lifetime: otter_core::signature::Lifetime::Unknown,
};

impl LeafShard for Record {
  type Compiled = ();
  type State = ();
  const DESC: ShardDesc = RECORD_DESC;

  fn compose(_: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<()>> {
    // No shape check here: core enforced the declared input.
    Ok(Composed {
      compiled: (),
      output: ctx.input(),
    })
  }

  fn instantiate(_: &(), _: &mut InstanceCtx) -> Result<()> {
    Ok(())
  }

  fn activate(_: &(), _: &mut (), _: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    Ok(Flow::Next(input.clone()))
  }
}

static RECORD: ShardType = leaf_type::<Record>();

fn record_wire(input: Var) -> WireDef {
  WireDef {
    name: "r".into(),
    looped: false,
    flow: vec![
      otter_core::shards::defs::konst(input),
      ShardDef::new(&RECORD, vec![]),
    ],
  }
}

#[test]
fn the_catalog_documents_a_full_input_type() {
  let catalog = otter_core::Catalog::new(&[&[&RECORD]]).unwrap();
  let json = catalog.describe_json("Host.Record").unwrap();
  assert!(
    json.contains("\"input\":{\"kind\":\"type\",\"type\":\"{addr: Int guid: [Int]}\"}"),
    "{json}"
  );
}

fn wire(input: Var) -> WireDef {
  WireDef {
    name: "w".into(),
    looped: false,
    flow: vec![
      otter_core::shards::defs::konst(input),
      ShardDef::new(&DRIFTED, vec![]),
    ],
  }
}

#[test]
fn unknown_host_effects_propagate_through_nested_flows() {
  use otter_core::shards::defs::{konst, sub};
  let mut mesh = Mesh::new();
  mesh.add_wire(WireDef {
    name: "unknown".into(),
    looped: false,
    flow: vec![sub(vec![
      konst(Var::Int(1)),
      ShardDef::new(&DRIFTED, vec![]),
    ])],
  });
  let wire = mesh.compile("unknown", Type::none()).unwrap();
  assert!(wire.flow.analysis.effects.unknown);
  assert!(
    wire
      .flow
      .analysis
      .occurrences
      .iter()
      .next()
      .unwrap()
      .effects
      .unknown
  );
  assert_eq!(
    wire.flow.analysis.lifetime,
    otter_core::signature::Lifetime::Unknown
  );
}

#[test]
fn declared_input_types_are_enforced() {
  let mut mesh = Mesh::new();
  mesh.add_wire(wire(Var::string("not an Int")));
  let err = mesh
    .compile("w", Type::none())
    .err()
    .expect("a compose error");
  let d = err.diagnostic().expect("structured");
  assert_eq!(
    (d.code, d.shard.as_deref()),
    ("input-type-mismatch", Some("Host.Drifted"))
  );
}

#[test]
fn a_full_input_type_checks_the_record_shape() {
  let guid = || Var::from_seq(std::sync::Arc::new(vec![Var::Int(1), Var::Int(2)]));
  let mut mesh = Mesh::new();
  mesh.add_wire(record_wire(Var::table([
    ("addr", Var::Int(7)),
    ("guid", guid()),
  ])));
  assert!(mesh.compile("r", Type::none()).is_ok());

  // A record missing a key fails at compose, not at runtime.
  let mut mesh = Mesh::new();
  mesh.add_wire(record_wire(Var::table([("addr", Var::Int(7))])));
  let err = mesh
    .compile("r", Type::none())
    .err()
    .expect("a compose error");
  let d = err.diagnostic().expect("structured");
  assert_eq!(d.code, "input-type-mismatch");
  assert!(
    d.message
      .contains("Host.Record needs {addr: Int guid: [Int]} input, got {addr: Int}"),
    "{}",
    d.message
  );
}

#[cfg(any(debug_assertions, feature = "output-checks"))]
#[test]
fn a_value_outside_the_declared_output_type_fails_the_instance() {
  use otter_core::{Error, Outcome};
  let mut mesh = Mesh::new();
  mesh.add_wire(wire(Var::Int(1)));
  let w = mesh.compile("w", Type::none()).expect("composes");
  let id = mesh.spawn(&w, Var::None).expect("spawns");
  mesh.run(2);
  match mesh.outcome(id) {
    Some(Outcome::Failed(Error::Activation(message))) => {
      assert!(
        message.contains("Host.Drifted produced {name: 1}"),
        "{message}"
      );
      assert!(
        message.contains("{name: String} does not admit"),
        "{message}"
      );
    }
    other => panic!("expected a failure, got {other:?}"),
  }
}

/// Hosts read and build collections through `Var` accessors and `Table`,
/// never through the storage type (golden-path.md §7.1).
#[test]
fn hosts_use_opaque_collections() {
  use otter_core::{Table, TableBuilder};
  let built = TableBuilder::new()
    .with("b", Var::Int(2))
    .with("a", Var::string("x"))
    .with("b", Var::Int(3))
    .build();
  let collected: Table = [("a", Var::string("x")), ("b", Var::Int(3))]
    .into_iter()
    .collect();
  assert_eq!(built, collected);
  assert_eq!(
    Var::from(built.clone()),
    Var::table([("b", Var::Int(3)), ("a", Var::string("x"))])
  );
  let mut b = Table::builder();
  b.insert("only", Var::None);
  assert_eq!(b.build().len(), 1);

  let value = Var::from_table(built);
  let table = value.as_table().unwrap();
  assert_eq!(table.len(), 2);
  assert!(!table.is_empty() && Table::new().is_empty());
  assert_eq!(table.get("a").and_then(Var::as_str), Some("x"));
  assert_eq!(table.get("missing"), None);
  assert!(table.contains_key("b"));
  // Sorted iteration, whatever the insertion order.
  assert_eq!(table.keys().collect::<Vec<_>>(), ["a", "b"]);
  assert_eq!(
    table.iter().map(|(k, _)| k).rev().collect::<Vec<_>>(),
    ["b", "a"]
  );
  assert_eq!(
    table.values().cloned().collect::<Vec<_>>(),
    [Var::string("x"), Var::Int(3)]
  );

  let seq = Var::from_seq(std::sync::Arc::new(vec![Var::Int(1), value.clone()]));
  assert_eq!(seq.as_seq().map(<[Var]>::len), Some(2));
  assert_eq!(seq.as_table(), None);
  assert_eq!(value.as_seq(), None);
  assert_eq!(Var::Int(1).as_str(), None);

  // Deriving a changed table: shared storage is copied, unique storage is
  // taken, and the original snapshot never changes.
  let original = table.clone();
  let mut changed = original.clone().into_builder();
  assert_eq!(changed.remove("a"), Some(Var::string("x")));
  changed.insert("b", Var::Int(4)).insert("c", Var::None);
  assert_eq!(changed.get("b"), Some(&Var::Int(4)));
  let changed = changed.build();
  assert_eq!(changed.keys().collect::<Vec<_>>(), ["b", "c"]);
  assert_eq!(original, *table);
  let unique = Table::builder().with("k", Var::Int(1)).build();
  assert_eq!(unique.storage_owners(), 1);
  assert_eq!(
    unique
      .into_builder()
      .with("k", Var::Int(2))
      .build()
      .get("k"),
    Some(&Var::Int(2))
  );
}
