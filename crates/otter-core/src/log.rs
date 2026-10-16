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

/// How severe a message is (`Msg`'s `level`), least first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
  Trace,
  Debug,
  Info,
  Warning,
  Error,
}

impl Level {
  pub const ALL: [Level; 5] = [
    Level::Trace,
    Level::Debug,
    Level::Info,
    Level::Warning,
    Level::Error,
  ];

  /// The name scripts write (`"warning"`).
  pub fn name(self) -> &'static str {
    match self {
      Level::Trace => "trace",
      Level::Debug => "debug",
      Level::Info => "info",
      Level::Warning => "warning",
      Level::Error => "error",
    }
  }

  pub fn from_name(name: &str) -> Option<Level> {
    Level::ALL.into_iter().find(|l| l.name() == name)
  }
}

/// The least severe level written, for the whole process (`Info` unless a
/// host lowers or raises it).
static THRESHOLD: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(Level::Info as u8);

/// Sets the least severe level [`emit_at`] writes; less severe messages
/// are dropped.
pub fn set_level(level: Level) {
  THRESHOLD.store(level as u8, std::sync::atomic::Ordering::Relaxed);
}

/// The least severe level written.
pub fn level() -> Level {
  Level::ALL[usize::from(THRESHOLD.load(std::sync::atomic::Ordering::Relaxed))]
}

/// Writes one line at `level`, if the threshold admits it: an `Info` line
/// as it is, any other prefixed with its level (`[warning] text`).
pub fn emit_at(level: Level, line: &str) {
  if (level as u8) < THRESHOLD.load(std::sync::atomic::Ordering::Relaxed) {
    return;
  }
  emit(match level {
    Level::Info => line.to_string(),
    other => format!("[{}] {line}", other.name()),
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
  fn the_threshold_drops_less_severe_lines() {
    // The only test here that changes the process-wide threshold.
    let ((), lines) = capture(|| {
      emit_at(Level::Debug, "dropped");
      set_level(Level::Trace);
      emit_at(Level::Debug, "kept");
      set_level(Level::Error);
      emit_at(Level::Warning, "dropped");
      emit_at(Level::Error, "bad");
      set_level(Level::Info);
      emit_at(Level::Info, "plain");
    });
    assert_eq!(lines, ["[debug] kept", "[error] bad", "plain"]);
    assert_eq!(level(), Level::Info);
    assert_eq!(Level::from_name("warning"), Some(Level::Warning));
    assert_eq!(Level::from_name("Warning"), None);
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
