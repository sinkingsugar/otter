//! Where `Log` writes. Lines go to standard output unless they are being
//! captured ([`capture`]), which is how tests read what a program logged.
//!
//! A capture belongs to the thread that starts it and to work started from
//! there on other threads: `otter_io` carries the current [`Sink`] into
//! blocking and async tasks ([`current`], [`with_sink`]), so a host shard
//! logging from the blocking pool is captured too.

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

/// Where captured lines go. Cheap to clone; shared across threads.
#[derive(Clone, Default)]
pub struct Sink(Arc<Mutex<Vec<String>>>);

impl Sink {
  fn push(&self, line: String) {
    // A poisoned sink (a panic while pushing) still records lines.
    match self.0.lock() {
      Ok(mut lines) => lines.push(line),
      Err(poisoned) => poisoned.into_inner().push(line),
    }
  }

  fn take(&self) -> Vec<String> {
    match self.0.lock() {
      Ok(mut lines) => std::mem::take(&mut *lines),
      Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
    }
  }
}

thread_local! {
  static CAPTURE: RefCell<Option<Sink>> = const { RefCell::new(None) };
}

/// Writes one log line.
pub fn emit(line: String) {
  CAPTURE.with(|c| match &*c.borrow() {
    Some(sink) => sink.push(line),
    None => println!("{line}"),
  });
}

/// The capture active on this thread, to hand to work started elsewhere.
pub fn current() -> Option<Sink> {
  CAPTURE.with(|c| c.borrow().clone())
}

/// Runs `f` with `sink` as this thread's capture (none: print), restoring
/// the previous one afterwards, even if `f` panics.
pub fn with_sink<R>(sink: Option<Sink>, f: impl FnOnce() -> R) -> R {
  struct Restore(Option<Option<Sink>>);
  impl Drop for Restore {
    fn drop(&mut self) {
      if let Some(outer) = self.0.take() {
        CAPTURE.with(|c| *c.borrow_mut() = outer);
      }
    }
  }
  let _restore = Restore(Some(CAPTURE.with(|c| c.replace(sink))));
  f()
}

/// Runs `f`, returning its result and the lines logged meanwhile on this
/// thread and by work it started on other threads (they are not printed).
/// Captures nest: the inner one takes the lines logged inside it.
pub fn capture<R>(f: impl FnOnce() -> R) -> (R, Vec<String>) {
  let sink = Sink::default();
  let result = with_sink(Some(sink.clone()), f);
  (result, sink.take())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_panicking_capture_restores_the_outer_one() {
    let ((), lines) = capture(|| {
      let inner = std::panic::catch_unwind(|| capture(|| panic!("boom")));
      assert!(inner.is_err());
      emit("after".into());
    });
    assert_eq!(lines, ["after"]);
  }

  #[test]
  fn work_on_another_thread_logs_into_the_capture_it_was_given() {
    let ((), lines) = capture(|| {
      let sink = current();
      std::thread::spawn(move || with_sink(sink, || emit("from a worker".into())))
        .join()
        .expect("worker");
      emit("from here".into());
    });
    assert_eq!(lines, ["from a worker", "from here"]);
  }
}
