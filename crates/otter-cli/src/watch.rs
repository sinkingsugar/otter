//! A small polling host for the reloadable session API.

use std::io::BufRead;
use std::process::ExitCode;
use std::sync::mpsc;

use otter_core::{Outcome, ResetPolicy};
use otter_lang::{FileWatcher, Finished, Session, WatchControl, WatchEvent, render};

use crate::{Options, catalog};

pub(super) fn watch(o: &Options) -> Result<ExitCode, String> {
  let (quit, input) = mpsc::channel();
  let signal_quit = quit.clone();
  let signal = otter_io::runtime::runtime().spawn(async move {
    let result = tokio::signal::ctrl_c()
      .await
      .map(|()| false)
      .map_err(|e| e.to_string());
    let _ = signal_quit.send(result);
  });
  std::thread::spawn(move || {
    for line in std::io::stdin().lock().lines() {
      match line {
        Ok(line) if line.trim() == "r" => {
          if quit.send(Ok(true)).is_err() {
            return;
          }
        }
        Ok(line) if line.trim() != "q" => continue,
        _ => break,
      }
    }
    let _ = quit.send(Ok(false));
  });
  eprintln!(
    "watching {}; r restarts; Ctrl-C, q or stdin EOF stops cleanly",
    o.file
  );
  let catalog = catalog();
  let mut session = Session::new();
  // Running watch is the opt-in for applying state resets (golden path §11).
  session.set_reset_policy(ResetPolicy::Apply);
  session.set_files(o.files());
  let mut error = None;
  FileWatcher::new(&o.file).run(
    &mut session,
    &catalog,
    &o.defines,
    || match input.try_recv() {
      Ok(Ok(true)) => WatchControl::Restart,
      Ok(result) => {
        error = result.err();
        WatchControl::Stop
      }
      Err(mpsc::TryRecvError::Disconnected) => {
        error = Some("watch shutdown channel disconnected".into());
        WatchControl::Stop
      }
      Err(mpsc::TryRecvError::Empty) => WatchControl::Continue,
    },
    |event| match event {
      WatchEvent::Reloaded {
        restarted,
        finished,
        report,
      } => {
        print_finished(finished);
        if restarted {
          eprintln!("{}: restarted", o.file);
        } else {
          let mut parts = vec!["reloaded (edited functions apply at their next call)".to_string()];
          for (label, names) in [
            ("retained", &report.retained),
            ("reset", &report.reset),
            ("restarted", &report.restarted),
            ("swapped", &report.swapped),
          ] {
            if !names.is_empty() {
              parts.push(format!("{label}: {}", names.join(", ")));
            }
          }
          eprintln!("{}: {}", o.file, parts.join("; "));
        }
      }
      WatchEvent::Rejected {
        source,
        diagnostics,
      } => {
        for d in diagnostics {
          eprint!("{}", render(&d, &source));
        }
        eprintln!("{}: edit rejected; previous execution retained", o.file);
      }
      WatchEvent::ReadError(e) => eprintln!("{}: {e}; previous execution retained", o.file),
      WatchEvent::Tick(finished) | WatchEvent::Stopped(finished) => print_finished(finished),
    },
  );
  signal.abort();
  error.map_or(Ok(ExitCode::SUCCESS), Err)
}

fn print_finished(finished: Vec<Finished>) {
  for Finished { wire, outcome } in finished {
    match outcome {
      Outcome::Completed(v) => println!("{wire}: {v}"),
      Outcome::Stopped => println!("{wire}: stopped"),
      Outcome::Cancelled => println!("{wire}: cancelled"),
      Outcome::Failed(e) => eprintln!(
        "{wire}: failed: {e}; stopped until the next successful reload or session restart"
      ),
    }
  }
}
