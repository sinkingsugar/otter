//! Object values: host resources held by scripts (docs/host-requirements.md
//! H1). A model, a connection, a session: anything a module hands a script
//! to pass back later, typed by name so compose can tell one kind from
//! another.
//!
//! A host type implements [`Object`] and names its [`ObjectType`]; the
//! crate lists the type in its catalog next to its shards, so scripts can
//! write the name in signatures. [`Var::object`] wraps a value; shards read
//! it back with [`Var::as_object`], or keep the [`ObjectRef`] (an `Arc`) when
//! an operation must hold the resource beyond the activation (H6: an
//! operation owns `Arc`s of everything it touches).
//!
//! Objects are compared, hashed and printed by identity: a creation id,
//! unique in the process and assigned in order, so printing and hashing do
//! not depend on addresses. There is no literal for an object, and
//! compose-time evaluation refuses to produce one, so an object never lands
//! in compose output: device- and connection-bound resources are resolved
//! at run time. Dropping the last value holding an object drops it.
//!
//! Objects are `Send + Sync`: a host object with thread affinity asserts its
//! thread on use (decided 2026-10-11).

use std::any::Any;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::types::Type;
use crate::var::Var;

/// The description of an object type: its name, as scripts write it in
/// signatures (`Test.Session`). A `static` per type, listed in its crate's
/// catalog ([`crate::Catalog::with_objects`]).
#[derive(Debug)]
pub struct ObjectType {
  pub name: &'static str,
}

impl ObjectType {
  pub const fn new(name: &'static str) -> ObjectType {
    ObjectType { name }
  }

  /// The type values of this object type have.
  pub fn to_type(&'static self) -> Type {
    Type::object(self.name)
  }
}

/// Two object types are the same type when they have the same name; a
/// catalog refuses two types with one name.
impl PartialEq for ObjectType {
  fn eq(&self, other: &ObjectType) -> bool {
    self.name == other.name
  }
}

impl Eq for ObjectType {}

/// A host resource a script can hold.
pub trait Object: Any + Send + Sync {
  fn object_type(&self) -> &'static ObjectType;
}

/// An object with its identity: what a [`Var::Object`] holds.
pub struct ObjectBox<T: ?Sized> {
  id: u64,
  value: T,
}

/// A shared object, as a value holds it.
pub type ObjectRef = Arc<ObjectBox<dyn Object>>;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

impl ObjectBox<dyn Object> {
  /// Boxes `value` with a new identity.
  pub fn new<T: Object>(value: T) -> ObjectRef {
    Arc::new(ObjectBox {
      id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
      value,
    })
  }

  /// The object's identity: unique in the process, assigned in creation
  /// order.
  pub fn id(&self) -> u64 {
    self.id
  }

  pub fn object_type(&self) -> &'static ObjectType {
    self.value.object_type()
  }

  pub fn get(&self) -> &dyn Object {
    &self.value
  }

  /// The object as `T`, if it is one.
  pub fn downcast_ref<T: Object>(&self) -> Option<&T> {
    (&self.value as &dyn Any).downcast_ref::<T>()
  }
}

/// `Test.Session#3`: the type and the identity.
impl fmt::Display for ObjectBox<dyn Object> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{}#{}", self.object_type().name, self.id)
  }
}

impl fmt::Debug for ObjectBox<dyn Object> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    fmt::Display::fmt(self, f)
  }
}

impl Var {
  /// A value holding `value` as a new object.
  pub fn object<T: Object>(value: T) -> Var {
    Var::from_object(ObjectBox::new(value))
  }

  /// A value holding this object (another reference to it).
  pub fn from_object(object: ObjectRef) -> Var {
    Var::Object(std::mem::ManuallyDrop::new(object))
  }

  /// The object, if this is one.
  pub fn object_ref(&self) -> Option<&ObjectRef> {
    match self {
      Var::Object(o) => Some(o),
      _ => None,
    }
  }

  /// The object as `T`, if this is an object of that Rust type.
  pub fn as_object<T: Object>(&self) -> Option<&T> {
    self.object_ref()?.downcast_ref::<T>()
  }
}
