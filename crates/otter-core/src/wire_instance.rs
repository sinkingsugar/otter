//! Spawned instances as values (docs/host-requirements.md H5): what `Spawn`
//! returns, and what `Wait`, `Peek`, `IsRunning` and `Stop(w)` act on.
//!
//! Every instance has a [`Completion`]: its outcome once it has finished
//! (published after its cleanup), the wakers of the instances waiting on it,
//! a cancel request, and the children it owns. A handle ([`WireInstance`],
//! an object of type `Wire.Instance`) holds the completion, so it stays
//! valid after the mesh retires the instance's record and never names
//! another instance: across a reload it keeps naming the original one.
//!
//! Ending an instance from outside is a request ([`Completion::request`]):
//! it marks the cell and counts it on the mesh's [`CancelRequests`]. The
//! mesh acts on requests at the end of a tick, after every running
//! instance had its activation (until none are left, so a whole tree goes
//! in that tick); a request made between ticks takes effect at the end of
//! the next one. Nothing is checked per activation. `Wait` acknowledges the
//! instance's cleanup, not the quiescence of external work it started (H6).

use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Waker;

use crate::instance::{Outcome, WakeFlag};
use crate::object::{Object, ObjectType};

/// The object type of spawned-instance handles.
pub static WIRE_INSTANCE: ObjectType = ObjectType::new("Wire.Instance");

/// Object types this crate defines, for [`crate::Catalog::with_objects`]
/// (a catalog built with `Catalog::new` includes them).
pub static OBJECTS: &[&ObjectType] = &[&WIRE_INSTANCE];

/// The cancel requests of one mesh not yet acted on. Shared by the mesh's
/// completions; the mesh reads it once per tick.
#[derive(Default)]
pub struct CancelRequests(AtomicUsize);

impl CancelRequests {
  pub(crate) fn pending(&self) -> bool {
    self.0.load(Ordering::Acquire) != 0
  }

  /// Takes the count, before a pass that acts on every marked instance.
  pub(crate) fn take(&self) -> usize {
    self.0.swap(0, Ordering::AcqRel)
  }
}

/// How a request ends an instance: `Stop(w)` stops it, its owner's end or
/// the host cancels it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
  Stop = 1,
  Cancel = 2,
}

impl Request {
  pub(crate) fn outcome(self) -> Outcome {
    match self {
      Request::Stop => Outcome::Stopped,
      Request::Cancel => Outcome::Cancelled,
    }
  }
}

/// One instance's completion: see the module documentation.
pub struct Completion {
  wire: Arc<str>,
  state: Mutex<State>,
  /// The first request (a [`Request`] as a number), 0 for none.
  request: AtomicU8,
  requests: Arc<CancelRequests>,
  /// The instance's wake flag and its waker (what it waits on).
  pub(crate) wake: Arc<WakeFlag>,
  pub(crate) waker: Waker,
}

#[derive(Default)]
struct State {
  outcome: Option<Outcome>,
  /// Waiting instances, by registration key (a waiter that goes away
  /// removes its own).
  waiters: Vec<(u64, Waker)>,
  next_key: u64,
  /// Children this instance owns (not detached), pruned of finished ones
  /// as new ones are added.
  children: Vec<Arc<Completion>>,
}

impl Completion {
  pub(crate) fn new(wire: Arc<str>, requests: Arc<CancelRequests>) -> Arc<Completion> {
    let wake = Arc::new(WakeFlag::default());
    Arc::new(Completion {
      wire,
      state: Mutex::new(State::default()),
      request: AtomicU8::new(0),
      requests,
      waker: Waker::from(wake.clone()),
      wake,
    })
  }

  /// The mesh's request counter, shared by the completions of its instances.
  pub(crate) fn requests(&self) -> &Arc<CancelRequests> {
    &self.requests
  }

  fn state(&self) -> std::sync::MutexGuard<'_, State> {
    // A waker that panicked while waking cannot leave the state torn: every
    // mutation completes before wakers run.
    self.state.lock().unwrap_or_else(|e| e.into_inner())
  }

  /// The name of the wire the instance runs.
  pub fn wire(&self) -> &str {
    &self.wire
  }

  /// The outcome, once the instance has finished.
  pub fn outcome(&self) -> Option<Outcome> {
    self.state().outcome.clone()
  }

  pub fn is_finished(&self) -> bool {
    self.state().outcome.is_some()
  }

  /// Asks the mesh to end the instance (its own children are cancelled in
  /// turn). A finished instance is left as it is; the first request wins,
  /// and asking again does nothing.
  pub fn request(&self, request: Request) {
    if self.is_finished()
      || self
        .request
        .compare_exchange(0, request as u8, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
      return;
    }
    self.requests.0.fetch_add(1, Ordering::AcqRel);
  }

  /// The request made of the instance, if any.
  pub(crate) fn requested(&self) -> Option<Request> {
    match self.request.load(Ordering::Acquire) {
      0 => None,
      1 => Some(Request::Stop),
      _ => Some(Request::Cancel),
    }
  }

  /// Registers `waker` to be woken when the instance finishes, unless it
  /// already has: then its outcome. The check and the registration are one
  /// step, so no finish falls between them.
  pub(crate) fn wait(&self, waker: &Waker) -> std::result::Result<u64, Outcome> {
    let mut state = self.state();
    if let Some(outcome) = &state.outcome {
      return Err(outcome.clone());
    }
    let key = state.next_key;
    state.next_key += 1;
    state.waiters.push((key, waker.clone()));
    Ok(key)
  }

  /// Removes a registration (a waiter that ended or went away).
  pub(crate) fn unwait(&self, key: u64) {
    self.state().waiters.retain(|(k, _)| *k != key);
  }

  /// How many waiters are registered, for tests; not a stable API.
  #[doc(hidden)]
  pub fn waiter_count(&self) -> usize {
    self.state().waiters.len()
  }

  /// How many children the instance holds, for tests; not a stable API.
  #[doc(hidden)]
  pub fn child_count(&self) -> usize {
    self.state().children.len()
  }

  /// Adds a child this instance owns, pruning the finished ones.
  pub(crate) fn adopt(&self, child: Arc<Completion>) {
    let mut state = self.state();
    state.children.retain(|c| !c.is_finished());
    state.children.push(child);
  }

  /// Publishes the outcome (after the instance's cleanup), wakes its
  /// waiters outside the lock, and asks its children to be cancelled.
  pub(crate) fn finish(&self, outcome: Outcome) {
    let (waiters, children) = {
      let mut state = self.state();
      if state.outcome.is_some() {
        return;
      }
      state.outcome = Some(outcome);
      (
        std::mem::take(&mut state.waiters),
        std::mem::take(&mut state.children),
      )
    };
    for (_, waker) in waiters {
      waker.wake();
    }
    for child in children {
      child.request(Request::Cancel);
    }
  }
}

impl std::fmt::Debug for Completion {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Completion")
      .field("wire", &self.wire)
      .field("outcome", &self.outcome())
      .finish()
  }
}

/// A handle to a spawned instance: the object `Spawn` returns.
pub struct WireInstance(pub(crate) Arc<Completion>);

impl WireInstance {
  pub fn completion(&self) -> &Arc<Completion> {
    &self.0
  }

  /// A handle value naming `completion`'s instance.
  pub(crate) fn var(completion: Arc<Completion>) -> crate::var::Var {
    crate::var::Var::object(WireInstance(completion))
  }
}

/// The completion a handle value names. Compose types the variables these
/// shards read, so another value is a host error.
pub(crate) fn completion_of(value: &crate::var::Var) -> crate::error::Result<&Arc<Completion>> {
  value
    .as_object::<WireInstance>()
    .map(WireInstance::completion)
    .ok_or_else(|| {
      crate::error::Error::Activation(format!("expected a Wire.Instance, got {}", value.type_of()))
    })
}

impl Object for WireInstance {
  fn object_type(&self) -> &'static ObjectType {
    &WIRE_INSTANCE
  }
}

/// A spawn waiting for the end of the tick to start.
pub(crate) struct Queued {
  pub wire: Arc<crate::compose::CompiledWire>,
  pub input: crate::var::Var,
  pub completion: Arc<Completion>,
}
