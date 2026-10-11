//! Values (contract §9, docs/values-and-types.md §2, golden path §7).
//!
//! `Var` is 32 bytes aligned to 16, as 1.x's `SHVar` (what `Float4` needs;
//! alignment 32 measured the same in time and cost a third more in every
//! frame, instruction and step that holds a value, see `bench/values`). The
//! explicit `u8` tag keeps the layout defined (RFC 2195): the tag at offset
//! 0, `Float4` at offset 16, and the table payload after the tag.
//! `tests/values.rs` pins these.

use std::fmt;
use std::hash::{Hash, Hasher};
use std::mem::ManuallyDrop;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use crate::types::{Shape, Type};

#[derive(Default)]
#[repr(u8, align(16))]
pub enum Var {
  #[default]
  None,
  Bool(bool),
  Int(i64),
  Float(f64),
  Float2(Float2),
  Float3(Float3),
  Float4(Float4),
  /// The variants holding shared storage come last (`is_plain`), and
  /// their storage is released by `Var`'s own drop (see `Drop for Var`):
  /// build them with [`Var::from_string`], [`Var::seq`] and the like.
  String(ManuallyDrop<Arc<str>>),
  Seq(ManuallyDrop<Arc<Vec<Var>>>),
  /// String keys in sorted order (docs/values-and-types.md §2).
  Table(ManuallyDrop<Table>),
  /// Raw bytes (a file read with `@read(... bytes: true)`, binary data).
  Bytes(ManuallyDrop<Arc<[u8]>>),
}

/// 1.x's `destroyVar`: a plain value needs nothing, inline; anything else
/// releases its storage out of line (`drop_storage`, its
/// `destroyVarSlow`). Written by hand so a value's drop is this small
/// wherever it happens (the VM, the engine, a sequence freeing its
/// elements), however many variants hold storage: a derived drop grows
/// with every one, and past the compiler's inlining budget every drop of a
/// number became a call (adding `Bytes` made the VM suite 10 to 88 percent
/// slower, 2026-10-09).
impl Drop for Var {
  #[inline(always)]
  fn drop(&mut self) {
    if !self.is_plain() {
      drop_storage(self);
    }
  }
}

#[inline(never)]
fn drop_storage(value: &mut Var) {
  // SAFETY: the storage is dropped once, here, as the value goes away; the
  // payload's own drop is a no-op (`ManuallyDrop`).
  unsafe {
    match value {
      Var::String(s) => ManuallyDrop::drop(s),
      Var::Seq(items) => ManuallyDrop::drop(items),
      Var::Table(table) => ManuallyDrop::drop(table),
      Var::Bytes(bytes) => ManuallyDrop::drop(bytes),
      _ => {}
    }
  }
}

/// As derived, without the storage wrappers: `Int(1)`, `String("a")`.
impl fmt::Debug for Var {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Var::None => f.write_str("None"),
      Var::Bool(v) => f.debug_tuple("Bool").field(v).finish(),
      Var::Int(v) => f.debug_tuple("Int").field(v).finish(),
      Var::Float(v) => f.debug_tuple("Float").field(v).finish(),
      Var::Float2(v) => f.debug_tuple("Float2").field(v).finish(),
      Var::Float3(v) => f.debug_tuple("Float3").field(v).finish(),
      Var::Float4(v) => f.debug_tuple("Float4").field(v).finish(),
      Var::String(v) => f.debug_tuple("String").field(&&***v).finish(),
      Var::Seq(v) => f.debug_tuple("Seq").field(&***v).finish(),
      Var::Table(v) => f.debug_tuple("Table").field(&**v).finish(),
      Var::Bytes(v) => f.debug_tuple("Bytes").field(&&***v).finish(),
    }
  }
}

/// A plain value is copied bit for bit in place; anything else (a reference
/// count) out of line, explicitly: a derived clone grows with every variant
/// until the compiler stops inlining it, and then every copy of a number
/// is a call (`copy`).
impl Clone for Var {
  #[inline(always)]
  fn clone(&self) -> Var {
    copy(self)
  }
}

/// Two 32-bit floats. Derefs to the array; `Var::Float2(v)` reads `v[0]`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[repr(transparent)]
pub struct Float2(pub [f32; 2]);

/// Three 32-bit floats.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[repr(transparent)]
pub struct Float3(pub [f32; 3]);

/// Four 32-bit floats, aligned for packed arithmetic.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[repr(C, align(16))]
pub struct Float4(pub [f32; 4]);

macro_rules! vector_payload {
  ($name:ident, $n:literal, $variant:ident) => {
    impl Deref for $name {
      type Target = [f32; $n];
      fn deref(&self) -> &[f32; $n] {
        &self.0
      }
    }
    impl DerefMut for $name {
      fn deref_mut(&mut self) -> &mut [f32; $n] {
        &mut self.0
      }
    }
    impl From<[f32; $n]> for $name {
      fn from(c: [f32; $n]) -> $name {
        $name(c)
      }
    }
    impl From<$name> for [f32; $n] {
      fn from(v: $name) -> [f32; $n] {
        v.0
      }
    }
    impl From<[f32; $n]> for Var {
      fn from(c: [f32; $n]) -> Var {
        Var::$variant($name(c))
      }
    }
    impl From<$name> for Var {
      fn from(v: $name) -> Var {
        Var::$variant(v)
      }
    }
  };
}

vector_payload!(Float2, 2, Float2);
vector_payload!(Float3, 3, Float3);
vector_payload!(Float4, 4, Float4);

impl Var {
  pub fn string(s: &str) -> Var {
    Var::from_string(Arc::from(s))
  }

  pub fn bytes(b: &[u8]) -> Var {
    Var::from_bytes(Arc::from(b))
  }

  pub fn from_string(s: Arc<str>) -> Var {
    Var::String(ManuallyDrop::new(s))
  }

  pub fn from_bytes(b: Arc<[u8]>) -> Var {
    Var::Bytes(ManuallyDrop::new(b))
  }

  /// A sequence of these values.
  pub fn seq(items: Vec<Var>) -> Var {
    Var::from_seq(Arc::new(items))
  }

  pub fn from_seq(items: Arc<Vec<Var>>) -> Var {
    Var::Seq(ManuallyDrop::new(items))
  }

  pub fn from_table(table: Table) -> Var {
    Var::Table(ManuallyDrop::new(table))
  }

  /// The sequence's storage, taken out of the value; the value itself when
  /// it is not a sequence.
  pub fn into_seq(self) -> std::result::Result<Arc<Vec<Var>>, Var> {
    match self {
      Var::Seq(_) => {
        let mut this = ManuallyDrop::new(self);
        let Var::Seq(items) = &mut *this else {
          unreachable!()
        };
        // SAFETY: `this` is never dropped, so the storage is taken once.
        Ok(unsafe { ManuallyDrop::take(items) })
      }
      other => Err(other),
    }
  }

  /// The table, taken out of the value; the value itself when it is not a
  /// table.
  pub fn into_table(self) -> std::result::Result<Table, Var> {
    match self {
      Var::Table(_) => {
        let mut this = ManuallyDrop::new(self);
        let Var::Table(table) = &mut *this else {
          unreachable!()
        };
        // SAFETY: `this` is never dropped, so the table is taken once.
        Ok(unsafe { ManuallyDrop::take(table) })
      }
      other => Err(other),
    }
  }

  pub fn float2(x: f32, y: f32) -> Var {
    Var::Float2(Float2([x, y]))
  }

  pub fn float3(x: f32, y: f32, z: f32) -> Var {
    Var::Float3(Float3([x, y, z]))
  }

  pub fn float4(x: f32, y: f32, z: f32, w: f32) -> Var {
    Var::Float4(Float4([x, y, z, w]))
  }

  /// A table from key/value pairs; a repeated key keeps the last value.
  pub fn table<K: Into<Arc<str>>>(entries: impl IntoIterator<Item = (K, Var)>) -> Var {
    Var::from_table(entries.into_iter().collect())
  }

  /// The table, if this is one.
  pub fn as_table(&self) -> Option<&Table> {
    match self {
      Var::Table(t) => Some(t),
      _ => None,
    }
  }

  /// The elements, if this is a sequence.
  pub fn as_seq(&self) -> Option<&[Var]> {
    match self {
      Var::Seq(items) => Some(items),
      _ => None,
    }
  }

  /// Whether the value holds no shared storage (the variants before
  /// `String`): dropping it frees nothing, and a bitwise copy of it is a
  /// whole, independent value. One compare of the tag.
  #[inline(always)]
  pub(crate) fn is_plain(&self) -> bool {
    matches!(
      self,
      Var::None
        | Var::Bool(_)
        | Var::Int(_)
        | Var::Float(_)
        | Var::Float2(_)
        | Var::Float3(_)
        | Var::Float4(_)
    )
  }

  /// The text, if this is a string.
  pub fn as_str(&self) -> Option<&str> {
    match self {
      Var::String(s) => Some(s),
      _ => None,
    }
  }

  /// The bytes, if this is a byte string.
  pub fn as_bytes(&self) -> Option<&[u8]> {
    match self {
      Var::Bytes(b) => Some(b),
      _ => None,
    }
  }

  /// The value's type. A table is the fixed table of its keys; a sequence
  /// of mixed element types is a sequence of their union. A sequence or
  /// table held in several places is typed once.
  pub fn type_of(&self) -> Type {
    Types::default().of(self)
  }

  /// How many nodes [`Var::hash_prefix`] hashes at most.
  pub const HASHED_NODES: usize = 64;

  /// Hashes the value's first [`Var::HASHED_NODES`] nodes (in order, each
  /// sequence and table with its length): consistent with equality, and
  /// bounded however large the value, or however many times it holds one
  /// shared value.
  pub fn hash_prefix<H: Hasher>(&self, state: &mut H) {
    let mut left = Self::HASHED_NODES;
    self.hash_nodes(state, &mut left);
  }

  fn hash_nodes<H: Hasher>(&self, state: &mut H, left: &mut usize) {
    if *left == 0 {
      return;
    }
    *left -= 1;
    match self {
      Var::Seq(items) => {
        std::mem::discriminant(self).hash(state);
        items.len().hash(state);
        for item in items.iter() {
          if *left == 0 {
            break;
          }
          item.hash_nodes(state, left);
        }
      }
      Var::Table(table) => {
        std::mem::discriminant(self).hash(state);
        table.len().hash(state);
        for (key, value) in table.iter() {
          if *left == 0 {
            break;
          }
          key.hash(state);
          value.hash_nodes(state, left);
        }
      }
      scalar => scalar.hash(state),
    }
  }

  /// Where a sequence's or table's contents are stored, and whether
  /// another value shares that storage; `None` for other values. Lets one
  /// walk over a value visit storage it holds in many places once (see
  /// [`Storage`]).
  pub(crate) fn storage(&self) -> Option<(Storage, bool)> {
    match self {
      Var::Seq(items) => Some((
        Storage(Arc::as_ptr(items) as *const (), None),
        Arc::strong_count(items) > 1,
      )),
      Var::Table(table) => Some(table.storage()),
      _ => None,
    }
  }

  /// The same value with every table (at any depth) in struct
  /// representation, its key shape interned. Compose applies this to
  /// literals, so a fixed-typed value is a struct table at runtime
  /// (golden path §7.3); hosts may apply it to values they build once and
  /// pass many times. Interns a shape per distinct key set, so do not apply
  /// it to values with unbounded key sets. A sequence or table held in
  /// several places is converted once and stays shared, so a value built
  /// from copies of another (a constant read in a constant) costs what it
  /// holds, not what it expands to.
  pub fn into_struct_tables(self) -> Var {
    StructTables::default().convert(&self).unwrap_or(self)
  }
}

/// A sequence's or table's storage, as a key for one walk over a value:
/// its address, and a struct table's shape (its keys are not in the
/// storage). Storage held once is reached once, through its one holder, so
/// walks remember only storage shared elsewhere. The address stays valid
/// because the value walked holds every node for the whole walk.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Storage(*const (), Option<Shape>);

/// What one walk over a value remembers per shared storage, created at the
/// first insert: a walk over a value that shares nothing allocates nothing
/// and seeds no hasher.
pub(crate) struct Seen<K, V>(Option<std::collections::HashMap<K, V>>);

impl<K, V> Default for Seen<K, V> {
  fn default() -> Self {
    Seen(None)
  }
}

impl<K: Eq + Hash, V> Seen<K, V> {
  pub(crate) fn get(&self, key: &K) -> Option<&V> {
    self.0.as_ref()?.get(key)
  }

  pub(crate) fn insert(&mut self, key: K, value: V) {
    self
      .0
      .get_or_insert_with(Default::default)
      .insert(key, value);
  }
}

/// One `into_struct_tables` conversion: the result for each shared
/// [`Storage`] already converted.
#[derive(Default)]
struct StructTables(Seen<Storage, Option<Var>>);

impl StructTables {
  /// `v` with its tables in struct representation, or `None` when it holds
  /// no map table (so the caller keeps `v`, sharing it).
  fn convert(&mut self, v: &Var) -> Option<Var> {
    let (key, shared) = v.storage()?;
    if shared && let Some(done) = self.0.get(&key) {
      return done.clone();
    }
    let converted = match v {
      Var::Seq(items) => self.each(items).map(|items| Var::from_seq(Arc::new(items))),
      Var::Table(table) => match &table.0 {
        TableRepr::Struct { shape, slots } => self.each(slots).map(|slots| {
          Var::from_table(Table::of(TableRepr::Struct {
            shape: *shape,
            slots: slots.into(),
          }))
        }),
        TableRepr::Map(entries) => {
          let shape = Shape::new(entries.iter().map(|(k, _)| k.clone()));
          let values: Vec<Var> = entries
            .iter()
            .map(|(_, v)| self.convert(v).unwrap_or_else(|| v.clone()))
            .collect();
          Some(Var::from_table(Table::with_shape(shape, values)))
        }
      },
      _ => unreachable!("only sequences and tables have storage"),
    };
    if shared {
      self.0.insert(key, converted.clone());
    }
    converted
  }

  /// `items` with each converted, or `None` when none needed it.
  fn each(&mut self, items: &[Var]) -> Option<Vec<Var>> {
    let mut out: Option<Vec<Var>> = None;
    for (i, item) in items.iter().enumerate() {
      let converted = self.convert(item);
      match &mut out {
        Some(out) => out.push(converted.unwrap_or_else(|| item.clone())),
        None => {
          if let Some(converted) = converted {
            let mut head = Vec::with_capacity(items.len());
            head.extend_from_slice(&items[..i]);
            head.push(converted);
            out = Some(head);
          }
        }
      }
    }
    out
  }
}

/// One `type_of`: the type of each shared [`Storage`] already typed.
#[derive(Default)]
struct Types(Seen<Storage, Type>);

impl Types {
  fn of(&mut self, v: &Var) -> Type {
    let (key, shared) = match v.storage() {
      Some(storage) => storage,
      None => return Self::scalar(v),
    };
    if shared && let Some(&ty) = self.0.get(&key) {
      return ty;
    }
    let ty = match v {
      Var::Seq(items) => {
        let mut members: Vec<Type> = Vec::new();
        for item in items.iter() {
          let ty = self.of(item);
          if !members.contains(&ty) {
            members.push(ty);
          }
        }
        if members.is_empty() {
          Type::seq(Type::any())
        } else {
          Type::seq(Type::union(members))
        }
      }
      Var::Table(table) => match &table.0 {
        TableRepr::Struct { shape, slots } => {
          let slots: Vec<Type> = slots.iter().map(|v| self.of(v)).collect();
          Type::fixed_table_of(*shape, slots)
        }
        TableRepr::Map(entries) => {
          let entries: Vec<(Arc<str>, Type)> = entries
            .iter()
            .map(|(k, v)| (k.clone(), self.of(v)))
            .collect();
          Type::fixed_table(entries)
        }
      },
      _ => unreachable!("only sequences and tables have storage"),
    };
    if shared {
      self.0.insert(key, ty);
    }
    ty
  }

  fn scalar(v: &Var) -> Type {
    match v {
      Var::None => Type::none(),
      Var::Bool(_) => Type::bool(),
      Var::Int(_) => Type::int(),
      Var::Float(_) => Type::float(),
      Var::Float2(_) => Type::float2(),
      Var::Float3(_) => Type::float3(),
      Var::Float4(_) => Type::float4(),
      Var::String(_) => Type::string(),
      Var::Bytes(_) => Type::bytes(),
      Var::Seq(_) | Var::Table(_) => unreachable!("sequences and tables have storage"),
    }
  }
}

/// A table: string keys in sorted order, values shared copy-on-write
/// (docs/values-and-types.md §2). Cloning is cheap. Two representations
/// with one behavior (golden path §7.3): a **struct** table holds an
/// interned key [`Shape`] and one slot per key, which is what compose
/// produces for every fixed table type, so a literal-key read is an indexed
/// load; a **map** table holds sorted entries, for open tables and values
/// hosts build with [`TableBuilder`] or `collect`. Equality, hashing,
/// printing, iteration and `type_of` do not depend on the representation.
#[derive(Clone)]
pub struct Table(TableRepr);

#[derive(Clone)]
enum TableRepr {
  Struct { shape: Shape, slots: Arc<[Var]> },
  Map(Arc<Vec<(Arc<str>, Var)>>),
}

impl Default for Table {
  fn default() -> Table {
    Table::of(TableRepr::Map(Arc::new(Vec::new())))
  }
}

impl Table {
  fn of(repr: TableRepr) -> Table {
    Table(repr)
  }

  /// Adds a reference to this table's storage, for a bitwise copy of it
  /// (`clone_value`).
  ///
  /// # Safety
  /// The reference belongs to a copy that is dropped once.
  #[inline(always)]
  unsafe fn retain(&self) {
    // SAFETY: the caller's copy owns the new reference.
    unsafe {
      match &self.0 {
        TableRepr::Struct { slots, .. } => Arc::increment_strong_count(Arc::as_ptr(slots)),
        TableRepr::Map(entries) => Arc::increment_strong_count(Arc::as_ptr(entries)),
      }
    }
  }

  /// Drops this table's reference to its storage (`drop_value`).
  ///
  /// # Safety
  /// The table must not be used or dropped afterwards.
  #[inline(always)]
  unsafe fn release(&self) {
    // SAFETY: the caller gives up this table's reference.
    unsafe {
      match &self.0 {
        TableRepr::Struct { slots, .. } => Arc::decrement_strong_count(Arc::as_ptr(slots)),
        TableRepr::Map(entries) => Arc::decrement_strong_count(Arc::as_ptr(entries)),
      }
    }
  }

  pub fn new() -> Table {
    Table::default()
  }

  /// This table's [`Storage`], and whether another table shares it.
  fn storage(&self) -> (Storage, bool) {
    match &self.0 {
      TableRepr::Struct { shape, slots } => (
        Storage(Arc::as_ptr(slots) as *const (), Some(*shape)),
        Arc::strong_count(slots) > 1,
      ),
      TableRepr::Map(entries) => (
        Storage(Arc::as_ptr(entries) as *const (), None),
        Arc::strong_count(entries) > 1,
      ),
    }
  }

  pub fn builder() -> TableBuilder {
    TableBuilder::default()
  }

  /// A struct table of `shape`, with one value per key in the shape's
  /// (sorted) key order. Panics if the counts differ: a caller bug, like a
  /// struct literal with a missing field.
  pub fn with_shape(shape: Shape, values: impl IntoIterator<Item = Var>) -> Table {
    let slots: Arc<[Var]> = values.into_iter().collect();
    assert_eq!(
      slots.len(),
      shape.len(),
      "table of shape {shape} needs {} values",
      shape.len()
    );
    Table::of(TableRepr::Struct { shape, slots })
  }

  /// A struct table of `shape` holding `slots`, in the shape's key order.
  /// Panics if the counts differ, as [`Table::with_shape`].
  pub(crate) fn with_slots(shape: Shape, slots: Arc<[Var]>) -> Table {
    assert_eq!(
      slots.len(),
      shape.len(),
      "table of shape {shape} needs {} values",
      shape.len()
    );
    Table::of(TableRepr::Struct { shape, slots })
  }

  /// The key shape of a struct table; `None` for a map table.
  pub fn shape(&self) -> Option<Shape> {
    match &self.0 {
      TableRepr::Struct { shape, .. } => Some(*shape),
      TableRepr::Map(_) => None,
    }
  }

  /// This table as a struct table of its own keys (interning the shape);
  /// nested tables are converted too. Same contents, same storage when it
  /// already is one.
  pub fn into_struct(self) -> Table {
    let var = Var::from_table(self);
    match StructTables::default()
      .convert(&var)
      .unwrap_or(var)
      .into_table()
    {
      Ok(table) => table,
      Err(_) => unreachable!("a table converts to a table"),
    }
  }

  /// A builder holding this table's entries, to derive a changed table.
  /// Takes the storage when no other table shares it; otherwise copies
  /// the entries, sharing their keys and values.
  pub fn into_builder(self) -> TableBuilder {
    TableBuilder(match self.0 {
      TableRepr::Map(entries) => {
        Arc::try_unwrap(entries).unwrap_or_else(|shared| (*shared).clone())
      }
      TableRepr::Struct { shape, slots } => shape
        .keys()
        .iter()
        .cloned()
        .zip(slots.iter().cloned())
        .collect(),
    })
  }

  pub fn len(&self) -> usize {
    match &self.0 {
      TableRepr::Struct { slots, .. } => slots.len(),
      TableRepr::Map(entries) => entries.len(),
    }
  }

  pub fn is_empty(&self) -> bool {
    self.len() == 0
  }

  /// Out of line: inlined into the VM's run loop (its `Take` arm), it grew
  /// the loop and its register pressure and slowed every instruction (VM
  /// suite, 2026-10-09; `docs/runtime-performance-overview.md` lesson 6).
  #[inline(never)]
  pub fn get(&self, key: &str) -> Option<&Var> {
    match &self.0 {
      TableRepr::Struct { shape, slots } => match shape.index_of(key) {
        Some(i) => Some(&slots[i]),
        None => None,
      },
      TableRepr::Map(entries) => match search(entries, key) {
        Ok(i) => Some(&entries[i].1),
        Err(_) => None,
      },
    }
  }

  /// The value at `index` in key order: the slot of a struct table, the
  /// entry of a map table. Compose resolves a literal key on a fixed table
  /// to its index (golden path §7.3); a map value admitted by a fixed type
  /// has exactly the type's keys, so its sorted entries are the slots.
  #[inline]
  pub fn slot(&self, index: usize) -> Option<&Var> {
    match &self.0 {
      TableRepr::Struct { slots, .. } => slots.get(index),
      TableRepr::Map(entries) => match entries.get(index) {
        Some((_, v)) => Some(v),
        None => None,
      },
    }
  }

  pub fn contains_key(&self, key: &str) -> bool {
    self.get(key).is_some()
  }

  /// Entries in key order.
  pub fn iter(&self) -> Iter<'_> {
    Iter(match &self.0 {
      TableRepr::Struct { shape, slots } => IterRepr::Struct(shape.keys().iter().zip(slots.iter())),
      TableRepr::Map(entries) => IterRepr::Map(entries.iter()),
    })
  }

  /// Keys in sorted order.
  pub fn keys(&self) -> Keys<'_> {
    Keys(self.iter())
  }

  /// Values in key order.
  pub fn values(&self) -> Values<'_> {
    Values(self.iter())
  }

  /// How many tables share this storage. For tests that check snapshot
  /// sharing and release; not a stable API.
  #[doc(hidden)]
  pub fn storage_owners(&self) -> usize {
    match &self.0 {
      TableRepr::Struct { slots, .. } => Arc::strong_count(slots),
      TableRepr::Map(entries) => Arc::strong_count(entries),
    }
  }

  /// The slots of a struct table of `shape` when nothing else shares them,
  /// for a constructor to overwrite in place (docs/values-and-types.md §7).
  pub(crate) fn unique_slots(&mut self, shape: Shape) -> Option<&mut [Var]> {
    match &mut self.0 {
      TableRepr::Struct { shape: s, slots } if *s == shape => Arc::get_mut(slots),
      _ => None,
    }
  }

  fn from_sorted(entries: Vec<(Arc<str>, Var)>) -> Table {
    Table::of(TableRepr::Map(Arc::new(entries)))
  }
}

/// A table's entries in key order.
pub struct Iter<'a>(IterRepr<'a>);

enum IterRepr<'a> {
  Struct(std::iter::Zip<std::slice::Iter<'a, Arc<str>>, std::slice::Iter<'a, Var>>),
  Map(std::slice::Iter<'a, (Arc<str>, Var)>),
}

impl<'a> Iterator for Iter<'a> {
  type Item = (&'a str, &'a Var);

  #[inline]
  fn next(&mut self) -> Option<(&'a str, &'a Var)> {
    match &mut self.0 {
      IterRepr::Struct(zip) => match zip.next() {
        Some((k, v)) => Some((&**k, v)),
        None => None,
      },
      IterRepr::Map(entries) => match entries.next() {
        Some((k, v)) => Some((&**k, v)),
        None => None,
      },
    }
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    match &self.0 {
      IterRepr::Struct(zip) => zip.size_hint(),
      IterRepr::Map(entries) => entries.size_hint(),
    }
  }
}

impl DoubleEndedIterator for Iter<'_> {
  #[inline]
  fn next_back(&mut self) -> Option<Self::Item> {
    match &mut self.0 {
      IterRepr::Struct(zip) => match zip.next_back() {
        Some((k, v)) => Some((&**k, v)),
        None => None,
      },
      IterRepr::Map(entries) => match entries.next_back() {
        Some((k, v)) => Some((&**k, v)),
        None => None,
      },
    }
  }
}

impl ExactSizeIterator for Iter<'_> {}

/// A table's keys in sorted order ([`Table::keys`]).
pub struct Keys<'a>(Iter<'a>);

impl<'a> Iterator for Keys<'a> {
  type Item = &'a str;

  #[inline]
  fn next(&mut self) -> Option<&'a str> {
    match self.0.next() {
      Some((k, _)) => Some(k),
      None => None,
    }
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    self.0.size_hint()
  }
}

impl DoubleEndedIterator for Keys<'_> {
  #[inline]
  fn next_back(&mut self) -> Option<Self::Item> {
    match self.0.next_back() {
      Some((k, _)) => Some(k),
      None => None,
    }
  }
}

impl ExactSizeIterator for Keys<'_> {}

/// A table's values in key order ([`Table::values`]).
pub struct Values<'a>(Iter<'a>);

impl<'a> Iterator for Values<'a> {
  type Item = &'a Var;

  #[inline]
  fn next(&mut self) -> Option<&'a Var> {
    match self.0.next() {
      Some((_, v)) => Some(v),
      None => None,
    }
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    self.0.size_hint()
  }
}

impl DoubleEndedIterator for Values<'_> {
  #[inline]
  fn next_back(&mut self) -> Option<Self::Item> {
    match self.0.next_back() {
      Some((_, v)) => Some(v),
      None => None,
    }
  }
}

impl ExactSizeIterator for Values<'_> {}

/// Something sorted by a string key, for [`search`].
pub(crate) trait Keyed {
  fn key(&self) -> &str;
}

impl<V> Keyed for (Arc<str>, V) {
  #[inline(always)]
  fn key(&self) -> &str {
    &self.0
  }
}

impl Keyed for Arc<str> {
  #[inline(always)]
  fn key(&self) -> &str {
    self
  }
}

/// Binary search of `items`, sorted by key, for `key`: `Ok` with its index,
/// or `Err` with where it would go (as `slice::binary_search`). Written out
/// so the runtime's key lookups take no closure.
#[inline]
pub(crate) fn search<K: Keyed>(items: &[K], key: &str) -> std::result::Result<usize, usize> {
  let (mut low, mut high) = (0, items.len());
  while low < high {
    let mid = low + (high - low) / 2;
    match items[mid].key().cmp(key) {
      std::cmp::Ordering::Less => low = mid + 1,
      std::cmp::Ordering::Greater => high = mid,
      std::cmp::Ordering::Equal => return Ok(mid),
    }
  }
  Err(low)
}

impl<'a> IntoIterator for &'a Table {
  type Item = (&'a str, &'a Var);
  type IntoIter = Iter<'a>;
  fn into_iter(self) -> Iter<'a> {
    self.iter()
  }
}

impl<K: Into<Arc<str>>> FromIterator<(K, Var)> for Table {
  /// A repeated key keeps the last value.
  fn from_iter<I: IntoIterator<Item = (K, Var)>>(entries: I) -> Table {
    let mut builder = TableBuilder::new();
    for (k, v) in entries {
      builder.insert(k, v);
    }
    builder.build()
  }
}

impl From<Table> for Var {
  fn from(t: Table) -> Var {
    Var::from_table(t)
  }
}

/// Representation-independent: equal contents are equal (with `Var`'s
/// bitwise float identity), whichever representation holds them.
impl PartialEq for Table {
  fn eq(&self, other: &Table) -> bool {
    SameValues::default().tables(self, other)
  }
}

impl Eq for Table {}

impl Hash for Table {
  fn hash<H: Hasher>(&self, state: &mut H) {
    self.len().hash(state);
    for (k, v) in self.iter() {
      k.hash(state);
      v.hash(state);
    }
  }
}

impl fmt::Debug for Table {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_map().entries(self.iter()).finish()
  }
}

/// Builds a [`Table`] (a map table); a repeated key keeps the last value.
/// Entries are kept sorted, so inserting keys in order appends.
#[derive(Debug, Default)]
pub struct TableBuilder(Vec<(Arc<str>, Var)>);

impl TableBuilder {
  pub fn new() -> TableBuilder {
    TableBuilder::default()
  }

  fn position(&self, key: &str) -> std::result::Result<usize, usize> {
    search(&self.0, key)
  }

  pub fn insert(&mut self, key: impl Into<Arc<str>>, value: Var) -> &mut TableBuilder {
    let key = key.into();
    match self.position(&key) {
      Ok(i) => self.0[i].1 = value,
      Err(i) => self.0.insert(i, (key, value)),
    }
    self
  }

  /// Removes a key, returning its value.
  pub fn remove(&mut self, key: &str) -> Option<Var> {
    match self.position(key) {
      Ok(i) => Some(self.0.remove(i).1),
      Err(_) => None,
    }
  }

  pub fn get(&self, key: &str) -> Option<&Var> {
    match self.position(key) {
      Ok(i) => Some(&self.0[i].1),
      Err(_) => None,
    }
  }

  pub fn with(mut self, key: impl Into<Arc<str>>, value: Var) -> TableBuilder {
    self.insert(key, value);
    self
  }

  pub fn build(self) -> Table {
    Table::from_sorted(self.0)
  }
}

impl Var {
  /// Human text (`Log`, `ToString`, `String.Format`), like 1.x's except
  /// that floats keep every digit:
  /// strings as they are (also inside sequences and tables), whole floats
  /// without `.0` (`3`, `[1 2.5]`), very large or small ones in exponent
  /// form. Not round-trippable: `Display` gives source syntax.
  pub fn text(&self) -> String {
    let mut out = String::new();
    write_text(self, &mut out);
    out
  }
}

/// A float as human text: the shortest form that reads back as the same
/// value (`3.14159265358`, `6416.2715`, `0.1`), whole numbers without `.0`
/// (`3`, `-0`), exponent form only when very large or small (`1e20`).
/// 1.x rounded to six significant digits (`6416.27`); 2.0 keeps the exact
/// value, a listed deviation.
fn float_text(f: f64, out: &mut String) {
  use std::fmt::Write;
  let _ = if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e16 {
    write!(out, "{f:.0}")
  } else if f != 0.0 && (f.abs() >= 1e16 || f.abs() < 1e-5) {
    write!(out, "{f:e}")
  } else {
    write!(out, "{f}")
  };
}

fn write_text(v: &Var, out: &mut String) {
  match v {
    Var::None => out.push_str("none"),
    Var::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
    Var::Int(i) => out.push_str(&i.to_string()),
    Var::Float(f) => float_text(*f, out),
    Var::Float2(c) => vector(out, "@f2(", c.iter().map(|x| shortest(*x))),
    Var::Float3(c) => vector(out, "@f3(", c.iter().map(|x| shortest(*x))),
    Var::Float4(c) => vector(out, "@f4(", c.iter().map(|x| shortest(*x))),
    Var::String(s) => out.push_str(s),
    Var::Bytes(b) => bytes_text(b, out),
    Var::Seq(items) => {
      out.push('[');
      for (i, item) in items.iter().enumerate() {
        if i > 0 {
          out.push(' ');
        }
        write_text(item, out);
      }
      out.push(']');
    }
    Var::Table(entries) => {
      out.push('{');
      for (i, (k, item)) in entries.iter().enumerate() {
        if i > 0 {
          out.push(' ');
        }
        out.push_str(k);
        out.push_str(": ");
        write_text(item, out);
      }
      out.push('}');
    }
  }
}

fn vector(out: &mut String, open: &str, components: impl Iterator<Item = f64>) {
  out.push_str(open);
  for (i, x) in components.enumerate() {
    if i > 0 {
      out.push(' ');
    }
    float_text(x, out);
  }
  out.push(')');
}

/// An f32 as the f64 its shortest decimal form denotes, so 0.1f32 prints
/// `0.1`, not `0.10000000149011612`.
fn shortest(f: f32) -> f64 {
  f.to_string().parse().unwrap_or(f64::from(f))
}

/// Source syntax: `none`, `true`, `42`, `1.5`, `"text"`, `[1 2]`,
/// `{key: value}`, `@f3(1.0 2.0 3.0)`.
impl fmt::Display for Var {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Var::None => write!(f, "none"),
      Var::Bool(b) => write!(f, "{b}"),
      Var::Int(i) => write!(f, "{i}"),
      Var::Float(x) => write!(f, "{x:?}"),
      Var::Float2(v) => write!(f, "@f2({:?} {:?})", v[0], v[1]),
      Var::Float3(v) => write!(f, "@f3({:?} {:?} {:?})", v[0], v[1], v[2]),
      Var::Float4(v) => write!(f, "@f4({:?} {:?} {:?} {:?})", v[0], v[1], v[2], v[3]),
      Var::String(s) => write!(f, "{:?}", &***s),
      Var::Bytes(b) => {
        let mut text = String::new();
        bytes_text(b, &mut text);
        f.write_str(&text)
      }
      Var::Seq(items) => {
        write!(f, "[")?;
        for (i, item) in items.iter().enumerate() {
          if i > 0 {
            write!(f, " ")?;
          }
          write!(f, "{item}")?;
        }
        write!(f, "]")
      }
      Var::Table(entries) => {
        write!(f, "{{")?;
        for (i, (k, v)) in entries.iter().enumerate() {
          if i > 0 {
            write!(f, " ")?;
          }
          write!(f, "{}: {v}", crate::types::key_text(k))?;
        }
        write!(f, "}}")
      }
    }
  }
}

/// Identity equality: floats compare by bit pattern, so `Var` can be part of
/// cache keys (parameter values). Not the language's `Is` semantics.
impl PartialEq for Var {
  fn eq(&self, other: &Var) -> bool {
    match (self, other) {
      (Var::None, Var::None) => true,
      (Var::Bool(a), Var::Bool(b)) => a == b,
      (Var::Int(a), Var::Int(b)) => a == b,
      (Var::Float(a), Var::Float(b)) => a.to_bits() == b.to_bits(),
      (Var::Float2(a), Var::Float2(b)) => same_bits(&a.0, &b.0),
      (Var::Float3(a), Var::Float3(b)) => same_bits(&a.0, &b.0),
      (Var::Float4(a), Var::Float4(b)) => same_bits(&a.0, &b.0),
      (Var::String(a), Var::String(b)) => a == b,
      (Var::Bytes(a), Var::Bytes(b)) => a == b,
      (Var::Seq(a), Var::Seq(b)) => SameValues::default().seqs(a, b),
      (Var::Table(a), Var::Table(b)) => SameValues::default().tables(a, b),
      _ => false,
    }
  }
}

/// One equality check: the pairs of shared sequences or tables already
/// found equal, by [`Storage`], so two values built from copies of one
/// value compare in what they hold, not in what they expand to. Only equal
/// pairs are remembered: the first unequal pair ends the check.
#[derive(Default)]
struct SameValues(Seen<(Storage, Storage), ()>);

impl SameValues {
  fn eq(&mut self, a: &Var, b: &Var) -> bool {
    match (a, b) {
      (Var::Seq(a), Var::Seq(b)) => self.seqs(a, b),
      (Var::Table(a), Var::Table(b)) => self.tables(a, b),
      _ => a == b,
    }
  }

  fn seqs(&mut self, a: &Arc<Vec<Var>>, b: &Arc<Vec<Var>>) -> bool {
    if Arc::ptr_eq(a, b) {
      return true;
    }
    if a.len() != b.len() {
      return false;
    }
    let storage = |items: &Arc<Vec<Var>>| {
      (
        Storage(Arc::as_ptr(items) as *const (), None),
        Arc::strong_count(items) > 1,
      )
    };
    self.remember(storage(a), storage(b), |same| {
      a.iter().zip(b.iter()).all(|(a, b)| same.eq(a, b))
    })
  }

  fn tables(&mut self, a: &Table, b: &Table) -> bool {
    match (&a.0, &b.0) {
      (TableRepr::Struct { shape: s, slots: x }, TableRepr::Struct { shape: t, slots: y }) => {
        if s != t {
          return false;
        }
        if Arc::ptr_eq(x, y) {
          return true;
        }
      }
      (TableRepr::Map(x), TableRepr::Map(y)) if Arc::ptr_eq(x, y) => return true,
      _ => {}
    }
    if a.len() != b.len() {
      return false;
    }
    self.remember(a.storage(), b.storage(), |same| match (&a.0, &b.0) {
      // Same shape (checked above): the slots line up.
      (TableRepr::Struct { slots: x, .. }, TableRepr::Struct { slots: y, .. }) => {
        x.iter().zip(y.iter()).all(|(v, w)| same.eq(v, w))
      }
      // Either representation iterates its keys in order.
      _ => a
        .iter()
        .zip(b.iter())
        .all(|((k, v), (l, w))| k == l && same.eq(v, w)),
    })
  }

  /// `compare()`, remembered when both sides' storage is shared elsewhere.
  /// A [`Storage`] holds a struct table's shape, so a pair decides its
  /// keys too.
  fn remember(
    &mut self,
    (a, a_shared): (Storage, bool),
    (b, b_shared): (Storage, bool),
    compare: impl FnOnce(&mut Self) -> bool,
  ) -> bool {
    let shared = a_shared && b_shared;
    if shared && self.0.get(&(a, b)).is_some() {
      return true;
    }
    let same = compare(self);
    if same && shared {
      self.0.insert((a, b), ());
    }
    same
  }
}

fn same_bits(a: &[f32], b: &[f32]) -> bool {
  for (x, y) in a.iter().zip(b) {
    if x.to_bits() != y.to_bits() {
      return false;
    }
  }
  true
}

impl Eq for Var {}

impl Hash for Var {
  fn hash<H: Hasher>(&self, state: &mut H) {
    std::mem::discriminant(self).hash(state);
    match self {
      Var::None => {}
      Var::Bool(v) => v.hash(state),
      Var::Int(v) => v.hash(state),
      Var::Float(v) => v.to_bits().hash(state),
      Var::Float2(v) => {
        for x in v.iter() {
          x.to_bits().hash(state);
        }
      }
      Var::Float3(v) => {
        for x in v.iter() {
          x.to_bits().hash(state);
        }
      }
      Var::Float4(v) => {
        for x in v.iter() {
          x.to_bits().hash(state);
        }
      }
      Var::String(v) => v.hash(state),
      Var::Bytes(v) => v.hash(state),
      Var::Seq(v) => v.hash(state),
      Var::Table(v) => v.hash(state),
    }
  }
}

/// Bytes as text: `@bytes(` and two lowercase hex digits per byte, `)`. No
/// source literal reads it back yet (like the vector forms).
fn bytes_text(bytes: &[u8], out: &mut String) {
  use std::fmt::Write;
  out.reserve(8 + 2 * bytes.len());
  out.push_str("@bytes(");
  for b in bytes {
    let _ = write!(out, "{b:02x}");
  }
  out.push(')');
}

/// `*slot = value` for the VM and the engine's step, as 1.x's `destroyVar`:
/// a plain old value is overwritten in place, anything else is dropped out
/// of line. Hot code calls this instead of assigning, so its code never
/// depends on whether the compiler inlines `Var`'s drop, which every
/// variant owning storage makes larger (adding `Bytes` stopped it being
/// inlined, and the whole VM suite got 10 to 88 percent slower,
/// 2026-10-09).
#[inline(always)]
pub(crate) fn assign(slot: &mut Var, value: Var) {
  if slot.is_plain() {
    // SAFETY: the old value owns nothing, so not dropping it leaks nothing.
    unsafe { std::ptr::write(slot, value) }
  } else {
    assign_slow(slot, value);
  }
}

#[inline(never)]
fn assign_slow(slot: &mut Var, value: Var) {
  drop_value(std::mem::replace(slot, value));
}

/// The slow paths' drop and clone: matched here, with each storage's
/// reference count handled in place, rather than through `Var`'s drop and
/// clone (which are calls once not inlined). Their size is off the hot
/// path.
#[inline(always)]
fn drop_value(value: Var) {
  let value = ManuallyDrop::new(value);
  // SAFETY: each storage loses the one reference this value held, once, and
  // the value is not dropped again (`ManuallyDrop`). Through the storage's
  // pointer rather than its `Arc`, so the value stays in registers: only
  // the last reference's free needs memory (a cold call).
  unsafe {
    match &*value {
      Var::String(s) => Arc::decrement_strong_count(Arc::as_ptr(&**s)),
      Var::Seq(items) => Arc::decrement_strong_count(Arc::as_ptr(&**items)),
      Var::Table(table) => table.release(),
      Var::Bytes(bytes) => Arc::decrement_strong_count(Arc::as_ptr(&**bytes)),
      _ => {}
    }
  }
}

#[inline(always)]
fn clone_value(value: &Var) -> Var {
  // SAFETY: the copy's bits are the value's, and its storage gains the
  // reference the copy holds. One whole-value copy whatever the variant,
  // rather than rebuilding each variant field by field.
  unsafe {
    match value {
      Var::String(s) => Arc::increment_strong_count(Arc::as_ptr(&**s)),
      Var::Seq(items) => Arc::increment_strong_count(Arc::as_ptr(&**items)),
      Var::Table(table) => table.retain(),
      Var::Bytes(bytes) => Arc::increment_strong_count(Arc::as_ptr(&**bytes)),
      _ => {}
    }
    std::ptr::read(value)
  }
}

/// Drops a value for hot code, as `assign` overwrites one: nothing to do
/// for a plain value, anything else out of line.
#[inline(always)]
pub(crate) fn release(value: Var) {
  if value.is_plain() {
    std::mem::forget(value);
  } else {
    drop_slow(value);
  }
}

#[inline(never)]
fn drop_slow(value: Var) {
  drop_value(value);
}

/// A copy of a value (`Clone for Var`), as `assign` is for overwriting: a
/// plain value bit for bit in place, anything else cloned out of line.
#[inline(always)]
pub(crate) fn copy(value: &Var) -> Var {
  if value.is_plain() {
    // SAFETY: a plain value owns nothing; its bits are a whole value.
    unsafe { std::ptr::read(value) }
  } else {
    copy_slow(value)
  }
}

#[inline(never)]
fn copy_slow(value: &Var) -> Var {
  clone_value(value)
}

/// `*slot = value.clone()` for the VM's `Set`: plain values bit for bit,
/// anything else with the references counted in place. Copying a string,
/// sequence or table into a variable is the operation itself here, and a
/// call per copy cost the heap assign cases about 2 ns each (2026-10-10).
/// Explicit, so its size does not grow with `Var`'s glue.
#[inline(always)]
pub(crate) fn assign_copy(slot: &mut Var, value: &Var) {
  if slot.is_plain() && value.is_plain() {
    // SAFETY: neither owns anything; the bits are a whole value.
    unsafe { std::ptr::write(slot, std::ptr::read(value)) }
  } else {
    let copied = clone_value(value);
    drop_value(std::mem::replace(slot, copied));
  }
}

/// A sequence's items to change in place, copied first when they are
/// shared (copy on write, as `Arc::make_mut`). The copy is one allocation
/// with room to grow, as the push that usually follows would make it, and
/// copies the items bit for bit before counting the references the
/// copies hold, rather than cloning one item at a time.
#[inline(always)]
pub(crate) fn seq_mut(items: &mut Arc<Vec<Var>>) -> &mut Vec<Var> {
  if Arc::get_mut(items).is_none() {
    unshare_seq(items);
  }
  // SAFETY: the items are not shared (checked, or just copied), and the
  // `&mut` keeps them so; nothing holds a weak reference to a sequence.
  unsafe { &mut *(Arc::as_ptr(items) as *mut Vec<Var>) }
}

#[inline(never)]
fn unshare_seq(items: &mut Arc<Vec<Var>>) {
  let len = items.len();
  let mut copy: Vec<Var> = Vec::with_capacity((len * 2).max(4));
  // SAFETY: the bits are copied into fresh room, then every item owning
  // storage gets the reference its copy holds (which cannot fail or
  // unwind: a reference count, a `Shape` copy) before the length covers
  // them.
  unsafe {
    std::ptr::copy_nonoverlapping(items.as_ptr(), copy.as_mut_ptr(), len);
    for item in items.iter() {
      if !item.is_plain() {
        std::mem::forget(clone_value(item));
      }
    }
    copy.set_len(len);
  }
  *items = Arc::new(copy);
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The storage counts `seq_mut` and `assign_copy` maintain by hand: every
  /// heap kind, each representation of a table, plain values among them.
  #[test]
  fn hand_counted_copies_keep_storage_counts() {
    let text: Arc<str> = Arc::from("text");
    let inner = Arc::new(vec![Var::Int(1)]);
    let bytes: Arc<[u8]> = Arc::from(&b"ab"[..]);
    let map = Var::table([("k", Var::Int(1))]);
    let shape = Shape::new(["a"]);
    let fixed = Var::from_table(Table::with_shape(shape, [Var::Int(2)]));
    let items = vec![
      Var::from_string(text.clone()),
      Var::Int(7),
      Var::from_seq(inner.clone()),
      Var::from_bytes(bytes.clone()),
      map.clone(),
      fixed.clone(),
      Var::float4(1.0, 2.0, 3.0, 4.0),
    ];
    let mut shared = Arc::new(items);
    let original = shared.clone();
    seq_mut(&mut shared).push(Var::Int(8));
    assert!(!Arc::ptr_eq(&shared, &original));
    assert_eq!(original.len(), 7);
    assert_eq!(shared.len(), 8);
    assert_eq!(&shared[..7], &original[..]);
    assert!(shared.capacity() >= 14);
    assert_eq!(Arc::strong_count(&text), 3);
    assert_eq!(Arc::strong_count(&inner), 3);
    assert_eq!(Arc::strong_count(&bytes), 3);
    // Unique now: changed in place, nothing copied.
    let before = Arc::as_ptr(&shared);
    seq_mut(&mut shared).push(Var::Int(9));
    assert_eq!(Arc::as_ptr(&shared), before);

    let mut slot = Var::Int(0);
    for value in [
      &original[0],
      &original[2],
      &original[3],
      &original[4],
      &original[5],
      &Var::Int(3),
    ] {
      assign_copy(&mut slot, value);
      assert_eq!(&slot, value);
    }
    assert_eq!(Arc::strong_count(&text), 3);
    assert_eq!(Arc::strong_count(&inner), 3);
    assert_eq!(Arc::strong_count(&bytes), 3);
    assign_copy(&mut slot, &original[0]);
    assert_eq!(Arc::strong_count(&text), 4);
    // Over itself (the VM skips this, `Set` of a slot's own value) and onto
    // a copy of the same storage.
    let same = slot.clone();
    assign_copy(&mut slot, &same);
    assert_eq!(Arc::strong_count(&text), 5);
    drop((slot, same, shared, original));
    assert_eq!(Arc::strong_count(&text), 1);
    assert_eq!(Arc::strong_count(&inner), 1);
    assert_eq!(Arc::strong_count(&bytes), 1);
    drop((map, fixed));
  }

  #[test]
  fn text_prints_values_for_people() {
    let seq = |v: Vec<Var>| Var::from_seq(Arc::new(v));
    // Whole floats, strings and nesting as 1.x printed them; digits exact.
    assert_eq!(Var::Float(3.0).text(), "3");
    assert_eq!(Var::Float(0.1).text(), "0.1");
    assert_eq!(Var::Float(-0.0).text(), "-0");
    assert_eq!(Var::Float(1e20).text(), "1e20");
    // Exact, not 1.x's six significant digits (a listed deviation).
    assert_eq!(Var::Float(1234.56789012).text(), "1234.56789012");
    assert_eq!(
      Var::float4(6416.2715, 514.1416, 8.651538, 4.5194016).text(),
      "@f4(6416.2715 514.1416 8.651538 4.5194016)"
    );
    assert_eq!(
      seq(vec![Var::Float(1.0), Var::Float(2.5)]).text(),
      "[1 2.5]"
    );
    assert_eq!(
      seq(vec![Var::string("a"), Var::string("b c")]).text(),
      "[a b c]"
    );
    assert_eq!(
      Var::table([("k", Var::string("v")), ("n", Var::Float(2.0))]).text(),
      "{k: v n: 2}"
    );
    assert_eq!(Var::float3(1.0, 2.5, 0.1).text(), "@f3(1 2.5 0.1)");
    // f32 components print their shortest form, not their f64 expansion.
    assert_eq!(Var::float2(0.1, 0.2).text(), "@f2(0.1 0.2)");
    assert_eq!(Var::float2(0.1, 0.2).to_string(), "@f2(0.1 0.2)");
    // Display stays source syntax, round-trippable.
    assert_eq!(Var::Float(3.0).to_string(), "3.0");
    assert_eq!(Var::string("a").to_string(), "\"a\"");
  }
}
