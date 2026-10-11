//! The type registry never frees descriptions, so nothing may intern a type
//! per runtime value. Its own test binary: other tests intern concurrently.

use otter_core::{Shape, Table, Type, Var};

#[test]
fn checking_values_against_types_interns_nothing() {
  // Intern what the checks use first.
  let (any_table, int) = (Type::any_table(), Type::int());
  let fixed = Type::fixed_table([("a", Type::int())]);
  let (other, shape) = (Shape::new(["b"]), fixed.as_table().unwrap().shape.unwrap());
  let before = (Type::registered(), Shape::registered());
  for i in 0..100 {
    let v = Var::table([(format!("key{i}"), Var::Int(i))]);
    assert!(any_table.admits(&v));
    assert!(!int.admits(&v));
    // Struct values of a fixed type: a handle compare either way.
    assert!(fixed.admits(&Var::from_table(Table::with_shape(shape, [Var::Int(i)]))));
    assert!(!fixed.admits(&Var::from_table(Table::with_shape(other, [Var::Int(i)]))));
    assert!(fixed.admits(&Var::table([("a", Var::Int(i))])));
    assert!(!fixed.admits(&Var::from_table(Table::with_shape(
      shape,
      [Var::Float(1.0)]
    ))));
  }
  assert_eq!((Type::registered(), Shape::registered()), before);
}
