//! Types: opaque, registry-scoped handles to interned, immutable descriptions
//! (contract §3, docs/values-and-types.md §3-§4). Equality and hashing of
//! handles is structural, because identical descriptions intern to the same
//! handle; sets and tables are canonicalized before interning.
//!
//! Prototype: one process-wide registry that never frees a description.
//! Recursive types are not supported.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, OnceLock, RwLock};

use crate::var::{Seen, Storage, Var};

/// Opaque handle to an interned type description. Cheap to copy and compare.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Type(u32);

/// The description behind a [`Type`]. Nested types refer to other handles,
/// so a description is never recursive.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum TypeDesc {
  None,
  Any,
  Bool,
  Int,
  Float,
  Float2,
  Float3,
  Float4,
  String,
  Bytes,
  Seq(Type),
  Table(TableType),
  /// A union of at least two members, canonical (see [`Type::union`]).
  Union(Vec<Type>),
  /// The output of something that never produces a value (`Stop`, a flow
  /// ending in it). Every type accepts it; it drops out of unions.
  Never,
}

/// A table's shape: known keys with their value types, and the value type of
/// any other key (`None` for a fixed table, which has exactly these keys).
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct TableType {
  /// Sorted by key, keys unique.
  pub keys: Vec<(Arc<str>, Type)>,
  pub rest: Option<Type>,
  /// The interned key shape of a fixed table (`None` with a rest type):
  /// a struct value of this type carries the same handle, so admission is
  /// a handle compare (golden path §7.3).
  pub shape: Option<Shape>,
}

impl TableType {
  /// The type of `key`'s value: a known key, else the rest type.
  pub fn get(&self, key: &str) -> Option<Type> {
    match self.keys.binary_search_by(|(k, _)| (**k).cmp(key)) {
      Ok(i) => Some(self.keys[i].1),
      Err(_) => self.rest,
    }
  }

  /// Whether this is a fixed table (no rest type).
  pub fn is_fixed(&self) -> bool {
    self.rest.is_none()
  }
}

struct Registry {
  descs: Vec<&'static TypeDesc>,
  ids: HashMap<&'static TypeDesc, Type>,
  shapes: Vec<&'static [Arc<str>]>,
  shape_ids: HashMap<&'static [Arc<str>], Shape>,
}

/// The primitive descriptions, interned first and in this order, so their
/// handles are constants: `Type::none()` and friends do no lookup (they
/// are compared on activation hot paths, such as a call's input check).
const PRIMITIVES: [TypeDesc; 11] = [
  TypeDesc::None,
  TypeDesc::Never,
  TypeDesc::Any,
  TypeDesc::Bool,
  TypeDesc::Int,
  TypeDesc::Float,
  TypeDesc::Float2,
  TypeDesc::Float3,
  TypeDesc::Float4,
  TypeDesc::String,
  TypeDesc::Bytes,
];

impl Default for Registry {
  fn default() -> Registry {
    let mut reg = Registry {
      descs: Vec::new(),
      ids: HashMap::new(),
      shapes: Vec::new(),
      shape_ids: HashMap::new(),
    };
    for (i, desc) in PRIMITIVES.iter().enumerate() {
      let desc: &'static TypeDesc = Box::leak(Box::new(desc.clone()));
      reg.descs.push(desc);
      reg.ids.insert(desc, Type(i as u32));
    }
    reg
  }
}

/// An interned sorted key list: the keys of a struct table (golden path
/// §7.3). Equal key sets intern to the same handle, so comparing shapes is
/// comparing handles. Shapes live in the type registry, which never frees.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Shape(u32);

impl Shape {
  /// The shape of these keys, in any order; a repeated key panics (a bug
  /// in the caller, like a duplicate field in a struct literal).
  pub fn new<K: Into<Arc<str>>>(keys: impl IntoIterator<Item = K>) -> Shape {
    let mut keys: Vec<Arc<str>> = keys.into_iter().map(Into::into).collect();
    keys.sort();
    if let Some(w) = keys.windows(2).find(|w| w[0] == w[1]) {
      panic!("duplicate table key {}", w[0]);
    }
    Shape::intern_sorted(keys)
  }

  /// Interns `keys`, already sorted and unique.
  fn intern_sorted(keys: Vec<Arc<str>>) -> Shape {
    if let Some(shape) = registry()
      .read()
      .expect("type registry poisoned")
      .shape_ids
      .get(&keys[..])
    {
      return *shape;
    }
    let mut reg = registry().write().expect("type registry poisoned");
    if let Some(shape) = reg.shape_ids.get(&keys[..]) {
      return *shape;
    }
    let shape = Shape(u32::try_from(reg.shapes.len()).expect("too many shapes"));
    let keys: &'static [Arc<str>] = Box::leak(keys.into_boxed_slice());
    reg.shapes.push(keys);
    reg.shape_ids.insert(keys, shape);
    shape
  }

  /// How many shapes the process has interned (never freed).
  pub fn registered() -> usize {
    registry()
      .read()
      .expect("type registry poisoned")
      .shapes
      .len()
  }

  /// The keys, sorted.
  pub fn keys(self) -> &'static [Arc<str>] {
    registry().read().expect("type registry poisoned").shapes[self.0 as usize]
  }

  pub fn len(self) -> usize {
    self.keys().len()
  }

  pub fn is_empty(self) -> bool {
    self.keys().is_empty()
  }

  /// The slot of `key`, if the shape has it.
  pub fn index_of(self, key: &str) -> Option<usize> {
    crate::var::search(self.keys(), key).ok()
  }
}

impl fmt::Display for Shape {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{{")?;
    for (i, k) in self.keys().iter().enumerate() {
      if i > 0 {
        write!(f, " ")?;
      }
      write!(f, "{}", key_text(k))?;
    }
    write!(f, "}}")
  }
}

impl fmt::Debug for Shape {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "Shape#{} {self}", self.0)
  }
}

fn registry() -> &'static RwLock<Registry> {
  static REGISTRY: OnceLock<RwLock<Registry>> = OnceLock::new();
  REGISTRY.get_or_init(Default::default)
}

impl Type {
  /// Whether a value of this type never owns heap storage (none, a
  /// boolean, a number or a float vector): dropping a stale one releases
  /// nothing.
  pub(crate) fn is_scalar(self) -> bool {
    // The primitive handles, in `PRIMITIVES` order: all but Any and String.
    matches!(self.0, 0 | 1 | 3..=8)
  }

  /// Returns the handle for `desc`, interning it if needed. Callers build
  /// sets and tables through [`Type::set`] and [`Type::table`], which
  /// canonicalize them first.
  fn intern(desc: TypeDesc) -> Type {
    if let Some(ty) = registry()
      .read()
      .expect("type registry poisoned")
      .ids
      .get(&desc)
    {
      return *ty;
    }
    let mut reg = registry().write().expect("type registry poisoned");
    if let Some(ty) = reg.ids.get(&desc) {
      return *ty;
    }
    let ty = Type(u32::try_from(reg.descs.len()).expect("too many types"));
    // Descriptions live for the process (the registry never frees them),
    // so readers get a reference instead of a copy made under the lock.
    let desc: &'static TypeDesc = Box::leak(Box::new(desc));
    reg.descs.push(desc);
    reg.ids.insert(desc, ty);
    ty
  }

  /// How many type descriptions the process has interned (they are never
  /// freed): for diagnostics and tests that check nothing interns per value.
  pub fn registered() -> usize {
    registry()
      .read()
      .expect("type registry poisoned")
      .descs
      .len()
  }

  pub fn desc(self) -> &'static TypeDesc {
    registry().read().expect("type registry poisoned").descs[self.0 as usize]
  }

  // The primitives are constants (see `PRIMITIVES`): no registry access.
  pub const fn none() -> Type {
    Type(0)
  }
  pub const fn never() -> Type {
    Type(1)
  }
  pub const fn any() -> Type {
    Type(2)
  }
  pub const fn bool() -> Type {
    Type(3)
  }
  pub const fn int() -> Type {
    Type(4)
  }
  pub const fn float() -> Type {
    Type(5)
  }
  pub const fn float2() -> Type {
    Type(6)
  }
  pub const fn float3() -> Type {
    Type(7)
  }
  pub const fn float4() -> Type {
    Type(8)
  }
  pub const fn string() -> Type {
    Type(9)
  }
  pub const fn bytes() -> Type {
    Type(10)
  }
  pub fn seq(inner: Type) -> Type {
    Type::intern(TypeDesc::Seq(inner))
  }

  /// A table type. Keys are sorted; a repeated key panics (a bug in the
  /// caller, like a duplicate field in a struct literal).
  pub fn table<K: Into<Arc<str>>>(
    keys: impl IntoIterator<Item = (K, Type)>,
    rest: Option<Type>,
  ) -> Type {
    let mut keys: Vec<(Arc<str>, Type)> = keys.into_iter().map(|(k, t)| (k.into(), t)).collect();
    keys.sort_by(|a, b| a.0.cmp(&b.0));
    if let Some(w) = keys.windows(2).find(|w| w[0].0 == w[1].0) {
      panic!("duplicate table key {}", w[0].0);
    }
    let shape = rest
      .is_none()
      .then(|| Shape::intern_sorted(keys.iter().map(|(k, _)| k.clone()).collect()));
    Type::intern(TypeDesc::Table(TableType { keys, rest, shape }))
  }

  /// The fixed table of `shape` with one value type per key, in key order.
  pub fn fixed_table_of(shape: Shape, types: impl IntoIterator<Item = Type>) -> Type {
    let keys: Vec<(Arc<str>, Type)> = shape.keys().iter().cloned().zip(types).collect();
    assert_eq!(keys.len(), shape.len(), "one type per key of {shape}");
    Type::intern(TypeDesc::Table(TableType {
      keys,
      rest: None,
      shape: Some(shape),
    }))
  }

  /// A fixed table: exactly these keys.
  pub fn fixed_table<K: Into<Arc<str>>>(keys: impl IntoIterator<Item = (K, Type)>) -> Type {
    Type::table(keys, None)
  }

  /// Any table: no known keys, any value type.
  pub fn any_table() -> Type {
    Type::table(Vec::<(Arc<str>, Type)>::new(), Some(Type::any()))
  }

  /// The union of `members`, canonical: nested unions are flattened,
  /// members sorted structurally ([`structural_cmp`]: stable across runs,
  /// unlike handle indices, and unambiguous, unlike printed forms) and
  /// deduplicated; one member is that member, a union with `Any` is `Any`,
  /// and `Never` members drop out (no members at all is `Never`).
  pub fn union(members: impl IntoIterator<Item = Type>) -> Type {
    let mut flat = Vec::new();
    for m in members {
      match m.desc() {
        TypeDesc::Union(inner) => flat.extend(inner.iter().copied()),
        TypeDesc::Any => return Type::any(),
        // A branch that never produces a value adds nothing.
        TypeDesc::Never => {}
        _ => flat.push(m),
      }
    }
    flat.sort_by(|a, b| structural_cmp(*a, *b));
    flat.dedup();
    match flat.len() {
      0 => Type::never(),
      1 => flat[0],
      _ => Type::intern(TypeDesc::Union(flat)),
    }
  }

  /// The table shape, if this is a table type.
  pub fn as_table(self) -> Option<&'static TableType> {
    match self.desc() {
      TypeDesc::Table(t) => Some(t),
      _ => None,
    }
  }

  /// Whether a value fits this type, checked on the value itself: nothing is
  /// interned, so this is safe on every activation (unlike comparing
  /// `value.type_of()`). An empty sequence fits every sequence type. A
  /// sequence or table held in several places is checked once per type.
  pub fn admits(self, value: &Var) -> bool {
    Admits::default().check(self, value)
  }

  /// Whether a value of type `actual` is acceptable where `self` is
  /// expected (docs/values-and-types.md §3.3). `Any` accepts everything,
  /// but an `Any` actual is accepted only by `Any`.
  pub fn accepts(self, actual: Type) -> bool {
    if self == actual {
      return true;
    }
    match (self.desc(), actual.desc()) {
      (TypeDesc::Any, _) | (_, TypeDesc::Never) => true,
      (_, TypeDesc::Union(members)) => members.iter().all(|m| self.accepts(*m)),
      (TypeDesc::Union(members), _) => members.iter().any(|m| m.accepts(actual)),
      (TypeDesc::Seq(e), TypeDesc::Seq(a)) => e.accepts(*a),
      (TypeDesc::Table(e), TypeDesc::Table(a)) => table_accepts(e, a),
      _ => false,
    }
  }
}

/// A total order on types by structure, used to canonicalize unions. It
/// does not depend on handle indices (registration order) or printed forms
/// (which can coincide for distinct tables). `None` sorts last, so unions
/// read `Int | None`.
pub fn structural_cmp(a: Type, b: Type) -> std::cmp::Ordering {
  use std::cmp::Ordering;
  if a == b {
    return Ordering::Equal;
  }
  fn rank(d: &TypeDesc) -> u8 {
    match d {
      TypeDesc::Any => 0,
      TypeDesc::Bool => 1,
      TypeDesc::Int => 2,
      TypeDesc::Float => 3,
      TypeDesc::Float2 => 4,
      TypeDesc::Float3 => 5,
      TypeDesc::Float4 => 6,
      TypeDesc::String => 7,
      TypeDesc::Seq(_) => 8,
      TypeDesc::Table(_) => 9,
      TypeDesc::Union(_) => 10,
      TypeDesc::None => 11,
      TypeDesc::Never => 12,
      TypeDesc::Bytes => 13,
    }
  }
  let (da, db) = (a.desc(), b.desc());
  rank(da).cmp(&rank(db)).then_with(|| match (da, db) {
    (TypeDesc::Seq(x), TypeDesc::Seq(y)) => structural_cmp(*x, *y),
    (TypeDesc::Table(x), TypeDesc::Table(y)) => {
      let keys = x
        .keys
        .iter()
        .zip(&y.keys)
        .fold(Ordering::Equal, |acc, ((ka, ta), (kb, tb))| {
          acc
            .then_with(|| ka.cmp(kb))
            .then_with(|| structural_cmp(*ta, *tb))
        });
      keys
        .then_with(|| x.keys.len().cmp(&y.keys.len()))
        .then_with(|| match (x.rest, y.rest) {
          (None, None) => Ordering::Equal,
          (None, Some(_)) => Ordering::Less,
          (Some(_), None) => Ordering::Greater,
          (Some(p), Some(q)) => structural_cmp(p, q),
        })
    }
    (TypeDesc::Union(x), TypeDesc::Union(y)) => x
      .iter()
      .zip(y)
      .fold(Ordering::Equal, |acc, (p, q)| {
        acc.then_with(|| structural_cmp(*p, *q))
      })
      .then_with(|| x.len().cmp(&y.len())),
    _ => Ordering::Equal,
  })
}

/// A table key as written in source and messages: plain names as they are,
/// anything else quoted (`{a: 1 "two words": 2}`), so printing is
/// unambiguous and round-trips.
pub(crate) fn key_text(key: &str) -> std::borrow::Cow<'_, str> {
  let mut chars = key.chars();
  let plain = chars
    .next()
    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
    && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
  if plain {
    std::borrow::Cow::Borrowed(key)
  } else {
    std::borrow::Cow::Owned(format!("{key:?}"))
  }
}

fn table_accepts(expected: &TableType, actual: &TableType) -> bool {
  // Every expected key must be present (a key only covered by the actual
  // rest type may be missing at runtime).
  let keys_present = expected.keys.iter().all(|(k, et)| {
    actual
      .keys
      .binary_search_by(|(ak, _)| ak.cmp(k))
      .is_ok_and(|i| et.accepts(actual.keys[i].1))
  });
  let others_accepted =
    actual.keys.iter().all(
      |(k, at)| match expected.keys.binary_search_by(|(ek, _)| ek.cmp(k)) {
        Ok(_) => true,
        Err(_) => expected.rest.is_some_and(|r| r.accepts(*at)),
      },
    );
  let rest_accepted = match actual.rest {
    None => true,
    Some(ar) => expected.rest.is_some_and(|r| r.accepts(ar)),
  };
  keys_present && others_accepted && rest_accepted
}

/// Prints a nested type, parenthesizing a set so `[(Int | None)]` reads
/// unambiguously.
struct Nested(Type);

impl fmt::Display for Nested {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self.0.desc() {
      TypeDesc::Union(_) => write!(f, "({})", self.0),
      _ => write!(f, "{}", self.0),
    }
  }
}

impl fmt::Display for Type {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self.desc() {
      TypeDesc::None => write!(f, "None"),
      TypeDesc::Never => write!(f, "Never"),
      TypeDesc::Any => write!(f, "Any"),
      TypeDesc::Bool => write!(f, "Bool"),
      TypeDesc::Int => write!(f, "Int"),
      TypeDesc::Float => write!(f, "Float"),
      TypeDesc::Float2 => write!(f, "Float2"),
      TypeDesc::Float3 => write!(f, "Float3"),
      TypeDesc::Float4 => write!(f, "Float4"),
      TypeDesc::String => write!(f, "String"),
      TypeDesc::Bytes => write!(f, "Bytes"),
      TypeDesc::Seq(inner) => write!(f, "[{}]", Nested(*inner)),
      TypeDesc::Table(t) => {
        write!(f, "{{")?;
        let mut first = true;
        for (k, ty) in &t.keys {
          if !first {
            write!(f, " ")?;
          }
          first = false;
          write!(f, "{}: {}", key_text(k), Nested(*ty))?;
        }
        if let Some(rest) = t.rest {
          if !first {
            write!(f, " ")?;
          }
          // An open table of anything reads `{a: Int ...}`.
          if rest == Type::any() {
            write!(f, "...")?;
          } else {
            write!(f, "...: {}", Nested(rest))?;
          }
        }
        write!(f, "}}")
      }
      TypeDesc::Union(members) => {
        for (i, m) in members.iter().enumerate() {
          if i > 0 {
            write!(f, " | ")?;
          }
          write!(f, "{}", Nested(*m))?;
        }
        Ok(())
      }
    }
  }
}

impl fmt::Debug for Type {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "Type({self})")
  }
}

/// One `admits` check: the result for each shared sequence or table already
/// checked against a type (see [`crate::var::Storage`]).
#[derive(Default)]
struct Admits(Seen<(Type, Storage), bool>);

impl Admits {
  /// A map table against a table type: every key the type lists is there
  /// and fits, and every other key fits the type's rest.
  fn map_table(&mut self, t: &TableType, entries: &crate::var::Table) -> bool {
    for (k, kt) in t.keys.iter() {
      match entries.get(k) {
        Some(v) if self.check(*kt, v) => {}
        _ => return false,
      }
    }
    for (k, v) in entries.iter() {
      if crate::var::search(&t.keys, k).is_err() {
        match t.rest {
          Some(r) if self.check(r, v) => {}
          _ => return false,
        }
      }
    }
    true
  }

  fn check(&mut self, ty: Type, value: &Var) -> bool {
    let key = match value.storage() {
      Some((storage, true)) => Some((ty, storage)),
      _ => None,
    };
    if let Some(key) = &key
      && let Some(&known) = self.0.get(key)
    {
      return known;
    }
    let admitted = match (ty.desc(), value) {
      (TypeDesc::Any, _) => true,
      (TypeDesc::Union(members), _) => {
        let mut any = false;
        for m in members.iter() {
          if self.check(*m, value) {
            any = true;
            break;
          }
        }
        any
      }
      (TypeDesc::None, Var::None)
      | (TypeDesc::Bool, Var::Bool(_))
      | (TypeDesc::Int, Var::Int(_))
      | (TypeDesc::Float, Var::Float(_))
      | (TypeDesc::Float2, Var::Float2(_))
      | (TypeDesc::Float3, Var::Float3(_))
      | (TypeDesc::Float4, Var::Float4(_))
      | (TypeDesc::String, Var::String(_))
      | (TypeDesc::Bytes, Var::Bytes(_)) => true,
      (TypeDesc::Seq(e), Var::Seq(items)) => {
        let mut all = true;
        for v in items.iter() {
          if !self.check(*e, v) {
            all = false;
            break;
          }
        }
        all
      }
      (TypeDesc::Table(t), Var::Table(entries)) => {
        if let (Some(expected), Some(actual)) = (t.shape, entries.shape()) {
          // A struct value of a fixed type: the shape handle says whether
          // the keys match (no lookups), then each slot is checked. The
          // slot check is unconditional: `set_var` and `spawn` admit host
          // values with this, in release too.
          if expected != actual {
            false
          } else {
            let mut all = true;
            for ((_, kt), v) in t.keys.iter().zip(entries.values()) {
              if !self.check(*kt, v) {
                all = false;
                break;
              }
            }
            all
          }
        } else {
          self.map_table(t, entries)
        }
      }
      _ => false,
    };
    if let Some(key) = key {
      self.0.insert(key, admitted);
    }
    admitted
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn identical_descriptions_intern_to_the_same_handle() {
    assert_eq!(Type::seq(Type::int()), Type::seq(Type::int()));
    assert_ne!(Type::seq(Type::int()), Type::seq(Type::float()));
    assert_eq!(Type::seq(Type::int()).to_string(), "[Int]");
  }

  #[test]
  fn sets_are_canonical() {
    let a = Type::union([Type::int(), Type::none()]);
    let b = Type::union([Type::none(), Type::int(), Type::int()]);
    assert_eq!(a, b);
    assert_eq!(a.to_string(), "Int | None");
    // Nested sets flatten; one member is that member; Any absorbs.
    assert_eq!(
      Type::union([a, Type::float()]),
      Type::union([Type::float(), Type::int(), Type::none()])
    );
    assert_eq!(Type::union([Type::int()]), Type::int());
    assert_eq!(Type::union([Type::int(), Type::any()]), Type::any());
    assert_eq!(Type::seq(a).to_string(), "[(Int | None)]");
  }

  #[test]
  fn tables_are_canonical_and_print_in_key_order() {
    let a = Type::fixed_table([("b", Type::int()), ("a", Type::string())]);
    let b = Type::fixed_table([("a", Type::string()), ("b", Type::int())]);
    assert_eq!(a, b);
    assert_eq!(a.to_string(), "{a: String b: Int}");
    assert_eq!(Type::any_table().to_string(), "{...}");
    assert_eq!(
      Type::table([("a", Type::int())], Some(Type::any())).to_string(),
      "{a: Int ...}"
    );
    assert_eq!(
      Type::table([("a", Type::int())], Some(Type::float())).to_string(),
      "{a: Int ...: Float}"
    );
    let t = a.as_table().unwrap();
    assert_eq!(t.get("b"), Some(Type::int()));
    assert_eq!(t.get("c"), None);
    assert!(t.is_fixed());
  }

  #[test]
  fn unions_are_order_independent_even_when_printed_forms_coincide() {
    // Distinct tables that printed alike before keys were quoted.
    let two_keys = Type::fixed_table([("a", Type::int()), ("b", Type::float())]);
    let one_key = Type::fixed_table([("a: Int b", Type::float())]);
    assert_ne!(two_keys, one_key);
    assert_ne!(two_keys.to_string(), one_key.to_string());
    assert_eq!(one_key.to_string(), "{\"a: Int b\": Float}");
    let ab = Type::union([two_keys, one_key]);
    assert_eq!(ab, Type::union([one_key, two_keys]));
    assert_eq!(Type::union([two_keys, one_key, two_keys]), ab);
    let TypeDesc::Union(members) = ab.desc() else {
      panic!("a union")
    };
    assert_eq!(members.len(), 2);
    // A key named like the rest marker is quoted, not confused with it.
    let dots = Type::fixed_table([("...", Type::int())]);
    assert_eq!(dots.to_string(), "{\"...\": Int}");
    assert_ne!(
      dots.to_string(),
      Type::table(Vec::<(&str, Type)>::new(), Some(Type::int())).to_string()
    );
  }

  #[test]
  fn admits_follows_the_type() {
    let maybe = Type::union([Type::int(), Type::none()]);
    assert!(maybe.admits(&Var::None) && maybe.admits(&Var::Int(1)));
    assert!(!maybe.admits(&Var::Float(1.0)));
    assert!(Type::seq(Type::int()).admits(&Var::Seq(Default::default())));
    let fixed = Type::fixed_table([("a", Type::int())]);
    assert!(fixed.admits(&Var::table([("a", Var::Int(1))])));
    assert!(!fixed.admits(&Var::table([("a", Var::Int(1)), ("b", Var::Int(2))])));
  }

  #[test]
  #[should_panic(expected = "duplicate table key a")]
  fn a_repeated_table_key_panics() {
    Type::fixed_table([("a", Type::int()), ("a", Type::float())]);
  }

  #[test]
  #[should_panic(expected = "duplicate table key a")]
  fn a_repeated_shape_key_panics() {
    Shape::new(["a", "b", "a"]);
  }

  #[test]
  fn acceptance_follows_the_documented_rules() {
    let int_or_none = Type::union([Type::int(), Type::none()]);
    // Any accepts everything; an Any actual only fits Any.
    assert!(Type::any().accepts(int_or_none));
    assert!(!Type::int().accepts(Type::any()));
    // Sets: some expected member, every actual member.
    assert!(int_or_none.accepts(Type::int()));
    assert!(int_or_none.accepts(Type::none()));
    assert!(!Type::int().accepts(int_or_none));
    assert!(!int_or_none.accepts(Type::float()));
    // Sequences are covariant.
    assert!(Type::seq(int_or_none).accepts(Type::seq(Type::int())));
    assert!(!Type::seq(Type::int()).accepts(Type::seq(int_or_none)));

    let pose = Type::fixed_table([("x", Type::float()), ("y", Type::float())]);
    // Any table accepts a fixed table; a fixed table accepts only its keys.
    assert!(Type::any_table().accepts(pose));
    assert!(pose.accepts(pose));
    let extra = Type::fixed_table([
      ("x", Type::float()),
      ("y", Type::float()),
      ("z", Type::float()),
    ]);
    assert!(!pose.accepts(extra), "a fixed table rejects an unknown key");
    let missing = Type::fixed_table([("x", Type::float())]);
    assert!(!pose.accepts(missing), "every expected key must be present");
    // A key covered only by the actual rest type may be missing.
    let open = Type::table([("x", Type::float())], Some(Type::float()));
    assert!(!pose.accepts(open));
    // An open expected table checks other keys against its rest type.
    let floats = Type::table(Vec::<(&str, Type)>::new(), Some(Type::float()));
    assert!(floats.accepts(pose));
    assert!(!floats.accepts(Type::fixed_table([("s", Type::string())])));
    assert!(
      !floats.accepts(Type::any_table()),
      "a rest of Any is not a rest of Float"
    );
    // Value types inside a table follow the same rule.
    let maybe = Type::fixed_table([("x", int_or_none)]);
    assert!(maybe.accepts(Type::fixed_table([("x", Type::none())])));
    assert!(!Type::fixed_table([("x", Type::int())]).accepts(maybe));
  }
}
