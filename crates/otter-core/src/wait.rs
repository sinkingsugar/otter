//! Blocking until a mesh has work: the signal its instances' wakers fire
//! ([`crate::Mesh::wait`]), and a cancellation token that fires it too, so a
//! host thread blocked on a mesh sleeps instead of polling, and still
//! notices a wake, a due pause or a cancellation at once.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};

/// What a mesh's waiting thread sleeps on. A wake from another thread
/// takes the lock only while a thread is waiting.
#[derive(Default)]
pub(crate) struct Signal {
  /// A thread is in [`crate::Mesh::wait`].
  waiting: AtomicBool,
  /// Set by a wake while a thread waits; cleared when the wait ends.
  woken: Mutex<bool>,
  ready: Condvar,
}

impl Signal {
  /// Wakes the waiting thread, if any. Call it after publishing what the
  /// waiter checks (an instance's wake flag, a cancellation).
  pub(crate) fn notify(&self) {
    // Paired with the waiter's store of `waiting` and its reads of the
    // flags after it: either this load sees the waiter, or the waiter sees
    // what was published before it.
    if self.waiting.load(Ordering::SeqCst) {
      *self.woken.lock().unwrap_or_else(|e| e.into_inner()) = true;
      self.ready.notify_all();
    }
  }

  /// Runs `wait` with the signal's lock held and the waiter announced:
  /// `wait` checks what it waits for, then sleeps through the guard it is
  /// given (which releases the lock while asleep). A wake between the check
  /// and the sleep blocks on the lock until the sleep, so none is lost.
  pub(crate) fn hold<R>(&self, wait: impl FnOnce(Held<'_>) -> R) -> R {
    let mut guard = self.woken.lock().unwrap_or_else(|e| e.into_inner());
    // A wake as the last wait ended: the caller checks afresh.
    *guard = false;
    self.waiting.store(true, Ordering::SeqCst);
    let result = wait(Held {
      guard: Some(guard),
      ready: &self.ready,
    });
    self.waiting.store(false, Ordering::SeqCst);
    result
  }
}

/// The signal's lock, held by a waiter ([`Signal::hold`]).
pub(crate) struct Held<'a> {
  guard: Option<std::sync::MutexGuard<'a, bool>>,
  ready: &'a Condvar,
}

impl Held<'_> {
  /// Whether a wake came since the wait began; clears it.
  pub(crate) fn take_woken(&mut self) -> bool {
    let guard = self.guard.as_mut().expect("held");
    std::mem::replace(&mut **guard, false)
  }

  /// Sleeps until a wake, or for at most `limit`. May return early.
  pub(crate) fn sleep(&mut self, limit: Option<std::time::Duration>) {
    let guard = self.guard.take().expect("held");
    let guard = match limit {
      Some(limit) => {
        self
          .ready
          .wait_timeout(guard, limit)
          .unwrap_or_else(|e| e.into_inner())
          .0
      }
      None => self.ready.wait(guard).unwrap_or_else(|e| e.into_inner()),
    };
    self.guard = Some(guard);
  }
}

/// Cancels a run from any thread: a budgeted run ([`crate::Budget`]) stops
/// inside its activation, and a thread blocked in [`crate::Mesh::wait`]
/// with this token wakes. Clones share one cancellation.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<Cancel>);

#[derive(Default)]
struct Cancel {
  cancelled: AtomicBool,
  /// The signals of the meshes waiting with this token.
  waiters: Mutex<Vec<Weak<Signal>>>,
}

impl CancelToken {
  pub fn new() -> CancelToken {
    CancelToken::default()
  }

  /// Cancels: every run under this token stops. Cannot be undone.
  pub fn cancel(&self) {
    self.0.cancelled.store(true, Ordering::SeqCst);
    let waiters = self.0.waiters.lock().unwrap_or_else(|e| e.into_inner());
    for signal in waiters.iter().filter_map(Weak::upgrade) {
      signal.notify();
    }
  }

  pub fn is_cancelled(&self) -> bool {
    self.0.cancelled.load(Ordering::Acquire)
  }

  /// Wakes `signal` on cancellation until the returned guard drops.
  pub(crate) fn watch(&self, signal: &Arc<Signal>) -> Watch<'_> {
    let mut waiters = self.0.waiters.lock().unwrap_or_else(|e| e.into_inner());
    waiters.retain(|w| w.strong_count() > 0);
    waiters.push(Arc::downgrade(signal));
    Watch {
      token: self,
      signal: Arc::as_ptr(signal),
    }
  }
}

impl std::fmt::Debug for CancelToken {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_tuple("CancelToken")
      .field(&self.is_cancelled())
      .finish()
  }
}

/// A signal watching a token ([`CancelToken::watch`]).
pub(crate) struct Watch<'a> {
  token: &'a CancelToken,
  signal: *const Signal,
}

impl Drop for Watch<'_> {
  fn drop(&mut self) {
    let mut waiters = self
      .token
      .0
      .waiters
      .lock()
      .unwrap_or_else(|e| e.into_inner());
    if let Some(at) = waiters.iter().position(|w| w.as_ptr() == self.signal) {
      waiters.swap_remove(at);
    }
  }
}
