//! Services: process- or mesh-wide resources shards look up at activation
//! (docs/host-requirements.md H3). Database connections per database, a
//! model cache, a client pool: what owns and pools the resources that
//! scripts hold as objects ([`crate::object`]), which refer back to them.
//!
//! The host inserts services before running (`mesh.services_mut().insert(db)`);
//! a shard reads them at activation (`ctx.services().get::<Db>()`), never in
//! compose, which stays a function of its declared inputs. An operation that
//! outlives the activation keeps the service's `Arc` (`get_arc`), so
//! cancelling its instance cannot release a service still in use (H6).
//!
//! When the host inserts none, a module falls back to a default of its own
//! (a process-wide one, built once): activation has shared access to the
//! map, so nothing inserts into it while a mesh runs.

use std::any::{Any, TypeId};
use std::sync::Arc;

use crate::hash::HashMap;

/// A typed map of services, one per type.
#[derive(Clone, Default)]
pub struct Services(HashMap<TypeId, Arc<dyn Any + Send + Sync>>);

impl Services {
  /// No services: what compose-time evaluation sees.
  pub fn empty() -> &'static Services {
    static EMPTY: std::sync::OnceLock<Services> = std::sync::OnceLock::new();
    EMPTY.get_or_init(Services::default)
  }

  /// Inserts `service`, replacing (and returning) one of the same type.
  pub fn insert<T: Any + Send + Sync>(&mut self, service: T) -> Option<Arc<T>> {
    self.insert_arc(Arc::new(service))
  }

  /// Inserts a service the host also holds.
  pub fn insert_arc<T: Any + Send + Sync>(&mut self, service: Arc<T>) -> Option<Arc<T>> {
    let old = self.0.insert(TypeId::of::<T>(), service)?;
    Some(Arc::downcast::<T>(old).expect("services are keyed by their type"))
  }

  pub fn remove<T: Any + Send + Sync>(&mut self) -> Option<Arc<T>> {
    let old = self.0.remove(&TypeId::of::<T>())?;
    Some(Arc::downcast::<T>(old).expect("services are keyed by their type"))
  }

  /// The service of type `T`, borrowed for the activation.
  pub fn get<T: Any + Send + Sync>(&self) -> Option<&T> {
    self.0.get(&TypeId::of::<T>())?.downcast_ref::<T>()
  }

  /// The service of type `T`, shared: for an operation that outlives the
  /// activation.
  pub fn get_arc<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
    let service = self.0.get(&TypeId::of::<T>())?.clone();
    Some(Arc::downcast::<T>(service).expect("services are keyed by their type"))
  }

  pub fn len(&self) -> usize {
    self.0.len()
  }

  pub fn is_empty(&self) -> bool {
    self.0.is_empty()
  }
}
