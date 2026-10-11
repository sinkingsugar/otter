//! Synchronous file watching for embedding hosts. No terminal, signal,
//! logging or async-runtime policy is installed by this module.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use otter_core::{Catalog, Diagnostic, ReloadReport};

use crate::{Finished, Session, Source};

const POLL: Duration = Duration::from_millis(100);
const DEFAULT_FRAME: Duration = Duration::from_millis(16);

/// A command from the host's keyboard, channel or other event source.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WatchControl {
  #[default]
  Continue,
  /// Validate the current file and restart all script state, even if unchanged.
  Restart,
  Stop,
}

/// Events are delivered synchronously on the driving thread. Diagnostics
/// belong to the rejected source; the running revision remains untouched.
pub enum WatchEvent {
  Reloaded {
    restarted: bool,
    finished: Vec<Finished>,
    /// What a preserving reload retained, reset and restarted.
    report: ReloadReport,
  },
  Rejected {
    source: Source,
    diagnostics: Vec<Diagnostic>,
  },
  ReadError(String),
  /// Called after every session tick, including ticks with no outcomes.
  Tick(Vec<Finished>),
  Stopped(Vec<Finished>),
}

/// Content polling, save stability and script pacing shared by the CLI and
/// embedding hosts. Two identical samples, 100 ms apart, admit a save.
/// Failed revisions/read errors are reported once until the observation changes.
/// Compilation and file reads are synchronous; no background thread is created.
pub struct FileWatcher {
  path: PathBuf,
  changes: Changes<Observation>,
  poll_at: Instant,
  tick_at: Instant,
}

impl FileWatcher {
  pub fn new(path: impl Into<PathBuf>) -> Self {
    let now = Instant::now();
    Self {
      path: path.into(),
      changes: Changes::default(),
      poll_at: now,
      tick_at: now,
    }
  }

  pub fn path(&self) -> &Path {
    &self.path
  }

  /// Drive from a host event loop. Returns a suggested delay until the next
  /// poll/tick; this method does not sleep. `restart` bypasses save stability
  /// and validates the current file immediately, including rejected text.
  /// The host owns shutdown (`Session::stop`) when using this method directly.
  pub fn poll(
    &mut self,
    session: &mut Session,
    catalog: &Catalog,
    defines: &HashMap<String, String>,
    restart: bool,
    mut event: impl FnMut(WatchEvent),
  ) -> Duration {
    if restart || Instant::now() >= self.poll_at {
      // The script and every file its last load looked at, as one
      // observation: an edit to any of them is a new revision (of the
      // script's text). Files are compared by their exact bytes, and a
      // missing one counts, so creating it is a change.
      let read = Observation {
        text: std::fs::read_to_string(&self.path).map_err(|e| e.to_string()),
        files: session.observe_dependencies(),
      };
      let change = if restart {
        self.changes.previous = Some(read.clone());
        self.changes.attempted = Some(read.clone());
        Some(read)
      } else {
        self.changes.observe(read)
      };
      if let Some(change) = change {
        match change.text {
          Ok(text) => {
            let source = Source::new(self.path.to_string_lossy(), text.clone());
            let result = if restart {
              session.reload(source, catalog, defines)
            } else {
              session.reload_preserving(source, catalog, defines)
            };
            // What this load read is what the next sample is compared with
            // (the observation above did not know a newly named file): a
            // file edited since the load read it is still a change.
            let consumed = Observation {
              text: Ok(text),
              files: session.dependencies().to_vec(),
            };
            self.changes.previous = Some(consumed.clone());
            self.changes.attempted = Some(consumed);
            match result {
              Ok(finished) => {
                let report = if restart {
                  ReloadReport::default()
                } else {
                  session.reload_report().cloned().unwrap_or_default()
                };
                event(WatchEvent::Reloaded {
                  restarted: restart,
                  finished,
                  report,
                });
                self.tick_at = Instant::now();
              }
              Err((source, diagnostics)) => event(WatchEvent::Rejected {
                source,
                diagnostics,
              }),
            }
          }
          Err(e) => event(WatchEvent::ReadError(e)),
        }
      }
      self.poll_at = Instant::now() + POLL;
    }
    if Instant::now() >= self.tick_at {
      event(WatchEvent::Tick(session.tick()));
      let frame = session.frame_interval().unwrap_or(DEFAULT_FRAME);
      self.tick_at = Instant::now().checked_add(frame).unwrap_or(self.poll_at);
    }
    self
      .poll_at
      .min(self.tick_at)
      .saturating_duration_since(Instant::now())
  }

  /// Blocking convenience loop. The host supplies nonblocking control polling
  /// and event callbacks. Stop cancels the session and reports final outcomes.
  /// Sleeps between control checks are capped at 10 ms,
  /// even when script FPS is low. Completion or a failed edit does not exit.
  pub fn run(
    &mut self,
    session: &mut Session,
    catalog: &Catalog,
    defines: &HashMap<String, String>,
    mut control: impl FnMut() -> WatchControl,
    mut event: impl FnMut(WatchEvent),
  ) {
    loop {
      let restart = match control() {
        WatchControl::Continue => false,
        WatchControl::Restart => true,
        WatchControl::Stop => {
          event(WatchEvent::Stopped(session.stop()));
          return;
        }
      };
      let delay = self.poll(session, catalog, defines, restart, &mut event);
      std::thread::sleep(delay.min(Duration::from_millis(10)));
    }
  }
}

/// One sample: the script's text (or why it could not be read) and each
/// file its last load looked at, with what is there now.
#[derive(Clone, PartialEq)]
struct Observation {
  text: Result<String, String>,
  files: Vec<(String, crate::session::Fingerprint)>,
}

/// Compare contents, not timestamps, to notice atomic saves and same-size
/// edits. Require two identical samples before trying a revision; remember
/// rejected text too, so it is diagnosed once until the contents change.
struct Changes<T> {
  previous: Option<T>,
  attempted: Option<T>,
}

impl<T> Default for Changes<T> {
  fn default() -> Self {
    Changes {
      previous: None,
      attempted: None,
    }
  }
}

impl<T: Clone + PartialEq> Changes<T> {
  fn observe(&mut self, read: T) -> Option<T> {
    let stable = self.previous.as_ref() == Some(&read);
    self.previous = Some(read.clone());
    if stable && self.attempted.as_ref() != Some(&read) {
      self.attempted = Some(read.clone());
      Some(read)
    } else {
      None
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn changes_wait_for_stability_and_retry_after_read_errors() {
    let mut changes = Changes::<Result<String, String>>::default();
    let a = Ok("41".to_string());
    let b = Ok("42".to_string());
    let missing = Err("missing".to_string());
    assert!(changes.observe(a.clone()).is_none());
    // A partial write is never submitted.
    assert!(changes.observe(b.clone()).is_none());
    assert_eq!(changes.observe(b.clone()), Some(b.clone()));
    assert!(changes.observe(b.clone()).is_none());
    assert!(changes.observe(missing.clone()).is_none());
    assert_eq!(changes.observe(missing.clone()), Some(missing));
    assert!(changes.observe(b.clone()).is_none());
    assert_eq!(changes.observe(b.clone()), Some(b));
    assert!(changes.observe(a.clone()).is_none());
    assert_eq!(changes.observe(a.clone()), Some(a));
  }
}
