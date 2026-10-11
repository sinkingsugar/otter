//! Where `Log` and `Msg` write. Lines go to standard output unless a sink is set:
//! a buffer the caller reads afterwards ([`capture`], how tests read what a
//! program logged), or a stream that hands each line to the host as it is
//! written ([`Sink::stream`]; a session takes one with
//! `Session::set_log_sink`).
//!
//! A sink belongs to the thread that sets it and to work started from
//! there on other threads: `otter_io` carries the current [`Sink`] into
//! blocking and async tasks ([`current`], [`with_sink`]), so a host shard
//! logging from the blocking pool reaches it too.

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use crate::instance::InstanceId;

/// One logged line, as a stream receives it.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
  pub text: String,
  /// The instance that logged it, when a shard did (`Log`); `None` for
  /// lines the runtime itself writes.
  pub instance: Option<InstanceId>,
}

/// Where lines go. Cheap to clone; shared across threads.
#[derive(Clone)]
pub struct Sink(Kind);

#[derive(Clone)]
enum Kind {
  Buffer(Arc<Mutex<Vec<String>>>),
  Stream(Arc<dyn Fn(Line) + Send + Sync>),
}

impl Default for Sink {
  /// A buffer ([`capture`]).
  fn default() -> Sink {
    Sink(Kind::Buffer(Arc::default()))
  }
}

impl Sink {
  /// A sink that calls `f` with each line as it is written, on the thread
  /// that writes it (a worker's lines arrive from the worker's thread).
  /// `f` must not block: it runs inside the activation that logged.
  pub fn stream(f: impl Fn(Line) + Send + Sync + 'static) -> Sink {
    Sink(Kind::Stream(Arc::new(f)))
  }

  fn push(&self, line: Line) {
    match &self.0 {
      // A poisoned buffer (a panic while pushing) still records lines.
      Kind::Buffer(lines) => match lines.lock() {
        Ok(mut lines) => lines.push(line.text),
        Err(poisoned) => poisoned.into_inner().push(line.text),
      },
      Kind::Stream(f) => f(line),
    }
  }

  fn take(&self) -> Vec<String> {
    match &self.0 {
      Kind::Buffer(lines) => match lines.lock() {
        Ok(mut lines) => std::mem::take(&mut *lines),
        Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
      },
      Kind::Stream(_) => Vec::new(),
    }
  }
}

thread_local! {
  static CAPTURE: RefCell<Option<Sink>> = const { RefCell::new(None) };
}

/// Writes one line from the runtime itself (no instance).
pub fn emit(line: String) {
  emit_line(Line {
    text: line,
    instance: None,
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
/// `instance` is the one that logged it, as in [`Line`].
pub fn emit_at(level: Level, line: &str, instance: Option<InstanceId>) {
  if (level as u8) < THRESHOLD.load(std::sync::atomic::Ordering::Relaxed) {
    return;
  }
  emit_line(Line {
    text: match level {
      Level::Info => line.to_string(),
      other => format!("[{}] {line}", other.name()),
    },
    instance,
  });
}

/// Writes one line.
pub fn emit_line(line: Line) {
  // Cloned out first, so a stream that logs in turn does not find the
  // capture borrowed.
  match CAPTURE.with(|c| c.borrow().clone()) {
    Some(sink) => sink.push(line),
    None => println!("{}", line.text),
  }
}

/// The sink active on this thread, to hand to work started elsewhere.
pub fn current() -> Option<Sink> {
  CAPTURE.with(|c| c.borrow().clone())
}

/// Runs `f` with `sink` as this thread's sink (none: print), restoring
/// the previous one afterwards, even if `f` panics.
pub fn with_sink<R>(sink: Option<Sink>, f: impl FnOnce() -> R) -> R {
  struct Restore(Option<Option<Sink>>);
  impl Drop for Restore {
    fn drop(&mut self) {
      if let Some(outer) = self.0.take() {
        // The displaced sink is dropped after the borrow ends: a stream may
        // own host resources whose release logs.
        let displaced = CAPTURE.with(|c| c.replace(outer));
        drop(displaced);
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
      emit_at(Level::Debug, "dropped", None);
      set_level(Level::Trace);
      emit_at(Level::Debug, "kept", None);
      set_level(Level::Error);
      emit_at(Level::Warning, "dropped", None);
      emit_at(Level::Error, "bad", None);
      set_level(Level::Info);
      emit_at(Level::Info, "plain", None);
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

  #[test]
  fn a_stream_whose_release_logs_is_dropped_outside_the_sink() {
    struct Guard;
    impl Drop for Guard {
      fn drop(&mut self) {
        emit("sink dropped".into());
      }
    }
    let ((), lines) = capture(|| {
      let guard = Guard;
      with_sink(
        Some(Sink::stream(move |_| {
          let _ = &guard;
        })),
        || {},
      );
    });
    assert_eq!(lines, ["sink dropped"]);
  }

  #[test]
  fn a_stream_receives_each_line_as_it_is_written() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let into = seen.clone();
    let sink = Sink::stream(move |line| into.lock().unwrap().push(line));
    with_sink(Some(sink.clone()), || {
      emit("first".into());
      // Each line is there before the next is written.
      assert_eq!(seen.lock().unwrap().len(), 1);
      emit_line(Line {
        text: "second".into(),
        instance: Some(7),
      });
      let worker = current();
      std::thread::spawn(move || with_sink(worker, || emit("from a worker".into())))
        .join()
        .expect("worker");
    });
    let lines = seen.lock().unwrap().clone();
    let texts: Vec<_> = lines.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(texts, ["first", "second", "from a worker"]);
    assert_eq!(lines[1].instance, Some(7));
    assert_eq!(lines[0].instance, None);
  }
}
