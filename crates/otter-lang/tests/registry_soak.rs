//! Golden path §7.4: shapes join the type registry, which never frees. This
//! soak measures how much a thousand reloads, each introducing a table of
//! new keys, grow it: linear in the shapes introduced, nothing per
//! activation. Its own test binary, so other tests' interning does not
//! land in its counts (they still run concurrently in CI, so the bounds
//! leave room).

use std::collections::HashMap;

use otter_core::{Catalog, Shape, Type};
use otter_lang::{Session, Source};

#[test]
fn a_thousand_reloads_with_distinct_shapes_grow_the_registry_linearly() {
  let catalog = Catalog::new(&[otter_core::shards::CATALOG]).unwrap();
  let defines = HashMap::new();
  let mut session = Session::new();
  let program = |i: usize| {
    format!(
      "@wire(main {{ {{k{i}: {i} v: 1 nested: {{n{i}: 2}}}} = t  t.v | Math.Add(t.nested.n{i}) = s  t.k{i} | Math.Add(s) | Log }} looped: true)\n@mesh(m) @schedule(m main) @run(m)"
    )
  };
  // Warm up: the base types and the first program's shapes.
  session
    .reload(Source::new("soak.shs", program(0)), &catalog, &defines)
    .unwrap_or_else(|(_, d)| panic!("{d:?}"));
  session.tick();
  let (types, shapes) = (Type::registered(), Shape::registered());
  const RELOADS: usize = 1000;
  for i in 1..=RELOADS {
    session
      .reload(Source::new("soak.shs", program(i)), &catalog, &defines)
      .unwrap_or_else(|(_, d)| panic!("reload {i}: {d:?}"));
    session.tick();
    session.tick();
  }
  let (type_growth, shape_growth) = (Type::registered() - types, Shape::registered() - shapes);
  // Each program introduces two new shapes (`{kN v nested}` and `{nN}`)
  // and the types built on them; ticks add nothing.
  println!("registry growth over {RELOADS} reloads: {type_growth} types, {shape_growth} shapes");
  assert_eq!(shape_growth, 2 * RELOADS);
  assert!(
    (2 * RELOADS..=6 * RELOADS).contains(&type_growth),
    "{type_growth} types"
  );
  // Reloading the same program again interns nothing.
  let (types, shapes) = (Type::registered(), Shape::registered());
  session
    .reload(
      Source::new("soak.shs", program(RELOADS)),
      &catalog,
      &defines,
    )
    .unwrap_or_else(|(_, d)| panic!("{d:?}"));
  session.tick();
  assert_eq!((Type::registered(), Shape::registered()), (types, shapes));
}
