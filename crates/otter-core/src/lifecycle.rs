//! Shared lifecycle helpers: one cleanup policy for every flow, composite and
//! scheduler, so the same guarantees hold everywhere (contract §5).
//!
//! - [`cleanup_each`]: attempt every cleanup, even if some panic; re-raise the
//!   first panic after all have run.
//! - [`instantiate_all`]: instantiate in order; if one fails (or panics),
//!   roll back the ones already instantiated with [`cleanup_each`], and
//!   return an error. A panic during rollback is reported in that error, not
//!   propagated.

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

use crate::error::{Error, Result, panic_message};

/// Runs `cleanup` for every item, even if some calls panic. After all have
/// run, re-raises the first panic, if any.
pub(crate) fn cleanup_each<T>(items: impl IntoIterator<Item = T>, mut cleanup: impl FnMut(T)) {
  let mut first_panic: Option<Box<dyn Any + Send>> = None;
  for item in items {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| cleanup(item))) {
      first_panic.get_or_insert(payload);
    }
  }
  if let Some(payload) = first_panic {
    resume_unwind(payload);
  }
}

/// Instantiates `items` in order, returning their states. If one fails or
/// panics, the states already created are cleaned up (in reverse order,
/// every one attempted) before the error is returned. Never panics because
/// of a child's instantiate or rollback cleanup.
pub(crate) fn instantiate_all<T, S>(
  items: impl IntoIterator<Item = T>,
  mut instantiate: impl FnMut(&T) -> Result<S>,
  mut cleanup: impl FnMut(&T, &mut S),
) -> Result<Vec<(T, S)>> {
  let mut done: Vec<(T, S)> = Vec::new();
  for item in items {
    let result = catch_unwind(AssertUnwindSafe(|| instantiate(&item))).unwrap_or_else(|payload| {
      Err(Error::Activation(format!(
        "panic in instantiate: {}",
        panic_message(&*payload)
      )))
    });
    match result {
      Ok(state) => done.push((item, state)),
      Err(err) => {
        let rollback = catch_unwind(AssertUnwindSafe(|| {
          cleanup_each(done.iter_mut().rev(), |(item, state)| cleanup(item, state))
        }));
        return Err(match rollback {
          Ok(()) => err,
          Err(payload) => Error::Activation(format!(
            "{err}; and a cleanup panicked during rollback: {}",
            panic_message(&*payload)
          )),
        });
      }
    }
  }
  Ok(done)
}
