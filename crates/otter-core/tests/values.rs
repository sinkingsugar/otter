//! Values (golden path §7, test L): the pinned `Var` layout, f32 vectors,
//! and the two table representations behaving as one value.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

use otter_core::shards::defs::*;
use otter_core::{Float4, Mesh, Outcome, Shape, ShardDef, Table, Type, Var, WireDef};

fn hash_of(v: &impl Hash) -> u64 {
  let mut h = DefaultHasher::new();
  v.hash(&mut h);
  h.finish()
}

fn wire(flow: Vec<otter_core::ShardDef>) -> WireDef {
  WireDef {
    name: "w".into(),
    looped: false,
    flow,
  }
}

fn run(mesh: &mut Mesh, flow: Vec<otter_core::ShardDef>) -> Result<Var, otter_core::Error> {
  mesh.add_wire(wire(flow));
  let compiled = mesh.compile("w", Type::none())?;
  let id = mesh.spawn(&compiled, Var::None)?;
  mesh.run(10);
  match mesh.take_outcome(id) {
    Some(Outcome::Completed(v)) => Ok(v),
    Some(Outcome::Failed(e)) => Err(e),
    other => panic!("unexpected outcome {other:?}"),
  }
}

#[test]
fn var_is_32_bytes_with_float4_at_offset_16() {
  assert_eq!(std::mem::size_of::<Var>(), 32);
  // What Float4 needs; alignment 32 bought nothing measurable (bench/values).
  assert_eq!(std::mem::align_of::<Var>(), 16);
  assert_eq!(std::mem::align_of::<Float4>(), 16);
  let v = Var::float4(1.0, 2.0, 3.0, 4.0);
  let base = &v as *const Var as usize;
  let Var::Float4(payload) = &v else {
    unreachable!()
  };
  assert_eq!(payload as *const Float4 as usize - base, 16);
  // The tag's spare values stay available as a niche.
  assert_eq!(std::mem::size_of::<Option<Var>>(), 32);
}

#[test]
fn float2_rounds_to_f32_once_and_keeps_non_finite_components() {
  let seq = |items: Vec<Var>| Var::from_seq(Arc::new(items));
  let to_float2 = || otter_core::ShardDef::new(&otter_core::shards::values::TO_FLOAT2, vec![]);
  let mut mesh = Mesh::new();
  // Each component is rounded once, from the literal's f64.
  let v = run(
    &mut mesh,
    vec![
      konst(seq(vec![Var::Float(0.1), Var::Float(0.2)])),
      to_float2(),
    ],
  )
  .unwrap();
  assert_eq!(v, Var::float2(0.1, 0.2));
  assert_eq!(v.text(), "@f2(0.1 0.2)");
  assert_eq!(v.to_string(), "@f2(0.1 0.2)");
  // Arithmetic computes in f64 and rounds once at the end.
  let sum = run(
    &mut mesh,
    vec![konst(Var::float2(0.1, 0.2)), add(val(Var::Float(0.2)))],
  )
  .unwrap();
  let expect = |x: f32| (f64::from(x) + 0.2) as f32;
  assert_eq!(sum, Var::float2(expect(0.1), expect(0.2)));
  // A component read back is the f32's exact value.
  let x = run(
    &mut mesh,
    vec![konst(Var::float2(0.1, 0.2)), take(val(Var::Int(0)))],
  )
  .unwrap();
  assert_eq!(x, Var::Float(f64::from(0.1f32)));
  // Out-of-range literals become infinite; NaN stays NaN and is not equal
  // to itself under the language's equality, while `Var`'s identity
  // equality (cache keys) compares bits.
  let inf = run(
    &mut mesh,
    vec![
      konst(seq(vec![Var::Float(1e39), Var::Float(-1e39)])),
      to_float2(),
    ],
  )
  .unwrap();
  assert_eq!(inf, Var::float2(f32::INFINITY, f32::NEG_INFINITY));
  assert_eq!(inf.text(), "@f2(inf -inf)");
  let nan = Var::float2(f32::NAN, 0.0);
  assert_eq!(nan, nan.clone());
  assert_eq!(hash_of(&nan), hash_of(&nan.clone()));
  assert!(!otter_core::shards::values::values_equal(&nan, &nan));
  assert_ne!(Var::float2(0.0, 0.0), Var::float2(-0.0, 0.0));
  assert!(otter_core::shards::values::values_equal(
    &Var::float2(0.0, 0.0),
    &Var::float2(-0.0, 0.0)
  ));
}

#[test]
fn table_representations_are_one_value() {
  let map: Table = [("b", Var::string("x")), ("a", Var::Int(1))]
    .into_iter()
    .collect();
  let shape = Shape::new(["b", "a"]);
  let st = Table::with_shape(shape, [Var::Int(1), Var::string("x")]);
  assert_eq!(map.shape(), None);
  assert_eq!(st.shape(), Some(shape));
  for (x, y) in [(&map, &st), (&st, &map), (&map, &map), (&st, &st)] {
    assert_eq!(x, y);
    assert_eq!(hash_of(x), hash_of(y));
    assert_eq!(format!("{x:?}"), format!("{y:?}"));
    let (vx, vy) = (Var::from_table(x.clone()), Var::from_table(y.clone()));
    assert_eq!(vx.text(), vy.text());
    assert_eq!(vx.to_string(), "{a: 1 b: \"x\"}");
    assert_eq!(vx.type_of(), vy.type_of());
    assert_eq!(hash_of(&vx), hash_of(&vy));
    assert_eq!(x.keys().collect::<Vec<_>>(), ["a", "b"]);
    assert_eq!(
      x.iter().rev().map(|(k, _)| k).collect::<Vec<_>>(),
      ["b", "a"]
    );
    assert_eq!(x.get("a"), Some(&Var::Int(1)));
    assert_eq!(x.slot(1), Some(&Var::string("x")));
    assert_eq!(x.slot(2), None);
    assert_eq!(x.len(), 2);
  }
  assert_eq!(map.clone().into_struct(), st);
  assert_eq!(map.clone().into_struct().shape(), Some(shape));
  assert_ne!(
    st,
    Table::with_shape(shape, [Var::Int(2), Var::string("x")])
  );
  assert_ne!(
    map,
    Table::with_shape(Shape::new(["a", "c"]), [Var::Int(1), Var::string("x")])
  );
  // Conversion reaches nested tables, inside sequences too.
  let nested = Var::table([(
    "inner",
    Var::from_seq(Arc::new(vec![Var::table([("k", Var::None)])])),
  )])
  .into_struct_tables();
  let inner = nested.as_table().unwrap().get("inner").unwrap();
  assert_eq!(
    inner.as_seq().unwrap()[0].as_table().unwrap().shape(),
    Some(Shape::new(["k"]))
  );
  assert_eq!(
    nested.type_of(),
    Var::table([(
      "inner",
      Var::from_seq(Arc::new(vec![Var::table([("k", Var::None)])]))
    )])
    .type_of()
  );
}

#[test]
fn snapshots_stay_isolated_in_both_representations() {
  let shape = Shape::new(["a", "b"]);
  let st = Table::with_shape(shape, [Var::Int(1), Var::Int(2)]);
  let map: Table = st.iter().map(|(k, v)| (k, v.clone())).collect();
  for original in [st, map] {
    let snapshot = original.clone();
    assert_eq!(original.storage_owners(), 2);
    let changed = original
      .clone()
      .into_builder()
      .with("a", Var::Int(9))
      .build();
    assert_eq!(changed.get("a"), Some(&Var::Int(9)));
    assert_eq!(snapshot.get("a"), Some(&Var::Int(1)));
    assert_eq!(original, snapshot);
    assert_eq!(changed.shape(), None);
    drop(snapshot);
    assert_eq!(original.storage_owners(), 1);
  }
}

#[test]
fn literal_key_take_on_a_fixed_table_is_an_indexed_read() {
  let record = Var::table([("a", Var::Int(1)), ("b", Var::Int(2))]);
  let mut mesh = Mesh::new();
  mesh.add_wire(wire(vec![
    konst(record.clone()),
    take(val(Var::string("b"))),
  ]));
  let compiled = mesh.compile("w", Type::none()).unwrap();
  assert_eq!(
    compiled.flow.instruction_kinds(),
    ["const-drop", "take-slot"]
  );
  let id = mesh.spawn(&compiled, Var::None).unwrap();
  mesh.run(10);
  assert_eq!(mesh.take_outcome(id), Some(Outcome::Completed(Var::Int(2))));

  // A key read at activation, or an open table, looks the key up.
  let mut mesh = Mesh::new();
  mesh.declare_var("key", Var::string("b"), false);
  mesh.declare_var("open", record.clone(), true);
  mesh.add_wire(wire(vec![konst(record.clone()), take(var("key"))]));
  let compiled = mesh.compile("w", Type::none()).unwrap();
  assert_eq!(compiled.flow.instruction_kinds(), ["const-drop", "take"]);

  // A map-represented value admitted by the fixed type (a host value) is
  // read by position too: its sorted entries are the slots.
  let mut mesh = Mesh::new();
  mesh.declare_var("host", record.clone(), false);
  assert_eq!(
    mesh.get_var("host").unwrap().as_table().unwrap().shape(),
    None
  );
  let out = run(&mut mesh, vec![get("host"), take(val(Var::string("b")))]).unwrap();
  assert_eq!(out, Var::Int(2));
  // Literals reach activation as struct tables.
  let out = run(&mut Mesh::new(), vec![konst(record)]).unwrap();
  assert_eq!(
    out.as_table().unwrap().shape(),
    Some(Shape::new(["a", "b"]))
  );
}

#[test]
fn unknown_key_on_a_fixed_table_is_a_compose_error() {
  let mut mesh = Mesh::new();
  mesh.add_wire(wire(vec![
    konst(Var::table([("alpha", Var::Int(1)), ("beta", Var::Int(2))])),
    take(val(Var::string("alpah"))),
  ]));
  let err = match mesh.compile("w", Type::none()) {
    Ok(_) => panic!("composed"),
    Err(e) => e,
  };
  let otter_core::Error::Diagnostic(d) = err else {
    panic!("expected a diagnostic, got {err:?}")
  };
  assert_eq!(d.code, "unknown-key");
  assert_eq!(d.did_you_mean, ["alpha"]);
}

#[test]
fn fixed_types_admit_struct_values_by_shape_handle() {
  let ty = Type::fixed_table([("a", Type::int()), ("b", Type::string())]);
  let shape = ty.as_table().unwrap().shape.unwrap();
  assert_eq!(shape, Shape::new(["b", "a"]));
  let right = Var::from_table(Table::with_shape(shape, [Var::Int(1), Var::string("x")]));
  let wrong_shape = Var::from_table(Table::with_shape(
    Shape::new(["a", "c"]),
    [Var::Int(1), Var::string("x")],
  ));
  let wrong_slot = Var::from_table(Table::with_shape(
    shape,
    [Var::string("x"), Var::string("x")],
  ));
  let map_right = Var::table([("a", Var::Int(1)), ("b", Var::string("x"))]);
  let map_wrong = Var::table([("a", Var::Int(1)), ("c", Var::string("x"))]);
  assert!(ty.admits(&right));
  assert!(!ty.admits(&wrong_shape));
  assert!(ty.admits(&map_right));
  assert!(!ty.admits(&map_wrong));
  // Slot types of a struct value are checked in every build: hosts pass
  // values through `admits`.
  assert!(!ty.admits(&wrong_slot));
  // That none of this interns is checked in `tests/registry.rs`.

  // A host value of the wrong shape is rejected at the mesh boundary, in
  // release builds too.
  let mut mesh = Mesh::new();
  mesh.add_wire(wire(vec![take(val(Var::string("a")))]));
  let compiled = mesh.compile("w", ty).unwrap();
  assert!(mesh.spawn(&compiled, wrong_shape).is_err());
  let id = mesh.spawn(&compiled, right).unwrap();
  mesh.run(10);
  assert_eq!(mesh.take_outcome(id), Some(Outcome::Completed(Var::Int(1))));
}

#[test]
fn shapes_intern_once() {
  // Equal key sets give the same handle (the count is checked by the
  // registry soak, in its own binary: tests here intern concurrently).
  let s = Shape::new(["zeta", "alpha", "mid"]);
  assert_eq!(
    s.keys().iter().map(|k| &**k).collect::<Vec<_>>(),
    ["alpha", "mid", "zeta"]
  );
  assert_eq!(s.index_of("mid"), Some(1));
  assert_eq!(s.index_of("none"), None);
  assert_eq!(s.len(), 3);
  assert_eq!(s, Shape::new(["mid", "zeta", "alpha"]));
  assert_eq!(s.to_string(), "{alpha mid zeta}");
  assert_eq!(Shape::new(["two words"]).to_string(), "{\"two words\"}");
  // The empty literal is a struct table too.
  let empty = Var::table(Vec::<(&str, Var)>::new()).into_struct_tables();
  assert_eq!(
    empty.as_table().unwrap().shape(),
    Some(Shape::new(Vec::<&str>::new()))
  );
  assert_eq!(empty, Var::from_table(Table::new()));
}

#[test]
fn values_built_from_one_shared_value_cost_what_they_hold() {
  // 64 levels, each holding the level below twice: 2^64 tables if walked
  // path by path, 64 sequences and one table as held. Conversion, typing,
  // equality and cache-key hashing each visit what is held. On the device
  // 16 levels (2^16 paths): every level interns its type and shape in the
  // registry, which never frees, and the frontend suite after this one
  // reaches the classic ESP32's lowest free heap.
  let levels = if cfg!(target_os = "espidf") { 16 } else { 64 };
  let build = |leaf: i64| {
    let mut v = Var::table([("a", Var::Int(leaf))]);
    for _ in 0..levels {
      v = Var::from_seq(Arc::new(vec![v.clone(), v]));
    }
    v
  };
  let (v, w) = (build(1), build(1));
  let converted = v.clone().into_struct_tables();
  let mut level = &converted;
  for depth in 0..levels {
    let items = level.as_seq().unwrap();
    if depth < levels - 1 {
      let (Var::Seq(first), Var::Seq(second)) = (&items[0], &items[1]) else {
        panic!("a sequence at depth {depth}");
      };
      assert!(Arc::ptr_eq(first, second), "both copies stay one value");
    }
    level = &items[0];
  }
  assert_eq!(level.as_table().unwrap().shape(), Some(Shape::new(["a"])));
  assert_eq!(converted.type_of(), v.type_of());
  assert!(v.type_of().admits(&w));
  assert!(
    !build(2)
      .type_of()
      .admits(&Var::from_seq(Arc::new(vec![v.clone(), Var::Int(0)])))
  );
  // Separately built, so nothing is shared between the two sides.
  assert_eq!(v, w);
  assert_eq!(converted, w.clone().into_struct_tables());
  assert_ne!(v, build(2));
  let param = |v: &Var| hash_of(&otter_core::ParamValue::Value(v.clone()));
  assert_eq!(param(&v), param(&w));
  // The same with tables holding the level below twice: map tables as
  // literals lower to, compared with each other and with their struct
  // form (a preserving reload compares separately built definitions).
  let tables = |leaf: i64| {
    let mut v = Var::table([("a", Var::Int(leaf))]);
    for _ in 0..levels {
      v = Var::table([("a", v.clone()), ("b", v)]);
    }
    v
  };
  let (v, w) = (tables(1), tables(1));
  assert_eq!(v, w);
  assert_eq!(v.clone().into_struct_tables(), w);
  assert_eq!(w, v.clone().into_struct_tables());
  assert_ne!(v, tables(2));
  assert_ne!(v.clone().into_struct_tables(), tables(2));
}

static SESSION: otter_core::object::ObjectType =
  otter_core::object::ObjectType::new("Test.Session");
static OTHER: otter_core::object::ObjectType = otter_core::object::ObjectType::new("Test.Other");

/// A host object that counts its drops.
struct Session(Arc<std::sync::atomic::AtomicUsize>);

impl otter_core::object::Object for Session {
  fn object_type(&self) -> &'static otter_core::object::ObjectType {
    &SESSION
  }
}

impl Drop for Session {
  fn drop(&mut self) {
    self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
  }
}

struct Other;

impl otter_core::object::Object for Other {
  fn object_type(&self) -> &'static otter_core::object::ObjectType {
    &OTHER
  }
}

#[test]
fn objects_are_values_released_with_their_last_holder() {
  let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
  let dropped = || drops.load(std::sync::atomic::Ordering::SeqCst);
  let session = Var::object(Session(drops.clone()));
  {
    // Held by a mesh variable, read into locals, a sequence and a table.
    let mut mesh = Mesh::new();
    mesh.declare_var("s", session.clone(), true);
    let out = run(
      &mut mesh,
      vec![
        get("s"),
        declare("a"),
        konst(Var::seq(vec![])),
        declare("items"),
        ShardDef::new(
          &otter_core::shards::data::TABLE_MAKE,
          vec![val(Var::seq(vec![Var::string("k")])), var("a")],
        ),
        declare("t"),
        get("a"),
        push("items"),
        get("t"),
      ],
    )
    .unwrap();
    assert_eq!(out.as_table().unwrap().get("k"), Some(&session));
    drop(out);
    assert_eq!(dropped(), 0, "the mesh variable and the host still hold it");
  }
  assert_eq!(dropped(), 0, "the host still holds it");
  drop(session);
  assert_eq!(dropped(), 1, "the last holder released it, once");
}

#[test]
fn objects_compare_hash_and_print_by_identity() {
  let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
  let a = Var::object(Session(drops.clone()));
  let b = Var::object(Session(drops));
  assert_eq!(a, a.clone());
  assert_ne!(a, b);
  assert_eq!(hash_of(&a), hash_of(&a.clone()));
  let (ia, ib) = (a.object_ref().unwrap().id(), b.object_ref().unwrap().id());
  assert!(ib > ia, "ids are assigned in creation order");
  assert_eq!(a.to_string(), format!("Test.Session#{ia}"));
  assert!(a.as_object::<Session>().is_some());
  assert!(a.as_object::<Other>().is_none());
  // Typed by name: admitted by its own type, not another's, and by Any.
  let ty = a.type_of();
  assert_eq!(ty, SESSION.to_type());
  assert_eq!(ty.to_string(), "Test.Session");
  assert!(ty.admits(&a));
  assert!(!OTHER.to_type().admits(&a));
  assert!(!ty.admits(&Var::object(Other)));
  assert!(Type::any().admits(&a));
  assert!(Type::union([ty, Type::int()]).admits(&a));
  assert_ne!(
    Type::union([ty, OTHER.to_type()]),
    ty,
    "two object types stay apart in a union"
  );
}

#[test]
fn an_object_is_never_a_constant() {
  let session = Var::object(Session(Arc::default()));
  for constant in [
    session.clone(),
    Var::seq(vec![Var::Int(1), session.clone()]),
    Var::table([("deep", Var::seq(vec![session.clone()]))]),
  ] {
    assert!(constant.holds_object());
    let err = run(&mut Mesh::new(), vec![konst(constant)]).unwrap_err();
    let otter_core::Error::Diagnostic(d) = err else {
      panic!("expected a diagnostic, got {err:?}")
    };
    assert_eq!(d.code, "object-in-constant", "{}", d.message);
  }
  assert!(!Var::seq(vec![Var::Int(1)]).holds_object());
}
