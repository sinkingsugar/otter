//! Frontend acceptance: source programs parse, lower, compose and run with
//! matching results, and every problem is located in
//! the source.

use std::collections::HashMap;

use otter_core::diagnostic::Diagnostic;
use otter_core::{Catalog, Outcome, Var};
use otter_lang::{Program, Source};

fn catalog() -> Catalog {
  Catalog::new(&[otter_core::shards::CATALOG]).unwrap()
}

fn no_defines() -> HashMap<String, String> {
  HashMap::new()
}

/// Load errors (syntax or lowering) of a source.
fn load_errors(text: &str) -> Vec<Diagnostic> {
  match Program::load(Source::new("t.shs", text), &catalog(), &no_defines()) {
    Ok(_) => panic!("expected load errors"),
    Err((_, d)) => d,
  }
}

fn at(d: &Diagnostic) -> (u32, u32) {
  (d.line.unwrap(), d.column.unwrap())
}

use otter_core::Mesh;

fn run(text: &str, defines: &HashMap<String, String>) -> otter_lang::RunReport {
  let program = match Program::load(Source::new("t.shs", text), &catalog(), defines) {
    Ok(p) => p,
    Err((_, d)) => panic!("load failed: {d:?}"),
  };
  program
    .run()
    .unwrap_or_else(|d| panic!("run failed: {d:?}"))
}

fn check(text: &str) -> otter_lang::CheckReport {
  otter_lang::check(Source::new("t.shs", text), &catalog(), &no_defines())
}

fn completed(report: &otter_lang::RunReport, wire: &str) -> Var {
  match report.outcomes.iter().find(|(w, _)| w == wire) {
    Some((_, Some(Outcome::Completed(v)))) => v.clone(),
    other => panic!("{wire}: {other:?}"),
  }
}

#[test]
fn constructor_segments_preserve_snapshots_and_change_table_shapes() {
  let report = run(
    "1 | Var(n)\n{z: n a: 2}\n{a: 3 z: n} | Var(saved)\n4 | Update(n)\n{b: n a: 5}\n{a: n b: 6} | Var(result)\n[saved result]",
    &no_defines(),
  );
  assert_eq!(
    completed(&report, "root"),
    Var::from_seq(std::sync::Arc::new(vec![
      Var::table([("a", Var::Int(3)), ("z", Var::Int(1))]),
      Var::table([("a", Var::Int(4)), ("b", Var::Int(6))]),
    ]))
  );
  let report = run("1 | Var(n)\n{z: n a: 2}\n{c: n}", &no_defines());
  assert_eq!(completed(&report, "root"), Var::table([("c", Var::Int(1))]));
}

#[test]
fn collection_outputs_keep_saved_snapshots() {
  for (expression, expected) in [
    ("[n n]", "[[1 1] [2 2] [3 3]]"),
    ("{a: n b: n}", "[{a: 1 b: 1} {a: 2 b: 2} {a: 3 b: 3}]"),
  ] {
    let source = format!(
      "0 | Var(n)\n[] | Var(saved)\nRepeat({{ Inc(n) {expression} | Push(saved) }} times: 3)\nsaved | Is({expected})"
    );
    assert_eq!(
      completed(&run(&source, &no_defines()), "root"),
      Var::Bool(true)
    );
  }
}

fn reload(session: &mut otter_lang::Session, text: &str) -> Vec<otter_lang::Finished> {
  session
    .reload(Source::new("reload.shs", text), &catalog(), &no_defines())
    .unwrap_or_else(|(_, d)| panic!("reload failed: {d:?}"))
}

fn preserve(session: &mut otter_lang::Session, text: &str) -> Vec<otter_lang::Finished> {
  session
    .reload_preserving(Source::new("preserve.shs", text), &catalog(), &no_defines())
    .unwrap_or_else(|(_, d)| panic!("preserving reload failed: {d:?}"))
}

#[test]
fn preserving_reload_pins_a_suspended_invocation_and_keeps_the_caller_counter() {
  let source = |value| {
    format!(
      r#"@fn(Inner input: None output: Int params: {{}} {{ Pause() {value} }})
@fn(Outer input: None output: Int params: {{}} {{ Inner }})
@wire(main {{ Keep(n 0) Inc(n) Log Outer Log }} looped: true)
@mesh(m) @schedule(m main) @run(m)"#
    )
  };
  let mut session = otter_lang::Session::new();
  preserve(&mut session, &source(10));
  let (_, lines) = otter_core::log::capture(|| {
    session.tick(); // n=1, paused inside inner.
    assert!(preserve(&mut session, &source(20)).is_empty());
    session.tick(); // old call returns 10.
    session.tick(); // n=2, enters new inner.
    session.tick(); // new call returns 20.
    assert!(preserve(&mut session, &source(30)).is_empty());
    session.tick();
    session.tick();
  });
  assert_eq!(lines, ["1", "10", "2", "20", "3", "30"]);
}

#[test]
fn preserving_reload_updates_deep_calls_inside_a_never_returning_parent() {
  let source = |value| {
    format!(
      r#"@fn(Inner input: None output: Int params: {{}} {{ {value} Log Pause() }})
@fn(Outer stateful: true input: None output: None params: {{}} {{ Keep(n 0) Repeat({{ Inc(n) Log Inner }} forever: true) }})
Outer"#
    )
  };
  let mut session = otter_lang::Session::new();
  preserve(&mut session, &source(10));
  let (_, lines) = otter_core::log::capture(|| {
    session.tick();
    assert!(preserve(&mut session, &source(20)).is_empty());
    session.tick();
    assert!(preserve(&mut session, &source(30)).is_empty());
    session.tick();
  });
  assert_eq!(lines, ["1", "10", "2", "20", "3", "30"]);
}

#[test]
fn preserving_reload_keeps_unrelated_wires_and_mesh_values() {
  let source = |value| {
    format!(
      r#"@wire(main {{ Inc(shared) Log {value} Log }} looped: true)
@wire(ticker {{ Keep(n 100) Inc(n) Log }} looped: true)
@mesh(m) @schedule(m main) @schedule(m ticker) @run(m)"#
    )
  };
  let mut mesh = Mesh::new();
  mesh.declare_var("shared", Var::Int(0), true);
  let mut session = otter_lang::Session::with_mesh(mesh);
  preserve(&mut session, &source(10));
  let (_, lines) = otter_core::log::capture(|| {
    session.tick();
    let ended = preserve(&mut session, &source(20));
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].wire, "main");
    session.tick();
  });
  // Retained instances keep their scheduler order; replacements append.
  assert_eq!(lines, ["1", "10", "101", "102", "2", "20"]);
}

/// Editing a function that was inlined into a running wire gives the
/// wire's instances the new body at their next iteration, with `Keep`
/// slots carried over; the reload report lists the wire as swapped, not
/// restarted.
#[test]
fn editing_an_inlined_function_swaps_the_callers_body_and_keeps_its_state() {
  let source = |value| {
    format!(
      r#"@fn(Step input: Int output: Int params: {{}} {{ Math.Add({value}) }})
@wire(main {{ Keep(n 0) n | Step | Update(n) Log }} looped: true)
@mesh(m) @schedule(m main) @run(m)"#
    )
  };
  let mut session = otter_lang::Session::new();
  preserve(&mut session, &source(1));
  let (_, lines) = otter_core::log::capture(|| {
    session.tick();
    session.tick();
    let ended = preserve(&mut session, &source(10));
    assert!(ended.is_empty(), "the instance is kept, not restarted");
    let report = session.reload_report().unwrap();
    assert_eq!(report.swapped, ["main"]);
    assert!(report.restarted.is_empty());
    session.tick();
  });
  assert_eq!(lines, ["1", "2", "12"]);
}

/// A wire suspended in the middle of an iteration finishes it on the body
/// it started with (golden path §11): the swap waits for the iteration's
/// end, and the state of components it calls is carried over.
#[test]
fn a_swap_waits_for_the_iteration_in_flight_and_keeps_component_state() {
  let source = |value| {
    format!(
      r#"@fn(Counter stateful: true input: None output: Int params: {{}} {{ Keep(n 0) Inc(n) }})
@fn(Step input: Int output: Int params: {{}} {{ Math.Add({value}) }})
@wire(main {{ Counter | Log("c") Pause() 1 | Step | Log("b") }} looped: true)
@mesh(m) @schedule(m main) @run(m)"#
    )
  };
  let mut session = otter_lang::Session::new();
  preserve(&mut session, &source(1));
  let (_, lines) = otter_core::log::capture(|| {
    session.tick();
    let ended = preserve(&mut session, &source(10));
    assert!(ended.is_empty());
    let report = session.reload_report().unwrap();
    assert_eq!(report.swapped, ["main"]);
    assert!(report.reset.is_empty() && report.restarted.is_empty());
    for _ in 0..3 {
      session.tick();
    }
  });
  assert_eq!(lines, ["c: 1", "b: 2", "c: 2", "b: 11"]);
}

/// A run of a wire that does not loop finishes on its body: it is not
/// swapped, cut or restarted.
#[test]
fn a_run_that_does_not_loop_finishes_on_the_body_it_started() {
  let source = |value| {
    format!(
      r#"@fn(Step input: Int output: Int params: {{}} {{ Math.Add({value}) }})
@wire(main {{ Log("a") Pause() 1 | Step | Log("b") }})
@mesh(m) @schedule(m main) @run(m)"#
    )
  };
  let mut session = otter_lang::Session::new();
  preserve(&mut session, &source(1));
  let (_, lines) = otter_core::log::capture(|| {
    session.tick();
    let ended = preserve(&mut session, &source(10));
    assert!(ended.is_empty());
    assert!(session.reload_report().unwrap().is_empty());
    session.tick();
    session.tick();
  });
  assert_eq!(lines, ["a: none", "b: 2"]);
}

/// `Once(every:)` runs its action again once the interval has passed since
/// its last run started, and not before.
#[test]
fn once_every_runs_again_after_its_interval() {
  let mut session = otter_lang::Session::new();
  preserve(
    &mut session,
    r#"@wire(main { Once({ Log("run") } every: 0.4) } looped: true)
@mesh(m) @schedule(m main) @run(m)"#,
  );
  let (_, lines) = otter_core::log::capture(|| {
    for _ in 0..3 {
      session.tick();
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
    session.tick();
    session.tick();
  });
  assert_eq!(lines, ["run: none", "run: none"]);
  let d = load_or_compose_errors(r#"Once({1} every: 0)"#);
  assert_eq!(d[0].code, "invalid-argument-value");
}

/// Swapping a body keeps the state of the wire's own nodes and of the
/// components it calls: a `Once` does not run again, a stateful callee
/// keeps counting, under the default policy that rejects resets.
#[test]
fn a_swap_keeps_once_flags_and_component_state_under_reject() {
  let source = |delta| {
    format!(
      r#"@fn(Counter stateful: true input: None output: Int params: {{}} {{ Keep(n 0) Inc(n) }})
@fn(Bump input: Int output: Int params: {{}} {{ Math.Add({delta}) }})
@wire(main {{ Once({{ Log("once") }}) Counter Log Bump }} looped: true)
@mesh(m) @schedule(m main) @run(m)"#
    )
  };
  let mut session = otter_lang::Session::new();
  preserve(&mut session, &source(1));
  let (_, lines) = otter_core::log::capture(|| {
    session.tick();
    session.tick();
    preserve(&mut session, &source(10));
    let report = session.reload_report().unwrap();
    assert_eq!(report.swapped, ["main"]);
    assert!(report.reset.is_empty(), "{report:?}");
    session.tick();
  });
  assert_eq!(lines, ["once: none", "1", "2", "3"]);
}

/// An edit that takes an inlined callee past the inlining limits (here its
/// length) turns its sites into calls; the running wire moves onto that
/// body with its state.
#[test]
fn a_callee_that_stops_being_inlined_keeps_the_callers_state() {
  let source = |body: &str| {
    format!(
      r#"@fn(Counter stateful: true input: None output: Int params: {{}} {{ Keep(n 0) Inc(n) }})
@fn(Step input: Int output: Int params: {{}} {{ {body} }})
@wire(main {{ Counter | Step | Log }} looped: true)
@mesh(m) @schedule(m main) @run(m)"#
    )
  };
  let mut session = otter_lang::Session::new();
  preserve(&mut session, &source("Math.Add(100)"));
  let (_, lines) = otter_core::log::capture(|| {
    session.tick();
    preserve(&mut session, &source(&"Math.Add(8) ".repeat(25)));
    assert_eq!(session.reload_report().unwrap().swapped, ["main"]);
    session.tick();
    session.tick();
  });
  assert_eq!(lines, ["101", "202", "203"]);
}

/// A callee that keeps its frame (here it suspends) is selected at its
/// call sites' next entry; a wire spawning a wire that calls it is not
/// swapped, cut or restarted by the edit.
#[test]
fn editing_a_framed_callee_leaves_the_spawner_alone() {
  let source = |value| {
    format!(
      r#"@fn(F input: None output: Int params: {{}} {{ Pause() {value} }})
@wire(child {{ F | Log("child") }})
@wire(main {{ Spawn(child) Log("a") Pause() Log("b") }})
@mesh(m) @schedule(m main) @run(m)"#
    )
  };
  let mut session = otter_lang::Session::new();
  preserve(&mut session, &source(1));
  let (_, lines) = otter_core::log::capture(|| {
    session.tick();
    preserve(&mut session, &source(10));
    assert!(session.reload_report().unwrap().is_empty());
    for _ in 0..3 {
      session.tick();
    }
  });
  assert_eq!(
    lines.iter().filter(|l| *l == "a: none").count(),
    1,
    "{lines:?}"
  );
  assert_eq!(
    lines.iter().filter(|l| *l == "b: none").count(),
    1,
    "{lines:?}"
  );
  assert_eq!(
    lines.iter().filter(|l| l.starts_with("child")).count(),
    1,
    "{lines:?}"
  );
}

#[test]
fn preserving_reload_rejects_incompatible_function_interfaces_atomically() {
  let source = |signature, body| {
    format!(
      r#"@fn(Inner {signature} {{ {body} }})
@wire(main {{ Keep(n 0) Inc(n) Log Inner | ToString Log }} looped: true)
@mesh(m) @schedule(m main) @run(m)"#
    )
  };
  let mut session = otter_lang::Session::new();
  preserve(
    &mut session,
    &source("input: None output: Int params: {}", "10"),
  );
  let (_, lines) = otter_core::log::capture(|| {
    session.tick();
    for (signature, body) in [
      ("input: None output: String params: {}", r#""new type""#),
      ("input: None output: Int params: {by: 1}", "20"),
    ] {
      let (_, d) = session
        .reload_preserving(
          Source::new("bad.shs", source(signature, body)),
          &catalog(),
          &no_defines(),
        )
        .unwrap_err();
      assert_eq!(d[0].code, "reload-incompatible");
      assert_eq!(d[0].line, Some(1));
      session.tick();
    }
    // A stateless callee may change its locals freely.
    preserve(
      &mut session,
      &source("input: None output: Int params: {}", "1 = new-local 20"),
    );
    session.tick();
  });
  assert_eq!(lines, ["1", "10", "2", "10", "3", "10", "4", "20"]);
}

#[test]
fn preserving_reload_explains_and_locates_admission_failures() {
  let source = |signature: &str, body: &str| {
    format!(
      "@fn(Outer stateful: true input: None output: Any params: {{}} {{ Step }})\n@fn(Step {signature} {{\n{body}\n}})\n@wire(main {{ Outer Log }} looped: true)\n@mesh(m) @schedule(m main) @run(m)"
    )
  };
  // Defaults keep the unchanged caller composing, so admission decides.
  let base = "input: None output: Int params: {by: 1}";
  for (signature, body, reason) in [
    (
      "input: None output: String params: {by: 1}",
      "\"ten\"",
      "output type changed from Int to String",
    ),
    (
      "input: None output: Int params: {amount: 1}",
      "10",
      "parameter `by` became `amount`",
    ),
    (
      "input: None output: Int params: {by: 1.5}",
      "10",
      "parameter `by` changed type from Int to Float",
    ),
    (
      "input: None output: Int params: {by: 2}",
      "10",
      "parameter `by` changed its default",
    ),
    (
      "input: None output: Int params: {by: 1 extra: 2}",
      "10",
      "new parameter `extra`",
    ),
    (
      "input: None output: Int params: {}",
      "10",
      "parameter `by` was removed",
    ),
    (
      "stateful: true input: None output: Int params: {by: 1}",
      "10",
      "it became stateful",
    ),
    (base, "Pause 10", "it gained the effect `suspends`"),
  ] {
    let mut session = otter_lang::Session::new();
    preserve(&mut session, &source(base, "10"));
    session.tick();
    let (source, diagnostics) = session
      .reload_preserving(
        Source::new("edit.shs", source(signature, body)),
        &catalog(),
        &no_defines(),
      )
      .unwrap_err();
    let d = &diagnostics[0];
    assert_eq!(d.code, "reload-incompatible", "{}", d.message);
    assert!(d.message.contains(reason), "{}", d.message);
    assert!(d.message.contains("press r in watch"));
    // The unchanged caller keeps its body; the diagnostic points at the
    // edited function's declaration.
    assert_eq!(d.shard.as_deref(), Some("Step"), "{}", d.message);
    assert_eq!(d.line, Some(2), "{}", otter_lang::render(d, &source));
  }
}

#[cfg(not(any(target_arch = "wasm32", target_os = "espidf")))]
#[test]
fn file_watcher_callbacks_preserve_reject_restart_and_stop() {
  use otter_lang::{FileWatcher, WatchControl, WatchEvent};
  use std::cell::Cell;
  use std::time::{Duration, Instant};
  let path = std::env::temp_dir().join(format!(
    "shards-watch-{}-{}.shs",
    std::process::id(),
    module_path!().replace("::", "-")
  ));
  struct Remove(std::path::PathBuf);
  impl Drop for Remove {
    fn drop(&mut self) {
      let _ = std::fs::remove_file(&self.0);
    }
  }
  let _remove = Remove(path.clone());
  let source = |value| {
    format!(
      "@fn(Step input: None output: Int params: {{}} {{{value} Log}})\n@wire(main {{Keep(n 0) Inc(n) Log Step}} looped: true)\n@mesh(m) @schedule(m main) @run(m fps: 0.1)"
    )
  };
  std::fs::write(&path, source(10)).unwrap();
  let command = Cell::new(WatchControl::Continue);
  let mut revisions = 0;
  let mut rejected = 0;
  let mut ticks = 0;
  let mut stopped = false;
  let start = Instant::now();
  let mut session = otter_lang::Session::new();
  let (_, lines) = otter_core::log::capture(|| {
    FileWatcher::new(&path).run(
      &mut session,
      &catalog(),
      &no_defines(),
      || {
        assert!(
          start.elapsed() < Duration::from_secs(5),
          "watcher did not exit"
        );
        command.replace(WatchControl::Continue)
      },
      |event| match event {
        WatchEvent::Reloaded { restarted, .. } => {
          revisions += 1;
          match revisions {
            1 => {
              assert!(!restarted);
              std::fs::write(&path, "Missing.Shard").unwrap();
            }
            2 => {
              assert!(!restarted);
              command.set(WatchControl::Restart);
            }
            3 => {
              assert!(restarted);
              command.set(WatchControl::Stop);
            }
            _ => panic!("unexpected reload"),
          }
        }
        WatchEvent::Rejected {
          source: rejected_source,
          diagnostics,
        } => {
          rejected += 1;
          assert_eq!(rejected_source.text, "Missing.Shard");
          assert!(!diagnostics.is_empty());
          // Atomic saves exercise content comparison independent of mtime.
          let replacement = path.with_extension("new");
          std::fs::write(&replacement, source(20)).unwrap();
          std::fs::rename(replacement, &path).unwrap();
        }
        WatchEvent::Tick(_) => ticks += 1,
        WatchEvent::Stopped(finished) => {
          stopped = true;
          assert!(!finished.is_empty());
        }
        WatchEvent::ReadError(error) => panic!("{error}"),
      },
    )
  });
  assert_eq!(revisions, 3);
  assert_eq!(rejected, 1);
  assert!(ticks >= 3);
  assert!(stopped);
  assert_eq!(lines, ["1", "10", "2", "20", "1", "20"]);
}

#[test]
fn preserving_reload_keeps_unchanged_component_state_and_other_sessions() {
  let source = |value| {
    format!(
      r#"@fn(Stable stateful: true input: None output: Int params: {{}} {{Keep(n 0) Inc(n) Log}})
@fn(Changed input: None output: Int params: {{}} {{{value} Log}})
@wire(main {{Stable Changed}} looped: true)
@mesh(m) @schedule(m main) @run(m)"#
    )
  };
  let mut first = otter_lang::Session::new();
  let mut second = otter_lang::Session::new();
  preserve(&mut first, &source(10));
  preserve(&mut second, &source(10));
  let (_, lines) = otter_core::log::capture(|| {
    first.tick();
    second.tick();
    preserve(&mut first, &source(20));
    first.tick();
    second.tick();
  });
  assert_eq!(lines, ["1", "10", "1", "10", "2", "20", "2", "10"]);
}

#[test]
fn preserving_reload_retries_failed_nested_instantiation_after_maybe() {
  let source = |body| {
    format!(
      r#"@fn(Inner input: None output: Int params: {{}} {{{body}}})
@wire(main {{Keep(n 0) Inc(n) Log Maybe({{Inner}} silent: true)}} looped: true)
@mesh(m) @schedule(m main) @run(m)"#
    )
  };
  let mut session = otter_lang::Session::new();
  preserve(&mut session, &source(r#"Probe("ok") 10"#));
  let (_, lines) = otter_core::log::capture(|| {
    session.tick();
    preserve(
      &mut session,
      &source(r#"Probe("bad" "fail-instantiate") 10"#),
    );
    assert!(session.tick().is_empty());
    assert!(session.tick().is_empty());
    preserve(&mut session, &source(r#"Probe("ok") 20"#));
    assert!(session.tick().is_empty());
  });
  assert_eq!(lines, ["1", "2", "3", "4"]);
  assert_eq!(session.running(), 1);
}

#[test]
fn an_in_flight_parent_selects_the_newest_callee_at_its_next_entry() {
  let source = |outer: &str, value| {
    format!(
      r#"@fn(Inner input: None output: Int params: {{}} {{{value}}})
@fn(Outer input: None output: Int params: {{}} {{{outer}}})
@wire(main {{Outer Log}} looped: true)
@mesh(m) @schedule(m main) @run(m)"#
    )
  };
  let mut session = otter_lang::Session::new();
  preserve(&mut session, &source("Pause() Inner", 10));
  let (_, lines) = otter_core::log::capture(|| {
    session.tick();
    preserve(&mut session, &source("Pause() 0 Inner", 20));
    // `Inner` is small and straight-line, so it is inlined into `Outer`
    // at compose: the old Outer invocation finishes on its pinned body,
    // Inner included, and the next Outer entry selects the recomposed
    // Outer with the new Inner (§11 as it applies to inlined sites).
    session.tick();
    session.tick();
    session.tick();
  });
  assert_eq!(lines, ["10", "20"]);
}

/// A callee that keeps its frames (it suspends) is selected at its own
/// entry, even inside an in-flight caller (§11).
#[test]
fn an_in_flight_parent_selects_the_newest_framed_callee_at_its_next_entry() {
  let source = |outer: &str, value| {
    format!(
      r#"@fn(Inner input: None output: Int params: {{}} {{Pause() {value}}})
@fn(Outer input: None output: Int params: {{}} {{{outer}}})
@wire(main {{Outer Log}} looped: true)
@mesh(m) @schedule(m main) @run(m)"#
    )
  };
  let mut session = otter_lang::Session::new();
  preserve(&mut session, &source("Pause() Inner", 10));
  let (_, lines) = otter_core::log::capture(|| {
    session.tick();
    preserve(&mut session, &source("Pause() 0 Inner", 20));
    for _ in 0..5 {
      session.tick();
    }
  });
  assert_eq!(lines, ["20", "20"]);
}

#[test]
fn preserving_reload_does_not_renumber_other_wires_temporaries() {
  let source = |body| {
    format!(
      r#"@wire(changed {{{body}}})
@wire(ticker {{Keep(n 0) Inc(n) Add(0 | Add(1)) Log}} looped: true)
@mesh(m) @schedule(m changed) @schedule(m ticker) @run(m)"#
    )
  };
  let mut session = otter_lang::Session::new();
  preserve(&mut session, &source("1"));
  let (_, lines) = otter_core::log::capture(|| {
    session.tick();
    preserve(&mut session, &source("1 Add(0 | Add(2))"));
    session.tick();
  });
  assert_eq!(lines, ["2", "3"]);
}

#[cfg(panic = "unwind")]
#[test]
fn preserving_reload_attempts_cleanup_once_when_boundary_cleanup_panics() {
  use otter_core::shards::{ProbeEventKind, take_probe_events};
  let source = |body| {
    format!(
      r#"@fn(Inner stateful: true input: None output: Int params: {{}} {{{body}}})
@wire(main {{Probe("parent") Inner}} looped: true)
@mesh(m) @schedule(m main) @run(m)"#
    )
  };
  take_probe_events();
  let mut session = otter_lang::Session::new();
  session.set_reset_policy(otter_core::ResetPolicy::Apply);
  preserve(&mut session, &source(r#"Probe("old" "panic-cleanup") 10"#));
  session.tick();
  take_probe_events();
  preserve(&mut session, &source("20"));
  let finished = session.tick();
  assert!(matches!(finished[0].outcome, Outcome::Failed(_)));
  let events = take_probe_events();
  for tag in ["old", "parent"] {
    assert_eq!(
      events
        .iter()
        .filter(|e| e.tag == tag && e.kind == ProbeEventKind::Cleanup)
        .count(),
      1
    );
  }
  assert!(session.stop().is_empty());
}

#[test]
fn preserving_reload_updates_retained_spawned_children() {
  let source = |input, amount| {
    format!(
      r#"@fn(Inner input: Int output: Int params: {{}} {{Add({amount})}})
@wire(child {{Inner Log Pause()}} looped: true)
@wire(main {{{input} Spawn(child)}})
@mesh(m) @schedule(m main) @run(m)"#
    )
  };
  let mut session = otter_lang::Session::new();
  preserve(&mut session, &source("1", 1));
  let (_, lines) = otter_core::log::capture(|| {
    session.tick(); // Spawn the child.
    session.tick(); // Child prints 2 and pauses.
    preserve(&mut session, &source("10", 2));
    session.tick(); // Old child completes its iteration; new main spawns another.
    session.tick(); // Both children select the new Inner.
  });
  assert_eq!(lines, ["2", "3", "12"]);
}

#[test]
fn preserving_reload_recovers_failed_entries_on_the_next_accepted_save() {
  let source = |divisor| {
    format!(
      r#"@fn(Inner input: None output: Int params: {{}} {{1 Div({divisor})}})
@wire(main {{Inner}})
@mesh(m) @schedule(m main) @run(m)"#
    )
  };
  let mut session = otter_lang::Session::new();
  preserve(&mut session, &source(0));
  // `Inner` is inlined and fails at its first instruction, which follows
  // flat control code: a plain failure, not a panic in the engine.
  let Outcome::Failed(err) = &session.tick()[0].outcome else {
    panic!("expected the division to fail");
  };
  assert!(!err.to_string().contains("panic"), "{err}");
  assert!(session.tick().is_empty());
  preserve(&mut session, &source(1));
  assert!(matches!(
    session.tick()[0].outcome,
    Outcome::Completed(Var::Int(1))
  ));
}

#[test]
fn preserving_reload_removes_scheduled_callers_before_checking_their_interface() {
  let source = |value, scheduled| {
    format!(
      r#"@fn(Inner input: None output: Int params: {{}} {{{value}}})
@wire(main {{Inner Log Pause()}} looped: true)
@wire(other {{Pause()}} looped: true)
@mesh(m) @schedule(m {scheduled}) @run(m)"#
    )
  };
  let mut session = otter_lang::Session::new();
  preserve(&mut session, &source("10", "main"));
  session.tick();
  let finished = preserve(&mut session, &source(r#""new type" | Count"#, "other"));
  assert_eq!(finished.len(), 1);
  assert_eq!(finished[0].wire, "main");
  assert!(matches!(finished[0].outcome, Outcome::Cancelled));
  assert!(session.tick().is_empty());
}

#[test]
fn preserving_reload_instantiates_restarted_roots_unchanged_children_once() {
  use otter_core::shards::{ProbeEventKind, take_probe_events};
  let source = |value| {
    format!(
      r#"@fn(Inner stateful: true input: Int output: Int params: {{}} {{Probe("child")}})
@wire(main {{{value} Inner}} looped: true)
@mesh(m) @schedule(m main) @run(m)"#
    )
  };
  take_probe_events();
  let mut session = otter_lang::Session::new();
  preserve(&mut session, &source(1));
  session.tick();
  preserve(&mut session, &source(2));
  take_probe_events();
  session.tick();
  let events = take_probe_events();
  assert_eq!(
    events
      .iter()
      .filter(|e| e.kind == ProbeEventKind::Instantiate)
      .count(),
    1
  );
  assert_eq!(
    events
      .iter()
      .filter(|e| e.kind == ProbeEventKind::Cleanup)
      .count(),
    0
  );
}

#[test]
fn preserving_reload_does_not_repeat_unchanged_completed_effects() {
  let mut session = otter_lang::Session::new();
  let source = r#"@wire(setup {"setup" Log})
@wire(ticker {Pause()} looped: true)
@mesh(m) @schedule(m setup) @schedule(m ticker) @run(m)"#;
  preserve(&mut session, source);
  let (_, lines) = otter_core::log::capture(|| {
    session.tick();
    preserve(&mut session, &format!("// only a comment\n{source}"));
    session.tick();
  });
  assert_eq!(lines, ["setup"]);
}

#[test]
fn reload_rejects_bad_edits_without_resetting_the_running_program() {
  let mut session = otter_lang::Session::new();
  reload(
    &mut session,
    r#"@wire(tick { Keep(n 0) Inc(n) Log } looped: true)
@mesh(m) @schedule(m tick) @run(m fps: 30)"#,
  );
  let (_, lines) = otter_core::log::capture(|| {
    session.tick();
    for bad in [
      "When({",
      "No.SuchShard",
      "1 | Take(0)",
      "@wire(unused { missing }) 42",
    ] {
      let (_, d) = session
        .reload(Source::new("bad.shs", bad), &catalog(), &no_defines())
        .unwrap_err();
      assert!(!d.is_empty());
      assert_eq!(d[0].file.as_deref(), Some("bad.shs"));
      assert!(d[0].line.is_some());
      session.tick();
    }
  });
  assert_eq!(lines, ["1", "2", "3", "4", "5"]);
  assert_eq!(session.ticks(), 5);
  assert_eq!(
    session.frame_interval(),
    Some(std::time::Duration::from_secs_f64(1.0 / 30.0))
  );
  assert_eq!(reload(&mut session, "42").len(), 1);
  assert_eq!(session.ticks(), 0);
  let finished = session.tick();
  assert!(matches!(
    finished[0].outcome,
    Outcome::Completed(Var::Int(42))
  ));
  assert!(session.tick().is_empty());
  assert_eq!(session.ticks(), 1);
}

#[test]
fn reload_cancels_nested_flows_and_spawned_children_before_new_activation() {
  use otter_core::shards::{ProbeEventKind, take_probe_events};
  take_probe_events();
  let mut session = otter_lang::Session::new();
  reload(
    &mut session,
    r#"@wire(child { Probe("child") Pause(1000.0) })
@fn(Inner input: None output: None params: {} { Probe("inner") Pause(1000.0) })
@wire(main { Spawn(child) Inner })
@mesh(m) @schedule(m main) @run(m)"#,
  );
  session.tick();
  session.tick();
  take_probe_events();
  let stopped = reload(&mut session, r#"Probe("new") 42"#);
  assert_eq!(stopped.len(), 2);
  assert!(
    stopped
      .iter()
      .all(|f| matches!(f.outcome, Outcome::Cancelled))
  );
  let events = take_probe_events();
  assert_eq!(events.len(), 2);
  assert!(events.iter().all(|e| e.kind == ProbeEventKind::Cleanup));
  assert!(events.iter().any(|e| e.tag == "child"));
  assert!(events.iter().any(|e| e.tag == "inner"));
  session.tick();
  let events = take_probe_events();
  assert!(events.iter().all(|e| e.tag == "new"));
  assert_eq!(
    events
      .iter()
      .filter(|e| e.kind == ProbeEventKind::Cleanup)
      .count(),
    1
  );
  assert!(session.stop().is_empty());
  assert!(session.stop().is_empty());
}

#[test]
fn reload_recompiles_changed_callees_and_resets_once_and_locals() {
  let mut session = otter_lang::Session::new();
  for value in [10, 20, 30] {
    reload(
      &mut session,
      &format!(
        r#"@fn(Value input: None output: Int params: {{}} {{ {value} }})
@wire(main {{ Keep(n 0) Once({{Value | Update(n)}}) Inc(n) Log }} looped: true)
@mesh(m) @schedule(m main) @run(m iterations: 2)"#
      ),
    );
    let (finished, lines) = otter_core::log::capture(|| {
      assert!(session.tick().is_empty());
      session.tick()
    });
    assert_eq!(lines, [(value + 1).to_string(), (value + 2).to_string()]);
    assert!(matches!(finished[0].outcome, Outcome::Cancelled));
    assert_eq!(session.running(), 0);
    assert!(session.tick().is_empty());
  }
}

#[test]
fn session_reports_spawned_failures_once() {
  let mut session = otter_lang::Session::new();
  reload(&mut session, r#"@wire(child { 1 | Div(0) }) Spawn(child)"#);
  let entries = session.tick();
  assert_eq!(entries.len(), 1);
  let children = session.tick();
  assert_eq!(children.len(), 1);
  assert_eq!(children[0].wire, "child");
  assert!(matches!(children[0].outcome, Outcome::Failed(_)));
  assert!(session.tick().is_empty());
}

#[cfg(panic = "unwind")]
#[test]
fn reload_reports_cleanup_failure_and_still_cleans_other_instances() {
  use otter_core::shards::{ProbeEventKind, take_probe_events};
  take_probe_events();
  let mut session = otter_lang::Session::new();
  reload(
    &mut session,
    r#"@wire(a { Probe("a" "panic-cleanup") Pause(1000.0) })
@wire(b { Probe("b") Pause(1000.0) })
@mesh(m) @schedule(m a) @schedule(m b) @run(m)"#,
  );
  session.tick();
  take_probe_events();
  let finished = reload(&mut session, "42");
  assert!(matches!(finished[0].outcome, Outcome::Failed(_)));
  assert!(matches!(finished[1].outcome, Outcome::Cancelled));
  assert_eq!(
    take_probe_events()
      .iter()
      .filter(|e| e.kind == ProbeEventKind::Cleanup)
      .count(),
    2
  );
  assert!(matches!(
    session.tick()[0].outcome,
    Outcome::Completed(Var::Int(42))
  ));
}

#[test]
fn esp32_firmware_script_completes_after_suspending() {
  let report = run(
    include_str!("../../../examples/esp32/smoke.shs"),
    &no_defines(),
  );
  assert_eq!(report.outcomes.len(), 1);
  assert_eq!(completed(&report, "root"), Var::Int(42));
  assert!(report.spawned_failures.is_empty());
  assert!(report.ticks >= 2);
}

#[test]
fn loose_code_runs_as_the_root_wire() {
  let report = run(
    "// word forms and operators
0 | Var(n)
Repeat({Inc(n)} times: 3)
n | Add(10) = result
When({result | IsMoreEqual(13)} {
  result | Add(1) | Update(n)
  })
n",
    &no_defines(),
  );
  assert_eq!(completed(&report, "root"), Var::Int(14));
}

#[test]
fn declared_wires_run_on_the_scheduled_mesh() {
  let report = run(
    "@fn(AddOne input: Int output: Int params: {} { Add(1) })
@wire(answer { 41 | AddOne })
@wire(ticker { Keep(ticks 0) Inc(ticks) } looped: true)
@mesh(main)
@schedule(main answer)
@schedule(main ticker)
@run(main iterations: 5)",
    &no_defines(),
  );
  assert_eq!(completed(&report, "answer"), Var::Int(42));
  // The looped wire is still running when the iteration limit stops it.
  assert_eq!(report.ticks, 5);
  assert!(matches!(
    report.outcomes.iter().find(|(w, _)| w == "ticker"),
    Some((_, None))
  ));
}

#[test]
fn literals_tables_and_script_arguments() {
  let mut defines = HashMap::new();
  defines.insert("who".to_string(), "world".to_string());
  let report = run(
    "{name: @who count: 2 ratio: 2.5e-1 tags: [\"a\" \"b\"]} = t
// `{}` is an empty flow where a flow is expected
When({true} {})
t",
    &defines,
  );
  assert_eq!(
    completed(&report, "root"),
    Var::table([
      ("name", Var::string("world")),
      ("count", Var::Int(2)),
      ("ratio", Var::Float(0.25)),
      (
        "tags",
        Var::from_seq(std::sync::Arc::new(vec![
          Var::string("a"),
          Var::string("b")
        ]))
      ),
    ])
  );
}

#[test]
fn compose_errors_point_at_the_failing_source() {
  let report = check(
    "@fn(Helper input: Int output: Int params: {} {
  When({true} {
    1 | Add(2)
    \"a\" | Add(2)
  })
  })
@wire(main-wire { 0 | Helper })
@mesh(main)
@schedule(main main-wire)
@run(main)",
  );
  assert!(!report.ok());
  let d = &report.diagnostics[0];
  assert_eq!(d.code, "input-type-mismatch");
  // The second Add, inside the called wire's When body.
  assert_eq!(at(d), (4, 11));
  assert_eq!(
    d.path_string(),
    "main-wire/1:Helper/Helper()/0:When/action/3:Math.Add"
  );
  let json = report.to_json();
  assert!(
    json.starts_with("{\"ok\":false,\"file\":\"t.shs\",\"diagnostics\":[{"),
    "{json}"
  );
  assert!(json.contains("\"line\":4,\"column\":11"), "{json}");
}

#[test]
fn variable_errors_point_at_the_assignment() {
  let report = check("1 = x\n2 | Update(x)");
  let d = &report.diagnostics[0];
  assert_eq!(d.code, "immutable-binding");
  // At the Update that fails; the binding is the related location.
  assert_eq!(at(d), (2, 5));
  let related = d.related.as_ref().unwrap();
  assert_eq!((related.line, related.column), (Some(1), Some(3)));
}

#[test]
fn paths_read_tables_and_sequences() {
  let report = run(
    "{a: [10 20] b: {c: \"deep\"}} = t\nt.a.1 = second\nt.b.c = deep\n[second deep]",
    &no_defines(),
  );
  assert_eq!(
    completed(&report, "root"),
    Var::from_seq(std::sync::Arc::new(vec![Var::Int(20), Var::string("deep")]))
  );
  // A typo on a fixed table is a compose error at the key, with suggestions.
  let report = check("{name: 1 count: 2} = t\nt.cuont");
  let d = &report.diagnostics[0];
  assert_eq!(d.code, "unknown-key");
  assert_eq!(d.did_you_mean, ["count"]);
  assert_eq!(at(d), (2, 3));
}

#[test]
fn f_strings_and_computed_values() {
  let report = run(
    "5 = n
f\"n is {n}, next {n | Add(1)}, {{literal}}\" = text
[1 n n | Add(1)] = items
{a: n b: n | Add(10)} = table
// A computed parameter is computed before the shard; the input flows on.
1 | Add(n | Add(5)) = sum
[text items table sum]",
    &no_defines(),
  );
  let seq = |v: Vec<Var>| Var::from_seq(std::sync::Arc::new(v));
  assert_eq!(
    completed(&report, "root"),
    seq(vec![
      Var::string("n is 5, next 6, {literal}"),
      seq(vec![Var::Int(1), Var::Int(5), Var::Int(6)]),
      Var::table([("a", Var::Int(5)), ("b", Var::Int(15))]),
      Var::Int(11),
    ])
  );
}

#[test]
fn push_appends_to_a_sequence() {
  let report = run(
    "[] | Var(xs)\n1 | Push(xs)\n2 | Push(xs)\nxs",
    &no_defines(),
  );
  assert_eq!(
    completed(&report, "root"),
    Var::from_seq(std::sync::Arc::new(vec![Var::Int(1), Var::Int(2)]))
  );
  let report = check("[0] | Var(xs)\n\"s\" | Push(xs)");
  assert_eq!(report.diagnostics[0].code, "variable-type-mismatch");
  assert_eq!(at(&report.diagnostics[0]), (2, 7));
  // Push neither declares nor assigns an immutable binding.
  let report = check("1 | Push(xs)");
  assert_eq!(report.diagnostics[0].code, "unknown-variable");
  assert!(report.diagnostics[0].message.contains("`value | Var(xs)`"));
  let report = check("[] = xs\n1 | Push(xs)");
  assert_eq!(report.diagnostics[0].code, "immutable-binding");
}

#[test]
fn computed_values_in_a_function_called_twice() {
  // Both call sites share one body; the computed operand lives in the
  // invocation's own frame.
  let report = run(
    "@fn(Bump input: Int output: Int params: {} { Add(1 | Add(1)) })\n0 | Bump | Bump",
    &no_defines(),
  );
  assert_eq!(completed(&report, "root"), Var::Int(4));
}

#[test]
fn numbers_mix_in_arithmetic_and_comparisons() {
  let report = run(
    "5 | Math.Divide(2.0) = a
5.0 | Div(2) = b
5 | Div(2) = c
1.5 | IsMore(1) = d
1 | Is(1.0) = e
[1 2 3] | ToFloat3 | Mul(2) | Math.Subtract(1.0) | Math.Length = f
[a b c d e f]",
    &no_defines(),
  );
  let seq = |v: Vec<Var>| Var::from_seq(std::sync::Arc::new(v));
  let len = ((1.0f64).powi(2) + 3.0f64.powi(2) + 5.0f64.powi(2)).sqrt();
  assert_eq!(
    completed(&report, "root"),
    seq(vec![
      Var::Float(2.5),
      Var::Float(2.5),
      Var::Int(2),
      Var::Bool(true),
      Var::Bool(true),
      Var::Float(len),
    ])
  );
  // Vectors of different sizes do not mix.
  let report = check("[1 2 3] | ToFloat3 = v\n[1 2] | ToFloat2 | Math.Add(v)");
  assert_eq!(report.diagnostics[0].code, "input-type-mismatch");
  // Int division by zero is an activation error.
  let report = run("1 | Div(0)", &no_defines());
  assert!(matches!(&report.outcomes[0].1, Some(Outcome::Failed(_))));
}

#[test]
fn value_shards_through_source() {
  let (report, lines) = otter_core::log::capture(|| {
    run(
      "[\"a\" \"b\"] | Count | Log(\"count\")
\"ff\" | ParseInt(base: 16) | ToHex = hex
\"gone\" | IsAny([\"gone\" \"unknown\"]) | Not = live
none | IsNone = nothing
42 | ToString | Log
Time.Now = t0
Time.Now | Math.Subtract(t0) | IsMoreEqual(0.0) = later
[hex live nothing later]",
      &no_defines(),
    )
  });
  assert_eq!(lines, ["count: 2", "42"]);
  assert_eq!(
    completed(&report, "root"),
    Var::from_seq(std::sync::Arc::new(vec![
      Var::string("0xff"),
      Var::Bool(false),
      Var::Bool(true),
      Var::Bool(true),
    ]))
  );
  // Stop ends the instance; nothing after it runs.
  let (report, lines) = otter_core::log::capture(|| run("1 | Log\nStop\n2 | Log", &no_defines()));
  assert_eq!(lines, ["1"]);
  assert!(matches!(&report.outcomes[0].1, Some(Outcome::Stopped)));
}

#[test]
fn control_flow_shards() {
  let report = run(
    "5 = n
n | If({IsMore(3)} {\"big\"} {\"small\"}) = a
n | If({IsMore(10)} {\"big\"}) = b
\"gone\" | Match([\"biting\" {\"hook\"} \"gone\" {\"despawned\"}] default: {\"other\"}) = c
\"x\" | Match([\"biting\" {\"hook\"}] default: {\"other\"}) = d
3 | Match([1 {\"one\"}] default: {}) = e
Maybe({[1 2] | Take(5)} {\"fallback\"} silent: true) = f
Maybe({\"fine\"} {\"fallback\"}) = g
true = far
false = close
All(far {n | IsMore(1)}) = h
All(far close) = i
Any(close {n | Is(5)}) = j
0 | Var(count)
Repeat({Inc(count)} until: {count | IsMoreEqual(3)})
Repeat({Inc(count)} until: {count | IsMoreEqual(100)} times: 2)
[a b c d e f g h i j count]",
    &no_defines(),
  );
  let s = Var::string;
  assert_eq!(
    completed(&report, "root"),
    Var::from_seq(std::sync::Arc::new(vec![
      s("big"),
      Var::Int(5),
      s("despawned"),
      s("other"),
      Var::Int(3),
      s("fallback"),
      s("fine"),
      Var::Bool(true),
      Var::Bool(false),
      Var::Bool(true),
      Var::Int(5),
    ]))
  );
}

/// Golden path test F: Match is exhaustive or has `default:`.
#[test]
fn match_is_exhaustive_or_has_a_default() {
  // Bool and None cases can cover their type.
  let report = run(
    "true | Match([true {1} false {2}]) = a\nnone | Match([none {3}]) = b\n[a b]",
    &no_defines(),
  );
  assert_eq!(
    completed(&report, "root"),
    Var::from_seq(std::sync::Arc::new(vec![Var::Int(1), Var::Int(3)]))
  );
  let report = check("2 | Match([1 {10}])");
  let d = &report.diagnostics[0];
  assert_eq!((d.code, at(d)), ("non-exhaustive-match", (1, 5)));
  assert!(d.message.contains("`default: {...}`"), "{}", d.message);
  let report = check("true | Match([true {1}])");
  assert_eq!(report.diagnostics[0].code, "non-exhaustive-match");
  assert!(
    report.diagnostics[0]
      .message
      .contains("does not cover false"),
    "{}",
    report.diagnostics[0].message
  );
  // `default: {}` passes the input through.
  let (_, lines) =
    otter_core::log::capture(|| run("2 | Match([1 {10}] default: {}) | Log", &no_defines()));
  assert_eq!(lines, ["2"]);
  // `none` matches only none, so on an Int it can never match.
  let report = check("5 | Match([none {1}] default: {2})");
  assert_eq!(report.diagnostics[0].code, "unmatchable-case");
}

#[test]
fn control_flow_resumes_after_suspending_inside() {
  // Each control shard suspends (Pause) inside a nested flow and must
  // resume there, not restart.
  let (report, lines) = otter_core::log::capture(|| {
    run(
      "0 | Var(ticks)
If({true} {Pause Inc(ticks)})
All({Pause true} {Inc(ticks) true})
1 | Match([1 {Pause Inc(ticks)}] default: {})
Maybe({Pause [1] | Take(3)} {Inc(ticks)})
Repeat({Pause Inc(ticks)} until: {ticks | IsMoreEqual(7)})
ticks",
      &no_defines(),
    )
  });
  assert_eq!(completed(&report, "root"), Var::Int(7));
  // Maybe logs the error it caught (not Silent).
  assert_eq!(lines.len(), 1, "{lines:?}");
  assert!(
    lines[0].starts_with("Maybe: activation error: Take: index 3"),
    "{lines:?}"
  );
}

#[test]
fn control_flow_compose_errors() {
  let report = check("Repeat({})");
  assert_eq!(report.diagnostics[0].code, "missing-argument");
  let report = check("1 | Match([\"a\" {}] default: {})");
  assert_eq!(report.diagnostics[0].code, "unmatchable-case");
  let report = check("All({1})");
  assert_eq!(report.diagnostics[0].code, "predicate-not-bool");
  // A failing shard inside the second condition flow: located there.
  let report = check("true = far\nAll(far {1 | Add(\"x\")})");
  let d = &report.diagnostics[0];
  assert_eq!(d.path_string(), "root/2:All/conditions/#1/1:Math.Add");
  assert_eq!(at(d), (2, 14));
}

// --- review findings (Astra and Opus, 2026-10-05) ---

#[test]
fn a_failed_once_runs_again() {
  // The first attempt fails inside Once before assigning x; Maybe's Else
  // fixes the index. Once must run again (not count as done), so x is
  // assigned before it is read: never the initial `x: 0`.
  let (report, lines) = otter_core::log::capture(|| {
    run(
      "1 | Var(k)
0 | Var(x)
Repeat({
  Maybe({
    Once({[10] | Take(k) | Update(x)})
    x | Log(\"x\")
  } {Math.Dec(k)} silent: true)
} times: 2)",
      &no_defines(),
    )
  });
  assert!(report.succeeded());
  assert_eq!(lines, ["x: 10"]);
}

#[test]
fn temporaries_are_scoped_per_body() {
  // One body per input type: Int and Float callers compose the f-string
  // temporary separately.
  let report = run(
    "@fn(W input: Int | Float output: String params: {} {f\"{Add(1)}\"})\n1 | W\n1.0 | W",
    &no_defines(),
  );
  assert_eq!(completed(&report, "root"), Var::string("2"));
}

#[test]
fn the_suggested_all_rewrite_compiles_and_flow_parameters_take_shorthands() {
  // The exact rewrite the And diagnostic suggests.
  let d = load_errors("true = a\nfalse = b\nIf({a | And | b} {1} {2})");
  assert!(d[0].message.contains("`If(All(a b) ...)`"));
  let report = run(
    "true = a
false = b
If(All(a b) {1} {2}) = x
When(a {3 | Var(y)})
If(Any(b {a}) {4} {5}) = z
[x z]",
    &no_defines(),
  );
  assert_eq!(
    completed(&report, "root"),
    Var::from_seq(std::sync::Arc::new(vec![Var::Int(2), Var::Int(4)]))
  );
}

#[test]
fn spawned_failures_fail_the_run() {
  let report = run("@wire(child {1 | Div(0)})\nSpawn(child)", &no_defines());
  assert!(!report.succeeded());
  assert_eq!(report.spawned_failures.len(), 1);
  assert_eq!(report.spawned_failures[0].0, "child");
}

#[test]
fn mixed_number_equality_and_order_are_exact_and_consistent() {
  let report = run(
    "9007199254740993 | Is(9007199254740992.0) = a
9007199254740993 | IsMore(9007199254740992.0) = b
[1 2] | Is([1.0 2.0]) = c
1 | IsAny([1.0 \"a\"]) = d
9.21e18 | ToInt = e
[a b c d e]",
    &no_defines(),
  );
  assert_eq!(
    completed(&report, "root"),
    Var::from_seq(std::sync::Arc::new(vec![
      Var::Bool(false),
      Var::Bool(true),
      Var::Bool(true),
      Var::Bool(true),
      Var::Int(9_210_000_000_000_000_000),
    ]))
  );
  let report = run("9.3e18 | ToInt", &no_defines());
  assert!(!report.succeeded());
}

#[test]
fn per_iteration_and_kept_sequences() {
  // `Var` declares afresh on every iteration; `Keep` persists.
  let (_, lines) = otter_core::log::capture(|| {
    run(
        "@wire(w {Keep(kept [])  [] | Var(s)  1 | Push(s)  s | Count | Log(\"cleared\")  1 | Push(kept)  kept | Count | Log(\"kept\")} looped: true)
@mesh(m)
@schedule(m w)
@run(m iterations: 3)",
        &no_defines(),
      )
  });
  assert_eq!(
    lines,
    [
      "cleared: 1",
      "kept: 1",
      "cleared: 1",
      "kept: 2",
      "cleared: 1",
      "kept: 3"
    ]
  );
}

#[test]
fn check_composes_every_wire_that_can_run() {
  let report = check("@wire(unused { 1 | Add(\"x\") })\n1");
  assert_eq!(report.diagnostics.len(), 1, "{}", report.to_json());
  assert_eq!(at(&report.diagnostics[0]), (1, 20));
}

#[test]
fn computed_elements_keep_source_order() {
  let report = run("0 | Var(n)\n[n (Inc(n) n)]", &no_defines());
  assert_eq!(
    completed(&report, "root"),
    Var::from_seq(std::sync::Arc::new(vec![Var::Int(0), Var::Int(1)]))
  );
}

#[test]
fn until_runs_before_action_and_may_assign_what_it_reads() {
  let report = run(
    "0 | Var(n)\n\"\" | Var(x)\nRepeat({x | Log} until: {n | ToString | Update(x)  Inc(n) | IsMore(2)})\nn",
    &no_defines(),
  );
  assert_eq!(completed(&report, "root"), Var::Int(3));
  // Until and action are separate blocks: a name declared in one is
  // not visible in the other.
  let report =
    check("0 | Var(n)\nRepeat({x | Log} until: {n | ToString | Var(x)  Inc(n) | IsMore(2)})");
  assert_eq!(report.diagnostics[0].code, "unknown-variable");
}

#[test]
fn uppercase_path_keys_stay_one_segment() {
  let report = run("{a: {Key: {sub: 1}}} = t\nt.a.Key.sub", &no_defines());
  assert_eq!(completed(&report, "root"), Var::Int(1));
}

#[test]
fn unknown_dashed_names_explain_the_dash() {
  let report = check("5 = x\nx-1");
  assert!(
    report.diagnostics[0]
      .message
      .contains("`-` is part of names"),
    "{}",
    report.diagnostics[0].message
  );
}

// --- second review round (2026-10-05) ---

/// Golden path test E: binding errors name the variable and help.
#[test]
fn bindings_report_unknown_duplicate_and_immutable_names() {
  let report = check("0 | Var(counter)\n1 | Update(coutner)");
  let d = &report.diagnostics[0];
  assert_eq!((d.code, at(d)), ("unknown-variable", (2, 5)));
  assert_eq!(d.did_you_mean, ["counter"]);

  let report = check("0 | Var(counter)\n1 | Var(counter)");
  let d = &report.diagnostics[0];
  assert_eq!((d.code, at(d)), ("duplicate-binding", (2, 5)));
  assert!(d.message.contains("`Update(counter)`"), "{}", d.message);
  let related = d.related.as_ref().unwrap();
  assert_eq!((related.line, related.column), (Some(1), Some(5)));
  assert!(
    report
      .to_json()
      .contains(r#""related":{"message":"counter is declared here","line":1,"column":5}"#),
    "{}",
    report.to_json()
  );

  let report = check("0 = counter\n1 | Update(counter)");
  assert_eq!(report.diagnostics[0].code, "immutable-binding");
  // `=` cannot rebind a visible name either, nor shadow a mesh variable.
  let report = check("0 = x\n1 = x");
  assert_eq!(report.diagnostics[0].code, "duplicate-binding");
  // Disjoint sibling blocks may reuse a name.
  let report = check("If(true {1 | Var(t) t} {2 | Var(t) t}) | Log");
  assert!(report.ok(), "{}", report.to_json());
  // A block may update an enclosing mutable variable.
  let report = run("0 | Var(n)\nWhen(true {5 | Update(n)})\nn", &no_defines());
  assert_eq!(completed(&report, "root"), Var::Int(5));
}

#[test]
fn input_is_a_reserved_name() {
  for text in [
    "1 | Var(input)",
    "1 = input",
    "@wire(w {Keep(input 0)} looped: true) @mesh(m) @schedule(m w) @run(m)",
  ] {
    let report = check(text);
    assert_eq!(report.diagnostics[0].code, "reserved-name", "{text}");
    assert!(report.diagnostics[0].message.contains("entry value"));
  }
}

#[test]
fn keep_holds_state_across_iterations() {
  let (report, lines) = otter_core::log::capture(|| {
    run(
      "@wire(w {Keep(n 10)  n | Math.Add(1) | Update(n)  n | Log} looped: true)
@mesh(m) @schedule(m w) @run(m iterations: 3)",
      &no_defines(),
    )
  });
  assert!(report.succeeded());
  assert_eq!(lines, ["11", "12", "13"]);
  // Keep belongs to a wire's top level, outside branches and loops.
  let report =
    check("@wire(w {When(true {Keep(n 0)})} looped: true) @mesh(m) @schedule(m w) @run(m)");
  let d = &report.diagnostics[0];
  assert_eq!((d.code, at(d)), ("keep-not-top-level", (1, 21)));
  // Its initial value is a literal, and its type follows it.
  let report =
    check("@wire(w {Keep(n 0) 1.5 | Update(n)} looped: true) @mesh(m) @schedule(m w) @run(m)");
  assert_eq!(report.diagnostics[0].code, "variable-type-mismatch");
}

/// `n` functions, each calling the next, called from the root.
fn do_chain(n: usize) -> String {
  let mut src = String::new();
  for i in 0..n {
    src.push_str(&format!(
      "@fn(W{i} input: None output: Int params: {{}} {{W{}}})\n",
      i + 1
    ));
  }
  src.push_str(&format!(
    "@fn(W{n} input: None output: Int params: {{}} {{1}})\nW0"
  ));
  src
}

#[test]
fn deep_do_chains_are_a_diagnostic_not_a_crash() {
  // Within the limit it runs.
  // ESP-IDF's runtime invocation limit is 32; desktop defaults to 256.
  let depth = if cfg!(target_os = "espidf") { 20 } else { 40 };
  assert_eq!(
    completed(&run(&do_chain(depth), &no_defines()), "root"),
    Var::Int(1)
  );
  // The device exercises the same over-limit diagnostic without parsing
  // thousands of unrelated declarations into its constrained heap: a chain
  // of 40 function declarations is about 800 tokens (the token vector's
  // next doubling would ask the fragmented heap for 80 KiB at once).
  let excessive: &[usize] = if cfg!(target_os = "espidf") {
    &[40]
  } else {
    &[80, 400, 3000]
  };
  for &n in excessive {
    let report = check(&do_chain(n));
    assert_eq!(report.diagnostics[0].code, "too-deep", "{n}");
  }
  // Spawn chains compose recursively too.
  let mut src = String::new();
  let spawn_depth = if cfg!(target_os = "espidf") { 80 } else { 200 };
  assert!(spawn_depth > otter_core::compose::MAX_FLOW_DEPTH);
  for i in 0..spawn_depth {
    src.push_str(&format!("@wire(s{i} {{Spawn(s{})}})\n", i + 1));
  }
  src.push_str(&format!("@wire(s{spawn_depth} {{1}})\nSpawn(s0)"));
  assert_eq!(check(&src).diagnostics[0].code, "too-deep");
}

/// A chain of `n` functions, each calling the next inside the given
/// control-flow wrapper (`{next}` is replaced by the call).
fn wrapped_chain(n: usize, wrapper: &str) -> String {
  let mut src = String::new();
  for i in 0..n {
    let body = wrapper.replace("{next}", &format!("W{}", i + 1));
    src.push_str(&format!(
      "@fn(W{i} input: None output: Any params: {{}} {{{body}}})\n"
    ));
  }
  src.push_str(&format!(
    "@fn(W{n} input: None output: Any params: {{}} {{1}})\nW0"
  ));
  src
}

#[test]
fn nesting_up_to_the_limit_runs_through_every_control_shard() {
  // Each wrapper adds two or three flow levels per wire; the chain goes
  // as deep as compose allows, and must run (in debug builds too).
  for (wrapper, levels) in [
    ("If(All({true} {{next} true}) {1} {2})", 3),
    ("When({true} {{next}})", 2),
    ("Repeat({{next}} times: 1)", 2),
    ("1 | Match([1 {{next}}] default: {{}})", 2),
    ("Maybe({{next}} {2})", 2),
    ("Any({false} {{next} false})", 2),
    // A compose-time evaluation composes and runs its pipeline in the
    // middle of compose, an engine activation per level.
    ("#( {next} )", 3),
  ] {
    let n = (otter_core::compose::MAX_FLOW_DEPTH - 2) / levels;
    let src = wrapped_chain(n, wrapper);
    // This test needs diagnostics, not the full tooling report of every
    // occurrence and its expanded source path.
    let program = Program::load(Source::new("t.shs", &src), &catalog(), &no_defines())
      .unwrap_or_else(|(_, d)| panic!("{d:?}"));
    let diagnostics = program.compose();
    assert!(diagnostics.is_empty(), "{wrapper}: {diagnostics:?}");
    drop(program);
    #[cfg(not(target_os = "espidf"))]
    assert!(check(&src).ok(), "{wrapper}");
    assert!(run(&src, &no_defines()).succeeded(), "{wrapper}");
    // The chain is at the limit: one more wire is too deep. If compose's
    // level counting drifts, this fails instead of the test silently
    // running shallower than the limit (and the release CI step with it).
    let deeper = check(&wrapped_chain(n + 1, wrapper));
    assert_eq!(
      deeper.diagnostics.first().map(|d| d.code),
      Some("too-deep"),
      "{wrapper}: n = {n} should be the deepest chain that composes"
    );
  }
}

#[test]
fn maybe_without_else_passes_its_input_through() {
  // As in 1.x: Action runs for its effects, on success or failure.
  let report = run(
    "5 | Maybe({Add(1)}) = a\n5 | Maybe({[1] | Take(3)} silent: true) = b\n[a b]",
    &no_defines(),
  );
  assert_eq!(
    completed(&report, "root"),
    Var::from_seq(std::sync::Arc::new(vec![Var::Int(5), Var::Int(5)]))
  );
}

/// Cond lowers its branches in a loop: thousands of them compose on a small
/// stack (the recursion it replaced took about 140 bytes per branch).
#[cfg(not(any(target_arch = "wasm32", target_os = "espidf")))]
#[test]
fn a_cond_with_thousands_of_branches_composes_on_a_small_stack() {
  let branches = "{false} {2} ".repeat(4_000);
  let text = format!("1 | Cond({branches}{{true}} {{3}} passthrough: false)");
  let value = std::thread::Builder::new()
    .stack_size(128 * 1024)
    .spawn(move || root_value(&text).to_string())
    .unwrap()
    .join()
    .unwrap();
  assert_eq!(value, "3");
}

#[test]
fn get_with_a_default_and_match_passing_its_input_through() {
  let v = root_value(
    r#"
"{\"a\": 5}" | FromJson | ExpectTable = j
{a: 1 b: "x"} = t
none | ToAny | Var(any)
"" | Var(seen)
[
  Get(j key: "a" default: 0)
  Get(j key: "missing" default: 0)
  Get(j key: "a" default: "wrong type")
  Get(t key: "b" default: "")
  Get(any default: "fallback")
  (2 | Match([1 {"one" | Update(seen)} 2 {"two" | Update(seen)}] default: {} passthrough: true))
  seen
]
"#,
  );
  assert_eq!(
    v.to_string(),
    r#"[5 0 "wrong type" "x" "fallback" 2 "two"]"#
  );
  for (script, expected) in [
    (r#"{a: 1} = t  Get(t key: "zz" default: 0)"#, "has no key"),
    (r#"{a: 1} = t  Get(t key: "a")"#, "needs `default:`"),
    (r#"1 = n  Get(n key: "a" default: 0)"#, "reads a table"),
  ] {
    let d = load_or_compose_errors(script);
    assert!(
      d[0].message.contains(expected),
      "{script}: {}",
      d[0].message
    );
  }
}

#[test]
fn maybe_gives_else_the_error_when_asked() {
  let v = root_value(
    r#"
5 | Maybe({"no luck" | Fail} {= msg  f"caught: {msg}"} error: true silent: true) = caught
5 | Maybe({Add(1) | ToString} {"never"} error: true) = fine
5 | Maybe({"x" | Fail} {Add(1) | ToString} silent: true) = passed
[caught fine passed]
"#,
  );
  assert_eq!(v.to_string(), r#"["caught: no luck" "6" "6"]"#);
  // With `error`, else's input is the message, a String.
  let d = load_or_compose_errors("5 | Maybe({Add(1)} {Add(1)} error: true)");
  assert_eq!(d[0].code, "input-type-mismatch", "{:?}", d[0]);
}

#[test]
fn entry_outcomes_survive_retirement_of_finished_records() {
  // Many one-shot entries finish on the first tick; a looped entry keeps
  // the mesh running. Finished records are drained from the mesh every
  // tick; the report still has every entry's outcome.
  // Device-sized on ESP-IDF: 64 compiled wires and their instances exceed
  // the classic ESP32's heap late in the suite.
  let entries = if cfg!(target_os = "espidf") { 16 } else { 64 };
  let mut src = String::new();
  for i in 0..entries {
    src.push_str(&format!("@wire(e{i} {{{i}}})\n"));
  }
  src.push_str("@wire(ticker {Pause} looped: true)\n@mesh(m)\n");
  for i in 0..entries {
    src.push_str(&format!("@schedule(m e{i})\n"));
  }
  src.push_str("@schedule(m ticker)\n@run(m iterations: 3)");
  let report = run(&src, &no_defines());
  assert_eq!(report.ticks, 3);
  for i in 0..entries {
    assert_eq!(completed(&report, &format!("e{i}")), Var::Int(i));
  }
  assert!(matches!(
    report.outcomes.iter().find(|(w, _)| w == "ticker"),
    Some((_, None))
  ));
}

#[test]
fn code_after_stop_keeps_its_types() {
  assert!(check("1 | Stop\n1 | Add(1)").ok());
  // Nested Whens count one bracket and one brace per level, and one
  // compose level each (within the device's limit too).
  let mut src = String::from("1");
  for _ in 0..(if cfg!(target_os = "espidf") { 20 } else { 30 }) {
    src = format!("When({{true}} {{{src}}})");
  }
  let program = Program::load(Source::new("t.shs", &src), &catalog(), &no_defines())
    .unwrap_or_else(|(_, d)| panic!("{d:?}"));
  assert!(program.compose().is_empty());
  drop(program);
  #[cfg(not(target_os = "espidf"))]
  assert!(check(&src).ok());
}

#[test]
fn unreachable_wire_cycles_are_checked() {
  let report = check("@wire(a {Spawn(b)})\n@wire(b {Spawn(a)})\n1");
  assert_eq!(report.diagnostics.len(), 1, "{}", report.to_json());
  assert_eq!(report.diagnostics[0].code, "recursive-wire");
}

#[test]
fn analysis_locates_nested_calls_and_computed_operands() {
  use otter_core::diagnostic::PathStep;
  let report = check(
    "@fn(Helper input: Int output: Int params: {} {Pause 2 | Math.Add(3)})\n1 | Math.Add((Time.Now | ToInt))\nHelper\nHelper\nKeep(n 0)",
  );
  assert!(report.ok(), "{}", report.to_json());
  let root = &report.wires[0];
  assert!(root.analysis.effects.time && root.analysis.effects.suspends);
  assert_eq!(
    root.analysis.lifetime,
    otter_core::signature::Lifetime::Stateful
  );
  let calls: Vec<_> = root
    .occurrences
    .iter()
    .filter(
      |o| matches!(o.occurrence.path.last(), Some(PathStep::Shard {name, ..}) if name == "Helper"),
    )
    .collect();
  assert_eq!(
    calls.iter().map(|o| o.line.unwrap()).collect::<Vec<_>>(),
    [3, 4]
  );
  assert!(calls.iter().all(|o| o.occurrence.effects.suspends));
  let pauses: Vec<_> = root
    .occurrences
    .iter()
    .filter(
      |o| matches!(o.occurrence.path.last(), Some(PathStep::Shard {name, ..}) if name == "Pause"),
    )
    .collect();
  assert_eq!(pauses.len(), 2);
  assert_ne!(pauses[0].occurrence.path, pauses[1].occurrence.path);
  assert_eq!(pauses[0].line, Some(1));
  assert_eq!(pauses[0].column, Some(47));
  let time = root.occurrences.iter().find(|o| matches!(o.occurrence.path.last(), Some(PathStep::Shard {name, ..}) if name == "Time.Now")).unwrap();
  assert_eq!(time.line, Some(2));
  assert_eq!(time.occurrence.output, otter_core::Type::float());
  assert!(root.occurrences.iter().all(|o| o.span.is_some()));
  // Separate source locations never enter a shared compose artifact.
  let shifted = check("\n\n1 | Math.Add(2)");
  assert!(
    shifted.wires[0]
      .occurrences
      .iter()
      .all(|o| o.line == Some(3))
  );
}

#[test]
fn a_valid_program_checks_clean() {
  let report = check("@wire(w { 1 | Add(1) })\n@mesh(main)\n@schedule(main w)\n@run(main fps: 30)");
  assert!(report.ok(), "{}", report.to_json());
  assert!(
    report
      .to_json()
      .starts_with("{\"ok\":true,\"file\":\"t.shs\",\"diagnostics\":[],\"wires\":[")
  );
  assert_eq!(report.wires[0].occurrences.len(), 2);
  assert_eq!(
    report.wires[0].occurrences[1].occurrence.output,
    otter_core::Type::int()
  );
}

// --- load errors (scheduler-independent) ---

#[test]
fn syntax_errors_are_located_and_name_language_constructs() {
  let d = load_errors("\"Hello\" | Log(");
  assert_eq!(
    (d[0].phase.name(), d[0].kind, d[0].code),
    ("parse", "syntax", "unclosed")
  );
  assert_eq!(at(&d[0]), (1, 14));
  assert!(
    d[0]
      .message
      .contains("missing `)` for the parameters of `Log` opened at 1:14"),
    "{}",
    d[0].message
  );

  let d = load_errors("@wire(w {\n  1 | Add(1)\n)");
  assert!(
    d[0]
      .message
      .contains("missing `}` for the flow opened at 1:9 (found `)` at 3:1)"),
    "{}",
    d[0].message
  );

  // Several problems in one pass.
  let d = load_errors("1 | Add(1); 2\nnull | Add(1.)");
  let codes: Vec<&str> = d.iter().map(|d| d.code).collect();
  assert_eq!(codes, ["semicolon", "null", "number-form"]);
  assert!(d[2].message.contains("write 1.0"), "{}", d[2].message);
}

/// Labels are lowercase (golden-path.md D3): uppercase names a shard.
#[test]
fn uppercase_labels_are_rejected_with_the_lowercase_fix() {
  let d = load_errors("Repeat({} Times: 3)");
  assert_eq!((d[0].code, at(&d[0])), ("parameter-name", (1, 11)));
  assert!(
    d[0]
      .message
      .contains("parameter labels are lowercase: `times:`"),
    "{}",
    d[0].message
  );
  let d = load_errors("@wire(w {} Looped: true)");
  assert_eq!(d[0].code, "parameter-name");
}

#[test]
fn unknown_names_suggest_the_closest() {
  let d = load_errors("1 | Ad(1)");
  assert_eq!(
    (d[0].phase.name(), d[0].kind),
    ("construct", "unknown-shard")
  );
  assert_eq!(d[0].did_you_mean, ["Add"]);
  assert_eq!(at(&d[0]), (1, 5));

  let d = load_errors("0 | Var(n)\ninc(n)");
  assert!(
    d[0]
      .message
      .contains("shard names start with an uppercase letter"),
    "{}",
    d[0].message
  );
  assert_eq!(d[0].did_you_mean[0], "Inc");

  let d = load_errors("Repeat({} tims: 3)");
  assert_eq!(d[0].code, "unknown-argument");
  assert_eq!(d[0].did_you_mean, ["times"]);
  assert_eq!(at(&d[0]), (1, 11));

  let d = load_errors("@missing");
  assert_eq!(d[0].code, "unknown-define");
  assert!(d[0].message.contains("pass it as `missing:value`"));
}

#[test]
fn the_top_level_declares_when_there_is_a_run() {
  let d = load_errors("@wire(w { 1 })\n@mesh(main)\n@schedule(main w)\n@run(main)\n1 | Add(1)");
  assert_eq!(d[0].code, "code-outside-wire");
  assert_eq!(at(&d[0]), (5, 1));

  let d = load_errors("@mesh(main)\n@schedule(main nope)\n@run(main) | Add(1)");
  let codes: Vec<&str> = d.iter().map(|d| d.code).collect();
  assert!(
    codes.contains(&"piped-declaration") && codes.contains(&"unknown-wire"),
    "{codes:?}"
  );
}

#[test]
fn and_or_point_to_all_and_any() {
  let d = load_errors("true = a\nfalse = b\nIf({a | And | b} {})");
  assert_eq!(d[0].code, "and-or");
  assert_eq!(d[0].did_you_mean, ["All"]);
  assert!(
    d[0].message.contains("`If(All(a b) ...)`"),
    "{}",
    d[0].message
  );
  let d = load_errors("Or");
  assert_eq!(d[0].did_you_mean, ["Any"]);
  let d = load_errors("1 | Match([\"a\"])");
  assert_eq!(d[0].code, "cases");
}

#[test]
fn malformed_input_is_reported_not_crashed_on() {
  // Keep the parser's actual nesting boundary on devices without allocating
  // thousands of tokens that the parser will never visit after rejecting it.
  let arrays = if cfg!(target_os = "espidf") {
    otter_lang::parser::MAX_DEPTH + 1
  } else {
    5000
  };
  assert!(arrays > otter_lang::parser::MAX_DEPTH);
  let deep = format!("{}1{}", "[".repeat(arrays), "]".repeat(arrays));
  let d = load_errors(&deep);
  assert_eq!(d[0].code, "too-deep");
  let calls = if cfg!(target_os = "espidf") {
    otter_lang::parser::MAX_DEPTH + 1
  } else {
    500
  };
  assert!(calls > otter_lang::parser::MAX_DEPTH);
  let deep = format!(
    "{}{}",
    "Repeat({".repeat(calls),
    "} times: 1)".repeat(calls)
  );
  assert_eq!(load_errors(&deep)[0].code, "too-deep");
  assert_eq!(load_errors("{x: 1 y:}")[0].code, "missing-value");
  let d = load_errors("Repeat(times: action: {})");
  assert_eq!((d[0].code, at(&d[0])), ("missing-value", (1, 8)));
  assert_eq!(load_errors("0x1g")[0].code, "number-form");
  assert_eq!(
    load_errors("@run(m fps: 1e-300)\n@mesh(m)")[0]
      .param
      .as_deref(),
    Some("fps")
  );
  assert!(
    load_errors("@mesh(m)\n@run(m iterations: 0)")[0]
      .message
      .contains("positive whole number")
  );
  assert_eq!(
    load_errors("@wire(w {})\n@mesh(m)\n@schedule(m w)\n1")[0].code,
    "schedule-without-run"
  );
}

#[test]
fn every_1x_string_escape_is_accepted() {
  let program = Program::load(
    Source::new("t.shs", "\"\\'\\b\\f\\v\""),
    &catalog(),
    &no_defines(),
  );
  assert!(program.is_ok());
}

/// A declaration or assignment target is a name: computed values and paths
/// are rejected, not hoisted into a hidden temporary (M2 review F1).
#[test]
fn variable_targets_must_be_names() {
  for (text, line, column) in [
    ("1 | Var((2))", 1, 9),
    ("Keep((0) 10)", 1, 6),
    ("{count: 1} = obj\n7 | Var(obj.count)", 2, 9),
    ("{count: 1} = obj\n7 | Update(obj.count)", 2, 12),
    ("1 | Var(2)", 1, 9),
    ("0 | Var(n)\nInc(n | Add(1))", 2, 5),
  ] {
    let d = load_errors(text);
    assert_eq!(d[0].code, "expected-variable-name", "{text}");
    assert_eq!(
      (d[0].line, d[0].column),
      (Some(line), Some(column)),
      "{text}"
    );
    assert!(
      d[0].message.contains("takes a variable name"),
      "{}",
      d[0].message
    );
  }
}

/// The old assignment forms fail with a plain error naming the word form.
#[test]
fn removed_assignment_forms_say_what_to_write() {
  for (text, code, needle, column) in [
    ("1 >= n", "removed-operator", "`value | Var(name)`", 3),
    (
      "0 | Var(n)\n1 > n",
      "removed-operator",
      "`value | Update(name)`",
      3,
    ),
    (
      "[] | Var(xs)\n1 >> xs",
      "removed-operator",
      "`value | Push(name)`",
      3,
    ),
    ("1 | Set(n)", "removed-shard", "`value | Var(name)`", 5),
    ("1 | Ref(n)", "removed-shard", "`value = name`", 5),
  ] {
    let d = load_errors(text);
    assert_eq!(d[0].code, code, "{text}");
    assert!(d[0].message.contains(needle), "{text}: {}", d[0].message);
    assert_eq!(d[0].column, Some(column), "{text}");
  }
}

#[test]
fn constructs_not_supported_yet_are_rejected_explicitly() {
  for (text, needle) in [
    ("{1: 2}", "table keys are strings"),
    ("[{1: x}]", "table keys are strings"),
    ("Type::Value", "enums"),
    ("Pause(1.0 | Add(1.0))", "Pause.seconds takes a literal"),
  ] {
    let d = load_errors(text);
    assert!(d[0].message.contains(needle), "{text}: {}", d[0].message);
  }
}

// --- functions (golden path §3, tests A to I at the source level) ---

fn lines_of(text: &str) -> Vec<String> {
  let (report, lines) = otter_core::log::capture(|| run(text, &no_defines()));
  assert!(report.succeeded(), "{report:?}");
  lines
}

#[test]
fn functions_are_called_like_shards_and_share_one_body() {
  let source = "@fn(Scale input: Float output: Float params: {factor: Float} { Math.Multiply(factor) })\n3.0 | Scale(factor: 2.0) | Log\n3.0 | Scale(factor: 4.0) | Log";
  assert_eq!(lines_of(source), ["6", "12"]);
  let report = check(source);
  assert!(report.ok(), "{}", report.to_json());
  assert_eq!(report.functions.len(), 1);
  let signature = &report.functions[0].signature;
  assert_eq!(signature.name, "Scale");
  assert_eq!(signature.params.as_ref().map(Vec::len), Some(1));
  assert_eq!(
    (report.functions[0].line, report.functions[0].column),
    (Some(1), Some(1))
  );
  let json = report.to_json();
  assert!(
    json.contains("\"functions\":[{\"signature\":{\"name\":\"Scale\""),
    "{json}"
  );
  assert!(
    json.contains("\"params\":[{\"name\":\"factor\",\"type\":\"Float\""),
    "{json}"
  );
  // Occurrences inside the body are reported at each call site, located in
  // the function's source.
  let inner: Vec<_> = report.wires[0]
    .occurrences
    .iter()
    .filter(|o| matches!(o.occurrence.path.last(), Some(otter_core::diagnostic::PathStep::Shard { name, .. }) if name == "Math.Multiply"))
    .collect();
  assert_eq!(inner.len(), 2);
  assert!(inner.iter().all(|o| o.line == Some(1)));
  assert_ne!(inner[0].occurrence.path, inner[1].occurrence.path);
}

#[test]
fn a_function_cannot_capture_the_callers_locals() {
  let report = check(
    "@fn(Bad input: Int output: Int params: {} {\n  Math.Add(secret)\n})\n10 = secret\n1 | Bad",
  );
  let d = &report.diagnostics[0];
  assert_eq!(
    (d.code, d.shard.as_deref()),
    ("unknown-variable", Some("Math.Add"))
  );
  assert_eq!(at(d), (2, 3));
  assert!(
    d.message.contains("pass it as a parameter"),
    "{}",
    d.message
  );
  assert!(d.message.contains("Bad(secret: secret)"), "{}", d.message);
}

#[test]
fn stateless_functions_start_fresh_and_components_keep_state_per_site() {
  let fresh = "@fn(Counting input: None output: Int params: {} {\n  0 | Var(n)\n  Pause\n  n | Math.Add(1) | Update(n)\n})\nRepeat({Counting | Log} times: 3)";
  assert_eq!(lines_of(fresh), ["1", "1", "1"]);
  let component = |stateful| {
    format!(
      "@fn(Counter {stateful}input: None output: Int params: {{step: Int}} {{\n  Keep(n 0)\n  n | Math.Add(step) | Update(n)\n}})\n@wire(main {{ Counter(step: 1) | Log Counter(step: 10) | Log }} looped: true)\n@mesh(m) @schedule(m main) @run(m iterations: 2)"
    )
  };
  assert_eq!(
    lines_of(&component("stateful: true ")),
    ["1", "10", "2", "20"]
  );
  let report = check(&component(""));
  let d = &report.diagnostics[0];
  assert_eq!(
    (d.code, d.shard.as_deref()),
    ("keep-in-stateless", Some("Keep"))
  );
  assert_eq!(at(d), (2, 3));
  assert!(d.message.contains("stateful: true"), "{}", d.message);
}

#[test]
fn match_default_input_means_the_functions_entry_value() {
  let source = |default| {
    format!(
      "@fn(F input: Int output: Int params: {{}} {{ 2 | Match([1 {{10}}] default: {{{default}}}) }})\n99 | F | Log"
    )
  };
  assert_eq!(lines_of(&source("")), ["2"]);
  assert_eq!(lines_of(&source("input")), ["99"]);
}

#[test]
fn parameters_are_snapshots_read_from_declared_mesh_variables() {
  let source = r#"@fn(Later input: Int output: Int params: {amount: Int} { Pause | Math.Add(amount) })
@fn(Apply input: Int output: Int params: {} uses: [gain] { Later(amount: gain) })
@wire(main { 3 | Apply | Log })
@wire(bump { 100 | Update(gain) })
@mesh(m) @schedule(m main) @schedule(m bump) @run(m)"#;
  let mut mesh = Mesh::new();
  mesh.declare_var("gain", Var::Int(2), true);
  let mut session = otter_lang::Session::with_mesh(mesh);
  preserve(&mut session, source);
  let (_, lines) = otter_core::log::capture(|| {
    session.tick(); // main suspends inside Later with amount = 2; bump sets 100.
    session.tick();
  });
  assert_eq!(lines, ["5"]);
  // Reading the mesh without declaring it is rejected, directly and through
  // a callee.
  let mut mesh = Mesh::new();
  mesh.declare_var("gain", Var::Int(2), true);
  let mut session = otter_lang::Session::with_mesh(mesh);
  let (_, d) = session
    .reload_preserving(
      Source::new(
        "t.shs",
        "@fn(Read input: Int output: Int params: {} { Math.Add(gain) })\n1 | Read | Log",
      ),
      &catalog(),
      &no_defines(),
    )
    .unwrap_err();
  assert_eq!(d[0].code, "undeclared-mesh-access");
  assert_eq!(at(&d[0]), (1, 46));
  let (_, d) = session
    .reload_preserving(
      Source::new("t.shs", "@fn(Read input: Int output: Int params: {} uses: [gain] { Math.Add(gain) })\n@fn(Outer input: Int output: Int params: {} { Read })\n1 | Outer | Log"),
      &catalog(),
      &no_defines(),
    )
    .unwrap_err();
  assert_eq!(
    (d[0].code, d[0].shard.as_deref()),
    ("undeclared-mesh-access", Some("Read"))
  );
  assert_eq!(at(&d[0]), (2, 47));
}

#[test]
fn arguments_run_once_in_order_before_the_callee_is_entered() {
  use otter_core::shards::{ProbeEventKind, take_probe_events};
  let source = "@fn(Both input: Int output: Int params: {a: Int b: Int} { Probe(\"callee\") a | Math.Add(b) })\n7 | Both(a: (Log(\"first\")) b: (Log(\"second\"))) | Log";
  assert_eq!(lines_of(source), ["first: 7", "second: 7", "14"]);
  // Suspending inside the second argument does not rerun the first.
  let source = "@fn(Both input: Int output: Int params: {a: Int b: Int} { a | Math.Add(b) })\n7 | Both(a: (Log(\"first\")) b: (Pause | Log(\"second\"))) | Log";
  assert_eq!(lines_of(source), ["first: 7", "second: 7", "14"]);
  // Failing the second never instantiates the callee.
  take_probe_events();
  let source = "@fn(Both input: Int output: Int params: {a: Int b: Int} { Probe(\"callee\") a | Math.Add(b) })\nMaybe({7 | Both(a: (Log(\"first\")) b: ([1] | Take(3))) | Log} silent: true)";
  assert_eq!(lines_of(source), ["first: 7"]);
  assert!(
    take_probe_events()
      .iter()
      .all(|e| e.tag != "callee" || e.kind != ProbeEventKind::Instantiate)
  );
}

#[test]
fn purity_and_return_in_source() {
  let report =
    check("@fn(Now input: None output: Float params: {} pure: true { Time.Now })\nNow | Log");
  let d = &report.diagnostics[0];
  assert_eq!((d.code, d.shard.as_deref()), ("not-pure", Some("Now")));
  assert!(
    d.message.contains("Time.Now has the effect `time`"),
    "{}",
    d.message
  );
  assert_eq!(at(d), (1, 1));
  let source = "@fn(Early input: Int output: Int params: {} {\n  When({IsMore(10)} {input | Return})\n  Math.Add(100)\n})\n1 | Early | Log\n20 | Early | Log";
  assert_eq!(lines_of(source), ["101", "20"]);
  let report = check("@fn(Bad input: Int output: Int params: {} {\n  \"x\" | Return\n})\n1 | Bad");
  let d = &report.diagnostics[0];
  assert_eq!(
    (d.code, d.shard.as_deref()),
    ("return-type-mismatch", Some("Return"))
  );
  assert_eq!(at(d), (2, 9));
}

#[test]
fn uncalled_functions_are_still_checked() {
  let report = check("@fn(Bad input: Int output: Int params: {} { \"x\" })\n1 | Log");
  let d = &report.diagnostics[0];
  assert_eq!(
    (d.code, d.shard.as_deref()),
    ("output-type-mismatch", Some("Bad"))
  );
  assert_eq!(at(d), (1, 1));
  // A call before the declaration, and a recursive pair.
  assert_eq!(
    lines_of("1 | Twice | Log\n@fn(Twice input: Int output: Int params: {} { Math.Multiply(2) })"),
    ["2"]
  );
  // Mutual recursion composes against the declared signatures and runs.
  let source = "@fn(Even input: Int output: Bool params: {} { If({Is(0)} {true} {Math.Subtract(1) | Odd}) })\n@fn(Odd input: Int output: Bool params: {} { If({Is(0)} {false} {Math.Subtract(1) | Even}) })\n7 | Even | Log\n6 | Odd | Log";
  assert_eq!(lines_of(source), ["false", "false"]);
}

#[test]
fn recursion_folds_a_tree_of_indices() {
  // Children are indices into flat sequences; `Sum` recurses over them.
  let source = r#"@fn(Sum input: None output: Int params: {node: Int values: [Int] kids: [[Any]]} {
  values | Take(node) | ExpectInt | Var(total)
  kids | Take(node) | ExpectSeq = children
  children | Count | Var(n)
  0 | Var(i)
  Repeat({
    children | Take(i) | ExpectInt = child
    Sum(node: child values: values kids: kids) | Math.Add(total) | Update(total)
    Inc(i)
  } times: n)
  total
})
[1 2 3 4] = values
[[1 2] [3] [] []] = kids
Sum(node: 0 values: values kids: kids) | Log
@fn(Fact input: Int output: Int params: {} { If({IsLess(2)} {1} {Math.Subtract(1) | Fact | Math.Multiply(input)}) })
5 | Fact | Log
@fn(Down input: Int output: Int params: {} { Pause If({IsLess(1)} {0} {Math.Subtract(1) | Down | Math.Add(1)}) })
3 | Down | Log"#;
  assert_eq!(lines_of(source), ["10", "120", "3"]);
  let report = check(source);
  assert!(report.ok(), "{}", report.to_json());
  let down = report
    .functions
    .iter()
    .find(|f| f.signature.name == "Down")
    .unwrap();
  assert!(down.signature.effects.suspends);
}

#[test]
fn function_declarations_are_checked() {
  for (source, code, needle) in [
    (
      "@fn(scale input: Int output: Int params: {} { 1 })",
      "declaration",
      "uppercase",
    ),
    (
      "@fn(Scale input: Int output: Int { 1 })",
      "declaration",
      "`params:`",
    ),
    (
      "@fn(Log input: Int output: Int params: {} { 1 })",
      "function-name-collision",
      "already a shard",
    ),
    (
      "@fn(S input: Integer output: Int params: {} { 1 })",
      "unknown-type",
      "unknown type `Integer`",
    ),
    (
      "@fn(S input: Int output: Int params: {input: Int} { 1 })",
      "reserved-name",
      "`input` is reserved",
    ),
    (
      "@fn(S input: Int output: Int params: {} looped: true { 1 })",
      "unsupported",
      "function option `looped`",
    ),
    (
      "@fn(S input: Int output: Int params: {} { 1 })\n@fn(S input: Int output: Int params: {} { 2 })",
      "duplicate-function",
      "declared twice",
    ),
    (
      "@fn(S input: Int output: Int params: {} uses: 3 { 1 })",
      "declaration",
      "mesh variable names",
    ),
    (
      "@fn(S input: Int output: Int params: {} { 1 })\n1 | S(factor: 2)",
      "unknown-argument",
      "no parameter `factor`",
    ),
    (
      "@fn(S input: Int output: Int params: {} { 1 })\n1 | Scale",
      "unknown-shard",
      "unknown shard `Scale`",
    ),
  ] {
    let d = load_errors(source);
    assert_eq!(d[0].code, code, "{source}: {:?}", d[0]);
    assert!(d[0].message.contains(needle), "{source}: {}", d[0].message);
  }
  let d = load_errors("@fn(Scal input: Int output: Int params: {} { 1 })\n1 | Scale");
  assert_eq!(d[0].did_you_mean, ["Scal"]);
  // Types: sequences, tables, unions and defaults.
  let source = "@fn(Sum input: [Int] output: Int params: {start: 10} { Count | Math.Add(start) })\n@fn(Pick input: {value: Int children: [Int]} output: Int | None params: {} { Take(\"value\") })\n[1 2 3] | Sum | Log\n[1 2 3] | Sum(start: 0) | Log\n{value: 7 children: [1]} | Pick | Log";
  assert_eq!(lines_of(source), ["13", "3", "7"]);
}

#[test]
fn preserving_reload_migrates_keep_slots_by_name_and_type() {
  let source = |keeps: &str| {
    format!(
      "@fn(C stateful: true input: None output: Any params: {{}} {{\n  {keeps}\n  n | Math.Add(1) | Update(n) | Log\n  m | Math.Add(1) | Update(m) | Log\n}})\n@wire(main {{ Keep(count 0) Inc(count) C }} looped: true)\n@mesh(m) @schedule(m main) @run(m)"
    )
  };
  let mut session = otter_lang::Session::new();
  session.set_reset_policy(otter_core::ResetPolicy::Apply);
  preserve(&mut session, &source("Keep(n 0) Keep(m 100)"));
  let (_, lines) = otter_core::log::capture(|| {
    session.tick();
    session.tick();
    // A changed initializer keeps the value; reordering keeps both.
    preserve(&mut session, &source("Keep(m 100) Keep(n 10)"));
    let report = session.reload_report().cloned().unwrap();
    assert_eq!(report.retained, ["C.n", "C.m"]);
    assert!(report.reset.is_empty(), "{report:?}");
    session.tick();
    // A changed type resets that slot and reports it; the other survives.
    preserve(&mut session, &source("Keep(m 100) Keep(n 0.5)"));
    let report = session.reload_report().cloned().unwrap();
    assert_eq!(report.retained, ["C.m"]);
    assert_eq!(report.reset, ["C.n"]);
    session.tick();
  });
  assert_eq!(lines, ["1", "101", "2", "102", "3", "103", "1.5", "104"]);
  // Under the embedding default policy, the type change is rejected.
  let mut session = otter_lang::Session::new();
  preserve(&mut session, &source("Keep(n 0) Keep(m 100)"));
  session.tick();
  let (_, d) = session
    .reload_preserving(
      Source::new("t.shs", source("Keep(n 0.5) Keep(m 100)")),
      &catalog(),
      &no_defines(),
    )
    .unwrap_err();
  assert_eq!(d[0].code, "reload-resets-state");
  assert!(d[0].message.contains("C.n"), "{}", d[0].message);
  assert_eq!(session.tick().len(), 0);
}

/// Every edge inside a recursive group resolves through the table pinned
/// at the group's outermost entry, so a chain in flight never mixes
/// revisions: the old `Even` reached from the old `Odd` calls the old `Odd`.
#[test]
fn a_recursive_group_in_flight_keeps_one_revision_on_every_edge() {
  let source = |base: i64| {
    format!(
      r#"@fn(Even input: Int output: Int params: {{}} {{ If({{Is(0)}} {{1}} {{Math.Subtract(1) | Odd}}) }})
@fn(Odd input: Int output: Int params: {{}} {{ If({{Is(0)}} {{{base}}} {{Pause Math.Subtract(1) | Even}}) }})
@wire(main {{ 3 | Even | Log }} looped: true)
@mesh(m) @schedule(m main) @run(m)"#
    )
  };
  let mut session = otter_lang::Session::new();
  preserve(&mut session, &source(0));
  let (_, lines) = otter_core::log::capture(|| {
    session.tick(); // Even(3) -> Odd(2), paused.
    assert!(preserve(&mut session, &source(5)).is_empty());
    session.tick(); // old Odd(2) -> old Even(1) -> old Odd(0) = 0
    session.tick(); // a new chain: Even(3) -> new Odd(2), paused.
    session.tick(); // -> new Odd(0) = 5
  });
  assert_eq!(lines, ["0", "5"]);
}

/// A stateful site whose replacement fails to instantiate keeps its old
/// component (still owned, cleaned up exactly once at the end) and retries
/// the selection at the next entry.
#[test]
fn a_failed_component_migration_keeps_the_old_component_owned() {
  use otter_core::shards::{ProbeEventKind, take_probe_events};
  let source = |probe: &str, extra: &str| {
    format!(
      r#"@fn(C stateful: true input: None output: Int params: {{}} {{ Keep(n 0) Probe("{probe}") {extra} n | Math.Add(1) | Update(n) }})
@wire(main {{ Maybe({{C}} {{-1}} silent: true) Log }} looped: true)
@mesh(m) @schedule(m main) @run(m)"#
    )
  };
  take_probe_events();
  let mut session = otter_lang::Session::new();
  // The probe counts as native state, which the default policy would
  // refuse to reset.
  session.set_reset_policy(otter_core::ResetPolicy::Apply);
  preserve(&mut session, &source("v0", ""));
  let (_, lines) = otter_core::log::capture(|| {
    session.tick();
    assert!(
      preserve(
        &mut session,
        &source("v1", r#"Probe("bad" "fail-instantiate")"#)
      )
      .is_empty()
    );
    session.tick();
    session.tick();
    drop(session);
  });
  assert_eq!(lines, ["1", "-1", "-1"]);
  let events = take_probe_events();
  let count = |tag: &str, kind| {
    events
      .iter()
      .filter(|e| e.tag == tag && e.kind == kind)
      .count()
  };
  // The failing probe never activates (the two caught errors above are
  // its two attempts, each rolling the new frame back); the old component
  // is cleaned up exactly once, at the end.
  assert_eq!(count("bad", ProbeEventKind::Activate), 0);
  assert_eq!(
    count("v1", ProbeEventKind::Cleanup),
    2,
    "two rolled-back replacements"
  );
  assert_eq!(
    count("v0", ProbeEventKind::Cleanup),
    1,
    "owned until the end, cleaned once"
  );
}

/// A later edit is judged against the body a component actually adopted,
/// not the one its wire was composed with: removing a `Keep` added by an
/// earlier accepted edit is a reset, and the default policy rejects it.
#[test]
fn reload_admission_follows_the_live_component_body() {
  let source = |keeps: &str| {
    format!(
      "@fn(C stateful: true input: None output: Int params: {{}} {{ {keeps} n | Math.Add(1) | Update(n) }})\n@wire(main {{ C | Log }} looped: true)\n@mesh(m) @schedule(m main) @run(m)"
    )
  };
  let mut session = otter_lang::Session::new();
  preserve(&mut session, &source("Keep(n 0)"));
  session.tick();
  assert!(preserve(&mut session, &source("Keep(n 0) Keep(extra 0)")).is_empty());
  session.tick(); // The component adopts the body with `extra`.
  let (_, d) = session
    .reload_preserving(
      Source::new("t.shs", source("Keep(n 0)")),
      &catalog(),
      &no_defines(),
    )
    .unwrap_err();
  assert_eq!(d[0].code, "reload-resets-state");
  assert!(d[0].message.contains("C.extra"), "{}", d[0].message);
}

// --- compose-time evaluation (docs/metaprogramming.md §2, M8) ---

/// The compose diagnostics of a source that loads.
fn compose_errors(text: &str) -> Vec<Diagnostic> {
  let report = check(text);
  assert!(!report.ok(), "expected compose errors");
  report.diagnostics
}

#[test]
fn compose_time_values_replace_their_pipelines() {
  let source = r#"@fn(Square input: Int output: Int params: {} { = x  x | Math.Multiply(x) })
@fn(Label input: None output: String params: {name: String number: Int} { [name "-" number] | String.Format })
@const(size #( 4 | Square ))
@const(bigger #( @size | Math.Add(1) ))
@const(plain 7)
#( 3 | Square ) | Log
@size | Log
@bigger | Log
@plain | Log
0 | Var(n)
Repeat({ Inc(n) } times: #( 2 | Square ))
n | Log(prefix: #( Label(name: "count" number: 2) ))
[1 #( 5 | Square ) @size] | Log
{a: #( 6 | Square ) b: @plain} | Log
2 | Square(x: #( 1 | Square ))"#;
  // The last call passes a parameter Square does not declare: compose-time
  // values go through the same checks as literals.
  let d = compose_errors(source);
  assert_eq!(d[0].code, "unknown-argument", "{d:?}");
  let source = source.rsplit_once('\n').unwrap().0;
  assert_eq!(
    lines_of(source),
    [
      "9",
      "16",
      "17",
      "7",
      "count-2: 4",
      "[1 25 16]",
      "{a: 36 b: 7}"
    ]
  );
}

#[test]
fn compose_time_results_take_their_exact_literal_type() {
  // `Answer` outputs Any; the literal it evaluates to is an Int, so the
  // arithmetic after it composes as if `42` were written by hand.
  let source =
    "@fn(Answer input: None output: Any params: {} { 42 })\n#( Answer ) | Math.Add(1) | Log";
  assert_eq!(lines_of(source), ["43"]);
  let d =
    compose_errors("@fn(Answer input: None output: Any params: {} { 42 })\nAnswer | Math.Add(1)");
  assert_eq!(d[0].code, "input-type-mismatch", "{d:?}");
}

#[test]
fn not_compose_time_is_located_also_through_functions() {
  let functions = r#"@fn(Noisy input: Int output: Int params: {} { Log })
@fn(Counter stateful: true input: Int output: Int params: {} { Keep(k 0) })
@fn(Waits input: Int output: Int params: {} { Pause() })
@fn(Stops input: Int output: Int params: {} { Stop })"#;
  for (expression, code, shard, location) in [
    ("#( 1 | Log )", "not-compose-time", "Log", (5, 8)),
    ("#( 1 | Noisy )", "not-compose-time", "Log", (1, 47)),
    ("#( Time.Now )", "not-compose-time", "Time.Now", (5, 4)),
    ("#( 1 | Keep(k 0) )", "not-compose-time", "Keep", (5, 8)),
    ("#( 1 | Counter )", "not-compose-time", "Keep", (2, 64)),
    ("#( 1 | Waits )", "not-compose-time", "Pause", (3, 47)),
    ("#( Pause() )", "not-compose-time", "Pause", (5, 4)),
    // Not on the list of shards evaluation may run, although it has no
    // effect.
    ("#( 1 | Stops )", "not-compose-time", "Stop", (4, 47)),
    ("#( 1 | Return )", "not-compose-time", "Return", (5, 8)),
  ] {
    let d = compose_errors(&format!("{functions}\n{expression}"));
    let d = d
      .iter()
      .find(|d| d.path.first() == Some(&otter_core::diagnostic::PathStep::Wire("root".into())))
      .unwrap_or_else(|| panic!("{expression}: {d:?}"));
    assert_eq!(
      (d.code, d.shard.as_deref()),
      (code, Some(shard)),
      "{expression}: {d:?}"
    );
    assert_eq!(at(d), location, "{expression}: {}", d.message);
    assert!(
      d.path
        .contains(&otter_core::diagnostic::PathStep::Evaluation),
      "{expression}: {:?}",
      d.path
    );
  }
  // A runtime value is invisible to the evaluation, with a hint saying why.
  let d = compose_errors("3 = x\n#( x | Math.Add(1) )");
  assert_eq!(d[0].code, "unknown-variable");
  assert!(
    d[0].message.contains("cannot see runtime values"),
    "{}",
    d[0].message
  );
  assert_eq!(at(&d[0]), (2, 4));
  // Mesh access is reached only through a function declaring it: the
  // diagnostic names that function and points at the read.
  let mut mesh = Mesh::new();
  mesh.declare_var("gain", Var::Int(2), true);
  let mut session = otter_lang::Session::with_mesh(mesh);
  let (_, d) = session
    .reload_preserving(
      Source::new(
        "t.shs",
        "@fn(Read input: Int output: Int params: {} uses: [gain] { Math.Add(gain) })\n#( 1 | Read ) | Log",
      ),
      &catalog(),
      &no_defines(),
    )
    .unwrap_err();
  assert_eq!(
    (d[0].code, d[0].shard.as_deref()),
    ("not-compose-time", Some("Math.Add")),
    "{}",
    d[0].message
  );
  assert!(
    d[0].message.contains("(reached through Read)") && d[0].message.contains("mesh variable gain"),
    "{}",
    d[0].message
  );
  assert_eq!(at(&d[0]), (1, 59));
}

#[test]
fn constants_built_from_each_other_stop_at_the_expansion_budget() {
  // Each line reads the previous constant eight times: seven lines would
  // build millions of elements.
  let chain = |first: &str| {
    let mut source = format!("@const(c0 [{first} 2 3 4 5 6 7 8])\n");
    for i in 1..=7 {
      let read = format!("@c{} ", i - 1).repeat(8);
      source += &format!("@const(c{i} [{}])\n", read.trim_end());
    }
    source + "@c7 | Count | Log\n"
  };
  // Values: lowered once and shared, bounded by their size as text, and
  // reported at the definition that passes it.
  let d = load_errors(&chain("1"));
  assert_eq!(d.len(), 1, "{d:?}");
  assert_eq!(d[0].code, "expansion-budget", "{}", d[0].message);
  assert!(d[0].message.contains("expands past"), "{}", d[0].message);
  assert!((2..=8).contains(&at(&d[0]).0), "{}", d[0].message);
  // Holding a `#( )`, a constant is lowered again at each read: the source
  // those reads lower is bounded in total.
  let d = load_errors(&chain("#( 1 )"));
  assert_eq!(d.len(), 1, "{d:?}");
  assert_eq!(d[0].code, "expansion-budget", "{}", d[0].message);
  assert!(d[0].message.contains("each read of"), "{}", d[0].message);
  // A constant read a few times stays well within it.
  assert_eq!(
    lines_of("@const(row [1 2 3])\n@const(grid [@row @row @row])\n@grid | Count | Log"),
    ["3"]
  );
}

#[test]
fn a_lookup_table_constant_is_shared_by_every_read() {
  // Many reads of one large table cost nothing per read: the reads together
  // are far past the expansion limit if each lowered the table again. On
  // the device, 16 reads of about 400 bytes each (every statement costs a
  // few KB of compose state of its own there).
  let (size, reads) = if cfg!(target_os = "espidf") {
    (100, 16)
  } else {
    (5000, 80)
  };
  let table: Vec<String> = (0..size).map(|i| (i * 7 % 1000).to_string()).collect();
  let mut source = format!("@const(lut [{}])\n", table.join(" "));
  for i in 0..reads {
    source += &format!("@lut | Take({}) = v{i}\n", i * 3 % size);
  }
  source += &format!("v{} | Log", reads - 1);
  let expected = ((reads - 1) * 3 % size * 7 % 1000).to_string();
  assert_eq!(lines_of(&source), [expected]);
}

#[test]
fn a_table_constant_built_from_copies_composes_as_it_is_held() {
  // A constant holding 8 copies of one holding 8 copies ..., of a table,
  // read many times: compose converts, types, hashes and compares it as
  // held (each level once), not expanded at every read. Natively about
  // 850 KB as text, which each read once rebuilt in full. On the device
  // 2 levels (1,754 bytes as the value limit measures text; 3 levels pass
  // its 4 KiB).
  let (levels, reads) = if cfg!(target_os = "espidf") {
    (2, 16)
  } else {
    (5, 400)
  };
  let mut source = String::from("@const(t0 {a: 1})\n");
  for i in 1..=levels {
    let p = format!("@t{}", i - 1);
    source += &format!("@const(t{i} [{p} {p} {p} {p} {p} {p} {p} {p}])\n");
  }
  for i in 0..reads {
    source += &format!("@t{levels} | Count = c{i}\n");
  }
  source += &format!("c{} | Log", reads - 1);
  assert_eq!(lines_of(&source), ["8"]);
}

#[test]
fn compose_time_cycles_are_reported_where_they_close() {
  // A function whose body evaluates itself.
  let d = compose_errors(
    "@fn(Selfish input: Int output: Int params: {} { #( 1 | Selfish ) })\n2 | Selfish",
  );
  let cycle = d
    .iter()
    .find(|d| d.code == "compose-time-cycle")
    .expect("cycle");
  assert_eq!(at(cycle), (1, 56));
  // Through another function.
  let d = compose_errors(
    "@fn(A input: Int output: Int params: {} { #( 1 | B ) })\n@fn(B input: Int output: Int params: {} { A })\n2 | A",
  );
  assert!(d.iter().any(|d| d.code == "compose-time-cycle"), "{d:?}");
  // Constants that read each other.
  let d = load_errors("@const(a #( @b | Math.Add(1) ))\n@const(b #( @a ))\n@a | Log");
  assert_eq!(d.len(), 1, "reported once: {d:?}");
  assert_eq!((d[0].code, at(&d[0])), ("compose-time-cycle", (2, 14)));
  assert!(d[0].message.contains("a -> b -> a"), "{}", d[0].message);
  // Names: a constant cannot reuse a script argument's or another's.
  let d = load_errors("@const(a 1)\n@const(a 2)\n@a | Log");
  assert_eq!((d[0].code, at(&d[0])), ("duplicate-binding", (2, 8)));
}

#[test]
fn compose_time_failures_point_at_the_shard_that_failed() {
  // The path goes through the evaluation to the failing shard, whatever
  // ran it: flat code, a composite's child, a function in its own frame,
  // inlined, or run by the VM on a kept frame.
  // Past the inlining budget: it stays a call.
  let big = format!(
    "@fn(Big input: Int output: Int params: {{}} {{ = d{} 10 | Math.Divide(d) }})\n",
    " Math.Add(0)".repeat(25)
  );
  for (source, message, (line, needle), path) in [
    (
      "1 | Log\n#( 1 | Math.Divide(0) ) | Log".to_string(),
      "division by zero",
      (2, "Math.Divide"),
      "root/2:Const/value/#()/1:Math.Divide",
    ),
    (
      "1 | Log\n#( [1 2] | Take(5) ) | Log".to_string(),
      "out of range",
      (2, "Take"),
      "root/2:Const/value/#()/1:Take",
    ),
    (
      "1 | Log\n#( \"x\" | ParseInt ) | Log".to_string(),
      "is not an Int",
      (2, "ParseInt"),
      "root/2:Const/value/#()/1:ParseInt",
    ),
    // A flattened composite's child.
    (
      "#( 1 | When({ true } { Math.Divide(0) }) ) | Log".to_string(),
      "division by zero",
      (1, "Math.Divide"),
      "root/0:Const/value/#()/1:When/action/0:Math.Divide",
    ),
    // A child flow a composite runs inside its own step (a `Match` case).
    (
      "#( 1 | Match([1 { Math.Divide(0) }] default: {}) ) | Log".to_string(),
      "division by zero",
      (1, "Math.Divide"),
      "root/0:Const/value/#()/1:Match/cases/#0/0:Math.Divide",
    ),
    // An inlined function.
    (
      "@fn(Broken input: Int output: Int params: {} { Math.Divide(0) })\n#( 1 | Broken ) | Log"
        .to_string(),
      "division by zero",
      (1, "Math.Divide"),
      "root/0:Const/value/#()/1:Broken/Broken()/0:Math.Divide",
    ),
    // A function in a frame of its own (a shard without a VM form).
    (
      "@fn(Parse1 input: String output: Int params: {} { ParseInt })\n#( \"x\" | Parse1 ) | Log"
        .to_string(),
      "is not an Int",
      (1, "ParseInt"),
      "root/0:Const/value/#()/1:Parse1/Parse1()/0:ParseInt",
    ),
    // A body too large to inline, called again in a loop: the VM runs it
    // on its kept frame, and the third call fails.
    (
      format!(
        "{big}#( 2 | Var(n) Repeat({{ n | Big  n | Math.Subtract(1) | Update(n) }} times: 3) n ) | Log"
      ),
      "division by zero",
      (1, "Math.Divide"),
      "root/0:Const/value/#()/2:Repeat/action/1:Big/Big()/27:Math.Divide",
    ),
    // A block a function runs, where the call wrote it: in a frame of its
    // own, inlined with its callee, and passed on through another function.
    (
      format!("{TWICE}#( 1 | Twice(action: {{\"x\" | ParseInt}}) ) | Log"),
      "is not an Int",
      (4, "ParseInt"),
      "root/0:Const/value/#()/1:Twice/action/1:ParseInt",
    ),
    (
      format!("{TWICE}#( 1 | Twice(action: {{Math.Divide(0)}}) ) | Log"),
      "division by zero",
      (4, "Math.Divide"),
      "root/0:Const/value/#()/1:Twice/action/0:Math.Divide",
    ),
    (
      format!(
        "{TWICE}@fn(Outer input: Int output: Int params: {{action: Flow(input: Int output: Int)}} {{ Twice(action: action) }})\n#( 1 | Outer(action: {{\"x\" | ParseInt}}) ) | Log"
      ),
      "is not an Int",
      (5, "ParseInt"),
      "root/0:Const/value/#()/1:Outer/action/1:ParseInt",
    ),
  ] {
    let d = compose_errors(&source);
    assert_eq!(d[0].code, "compose-time-error", "{source}: {d:?}");
    assert!(d[0].message.contains(message), "{source}: {}", d[0].message);
    let column = source
      .lines()
      .nth(line as usize - 1)
      .unwrap()
      .find(needle)
      .unwrap() as u32
      + 1;
    assert_eq!(
      (at(&d[0]), d[0].path_string().as_str()),
      ((line, column), path),
      "{source}"
    );
  }
}

#[test]
fn compose_time_budgets_end_runaway_evaluations() {
  for (source, needle) in [
    // Flat code: the loop is lowered to jumps in the evaluated pipeline.
    ("#( 0 | Repeat({ Math.Add(1) } forever: true) )", "fuel"),
    // A framed call: a recursive function runs in frames of its own.
    (
      "@fn(Spin input: Int output: Int params: {} { When({ IsLess(0) } { Spin }) Repeat({ Math.Add(1) } forever: true) })\n#( 0 | Spin )",
      "fuel",
    ),
    // Unbounded recursion.
    (
      "@fn(Deeper input: Int output: Int params: {} { Math.Add(1) | Deeper })\n#( 0 | Deeper )",
      "depth limit",
    ),
    // A growing value stops before the allocation that would exceed the
    // value limit.
    (
      "#( [1] | Var(s) Repeat({ 1 | Push(s) } forever: true) )",
      "value limit",
    ),
    // Wrapping a value in itself stops at the nesting limit.
    (
      "#( [] | Var(s) Repeat({ [s] | Update(s) } forever: true) )",
      "nest deeper",
    ),
  ] {
    let d = compose_errors(source);
    let d = d
      .iter()
      .find(|d| d.code == "expansion-budget")
      .unwrap_or_else(|| panic!("{source}: {d:?}"));
    assert!(d.message.contains(needle), "{source}: {}", d.message);
    assert!(
      d.path_string().ends_with("#()"),
      "{source}: {}",
      d.path_string()
    );
  }
  // Match pays for each case it compares by size: a thousand comparisons of
  // 64-element sequences cost far more than the thousand dispatches.
  let input: Vec<String> = (0..63)
    .map(|i| i.to_string())
    .chain(["-1".into()])
    .collect();
  let case: Vec<String> = (0..64).map(|i| i.to_string()).collect();
  let source = format!(
    "#( 0 | Var(k) Repeat({{ [{}] | Match([[{}] {{ 1 }}] default: {{ 0 }}) Inc(k) }} times: 1000) k ) | Log",
    input.join(" "),
    case.join(" ")
  );
  let mut mesh = Mesh::new();
  mesh.set_eval_limits(otter_core::compose_time::EvalLimits {
    fuel: 10_000,
    ..Default::default()
  });
  let mut session = otter_lang::Session::with_mesh(mesh);
  let (_, d) = session
    .reload_preserving(Source::new("t.shs", &source), &catalog(), &no_defines())
    .unwrap_err();
  assert_eq!(d[0].code, "expansion-budget", "{}", d[0].message);
  assert!(d[0].message.contains("fuel"), "{}", d[0].message);
}

#[test]
fn an_evaluation_retaining_what_it_allocates_ends_at_its_budget() {
  // Every iteration allocates a new sequence and keeps it: the live heap
  // grows with the fuel spent, under the platform's default limits (on the
  // device, within its heap).
  let d = compose_errors(
    "#( [] | Var(all) 0 | Var(k) Repeat({ [k k k k k k k k] | Push(all) Inc(k) } forever: true) all ) | Log",
  );
  assert_eq!(d[0].code, "expansion-budget", "{}", d[0].message);
}

#[test]
fn expanding_a_function_is_not_a_macro() {
  let d = load_errors(
    "@fn(Square input: Int output: Int params: {} { = x  x | Math.Multiply(x) })\n4 | @Square()",
  );
  assert_eq!((d[0].code, at(&d[0])), ("not-a-macro", (2, 5)));
}

#[test]
fn implicit_loop_variables_are_rejected_with_the_1x_help() {
  let d = load_errors("[1 2] | Take(0) | Log($0)");
  assert_eq!(d[0].code, "implicit-loop-variable");
  assert_eq!(at(&d[0]), (1, 23));
  assert!(d[0].message.contains("`$0`"), "{}", d[0].message);
  assert!(
    d[0].message.contains("the element is the block's input"),
    "{}",
    d[0].message
  );
}

#[test]
fn preserving_reload_updates_compose_time_values() {
  let source = |value| {
    format!(
      r#"@fn(Base input: None output: Int params: {{}} {{ {value} }})
@fn(Show input: None output: Int params: {{}} {{ #( Base | Math.Add(1) ) | Log }})
@wire(main {{ #( Base ) | Log Show Pause }} looped: true)
@mesh(m) @schedule(m main) @run(m)"#
    )
  };
  let mut session = otter_lang::Session::new();
  preserve(&mut session, &source(10));
  let (_, lines) = otter_core::log::capture(|| {
    session.tick();
    assert!(preserve(&mut session, &source(20)).is_empty());
    session.tick();
    session.tick();
  });
  assert_eq!(lines, ["10", "11", "20", "21"]);
}

// --- M9: flow parameters (docs/metaprogramming.md §3) ---

/// `Retry` as a function: runs the block up to `times` times, stops at the
/// first success, and fails with the last attempt's error.
const RETRY: &str = r#"@fn(Retry input: None output: None params: {times: Int action: Flow(input: None)} {
  false | Var(done)
  1 | Var(attempt)
  While({All({done | Not} {attempt | IsLess(times)})} {
    Maybe({Run(action) true | Update(done)} silent: true)
    Inc(attempt)
  })
  When({done | Not} {Run(action)})
  none
})
"#;

/// `Timed` as a function: the seconds the block took.
const TIMED: &str = r#"@fn(Timed input: None output: Float params: {action: Flow(input: None)} {
  Time.Now = start
  Run(action)
  Time.Now | Math.Subtract(start)
})
"#;

/// The same `Retry`, as a native composite a host crate writes against the
/// public interface (`ControlShard`): this test crate is not otter-core.
mod host_retry {
  use otter_core::describe::{
    Forms, InputDesc, OutputDesc, ParamDecl, Params, Requirement, ShardDesc, Targets, TypeName,
  };
  use otter_core::flow::CompiledFlow;
  use otter_core::{ActivationCtx, Args, ComposeCtx, Composed, ControlShard, ControlStep};
  use otter_core::{Result, ShardType, Var};

  static PARAMS: &[ParamDecl] = &[
    ParamDecl::new(
      "times",
      "",
      Forms::LITERAL,
      &[TypeName::Int],
      Requirement::Required,
    ),
    ParamDecl::new("action", "", Forms::FLOW, &[], Requirement::Required),
  ];

  pub static RETRY: ShardType = ShardType::new(ShardDesc {
    params: Params::Declared(PARAMS),
    input: InputDesc::Any,
    output: OutputDesc::Passthrough,
    targets: Targets::All,
    effects: otter_core::signature::Effects::NONE,
    lifetime: otter_core::signature::Lifetime::Stateless,
    ..ShardDesc::undocumented("Test.Retry", 1)
  })
  .controlled_by::<Retry>();

  pub struct Retry;

  pub struct Compiled {
    flows: Vec<CompiledFlow>,
    times: i64,
  }

  /// The attempt running.
  #[derive(Default)]
  pub struct Attempt(i64);

  impl ControlShard for Retry {
    type Compiled = Compiled;
    type State = Attempt;
    const NAME: &'static str = "Test.Retry";

    fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Compiled>> {
      let input = ctx.input();
      let action = ctx.compose_flow_conditional(args.flow("action").expect("decoded"), input)?;
      Ok(Composed {
        compiled: Compiled {
          flows: vec![action],
          times: args.int("times").expect("decoded"),
        },
        output: input,
      })
    }

    fn flows(compiled: &Compiled) -> &[CompiledFlow] {
      &compiled.flows
    }

    fn resume(
      compiled: &Compiled,
      attempt: &mut Attempt,
      _: &mut ActivationCtx<'_>,
      input: &Var,
      completion: Option<Result<Var>>,
    ) -> Result<ControlStep> {
      match completion {
        None => {
          attempt.0 = 1;
          Ok(ControlStep::Enter(0, input.clone()))
        }
        Some(Ok(_)) => Ok(ControlStep::Complete(input.clone())),
        Some(Err(_)) if attempt.0 < compiled.times => {
          attempt.0 += 1;
          Ok(ControlStep::Enter(0, input.clone()))
        }
        Some(Err(err)) => Err(err),
      }
    }
  }
}

/// Compose (or load) errors of a source, located.
fn check_errors(text: &str) -> Vec<Diagnostic> {
  let report = check(text);
  assert!(!report.ok(), "expected errors");
  report.diagnostics
}

fn host_catalog() -> Catalog {
  Catalog::new(&[otter_core::shards::CATALOG, &[&host_retry::RETRY]]).unwrap()
}

fn run_with(catalog: &Catalog, text: &str) -> otter_lang::RunReport {
  match Program::load(Source::new("t.shs", text), catalog, &no_defines()) {
    Ok(p) => p.run().unwrap_or_else(|d| panic!("run failed: {d:?}")),
    Err((_, d)) => panic!("load failed: {d:?}"),
  }
}

/// A call of the script `Retry`, or the same through the host composite.
fn retry_call(host: bool, times: u32, block: &str) -> String {
  if host {
    format!("Test.Retry(times: {times} action: {{{block}}})")
  } else {
    format!("Retry(times: {times} action: {{{block}}})")
  }
}

#[test]
fn retry_stops_at_the_first_success_and_updates_the_callers_variables() {
  for host in [false, true] {
    // Fails on the first two attempts, through a suspension each.
    let block = r#"Inc(tries) tries | Log("try") Pause When({tries | IsLess(3)} {"x" | ParseInt})"#;
    let text = format!(
      "{RETRY}0 | Var(tries)\nnone | {}\ntries",
      retry_call(host, 5, block)
    );
    let (report, lines) = otter_core::log::capture(|| run_with(&host_catalog(), &text));
    assert_eq!(completed(&report, "root"), Var::Int(3), "host: {host}");
    assert_eq!(lines, ["try: 1", "try: 2", "try: 3"], "host: {host}");
  }
}

#[test]
fn retry_fails_with_the_last_attempts_error_which_a_maybe_around_it_catches() {
  for host in [false, true] {
    let block = r#"Inc(tries) f"x{tries}" | ParseInt"#;
    let text = format!(
      "{RETRY}0 | Var(tries)\nMaybe({{none | {}}} {{-1}})\ntries",
      retry_call(host, 3, block)
    );
    let (report, lines) = otter_core::log::capture(|| run_with(&host_catalog(), &text));
    assert_eq!(completed(&report, "root"), Var::Int(3), "host: {host}");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("\"x3\""), "host: {host}: {lines:?}");
  }
}

#[test]
fn cancelling_a_suspended_block_cleans_every_state_once() {
  for host in [false, true] {
    otter_core::shards::take_probe_events();
    let text = format!(
      "{RETRY}@wire(main {{ Probe(\"caller\") 0 | Var(tries) none | {} }} looped: true)\n@mesh(m) @schedule(m main) @run(m)",
      retry_call(host, 3, r#"Probe("block") Inc(tries) Pause(10.0)"#)
    );
    let mut session = otter_lang::Session::new();
    session
      .reload(Source::new("t.shs", &text), &host_catalog(), &no_defines())
      .unwrap_or_else(|(_, d)| panic!("load failed: {d:?}"));
    session.tick();
    session.tick();
    session.stop();
    let events = otter_core::shards::take_probe_events();
    for tag in ["caller", "block"] {
      let count = |kind| {
        events
          .iter()
          .filter(|e| &*e.tag == tag && e.kind == kind)
          .count()
      };
      use otter_core::shards::ProbeEventKind::*;
      assert_eq!(count(Instantiate), 1, "host: {host}, {tag}");
      assert_eq!(count(Cleanup), 1, "host: {host}, {tag}");
    }
  }
}

#[test]
fn timed_outputs_the_seconds_its_block_took() {
  let text = format!(
    "{TIMED}0 | Var(n)\nTimed(action: {{Pause(0.02) Inc(n)}}) = took\n[took | IsMoreEqual(0.02) n]"
  );
  assert_eq!(
    completed(&run(&text, &no_defines()), "root"),
    Var::from_seq(std::sync::Arc::new(vec![Var::Bool(true), Var::Int(1)]))
  );
}

/// Runs its block twice, feeding the first result to the second.
const TWICE: &str = "@fn(Twice input: Int output: Int params: {action: Flow(input: Int output: Int)} {\n  Run(action) | Run(action)\n})\n";

/// A loop over the first `n` elements, as a function: inlined with its
/// block when the block is straight-line code (`n` is a parameter because
/// `Count` has no VM form), framed otherwise.
const EACH: &str = r#"@fn(Each input: [Int] output: [Int] params: {n: Int action: Flow(input: Int)} {
  = xs
  0 | Var(i)
  While({i | IsLess(n)} { xs | Take(i) | Run(action) Inc(i) })
  xs
})
"#;

/// `Each`, counting the sequence itself: `Count` has no VM form, so the
/// call keeps its frame, and `Run` runs a block of leaf code in place.
const EACH_COUNTED: &str = r#"@fn(Each input: [Int] output: [Int] params: {n: Int action: Flow(input: Int)} {
  = xs
  0 | Var(i)
  xs | Count = m
  While({i | IsLess(n)} { xs | Take(i) | Run(action) Inc(i) })
  xs
})
"#;

#[test]
fn the_inlined_and_the_framed_paths_behave_the_same() {
  // Inlined with its callee; run in place by a framed callee's `Run`; in a
  // frame of its own (`Probe` is not leaf code).
  for (each, framed) in [(EACH, ""), (EACH_COUNTED, ""), (EACH, r#"Probe("b")"#)] {
    let case = format!("{framed:?} counted: {}", each == EACH_COUNTED);
    // Caller mutation.
    let text = format!(
      "{each}0 | Var(total)\n[1 2 3] | Each(n: 3 action: {{{framed} Math.Add(total) | Update(total)}})\ntotal"
    );
    assert_eq!(
      completed(&run(&text, &no_defines()), "root"),
      Var::Int(6),
      "{case}"
    );
    // A failure inside the block, after it updated the caller.
    let text = format!(
      "{each}0 | Var(total)\n[0 1 2] = ks\nMaybe({{[1 5 2] | Each(n: 3 action: {{{framed} = k ks | Take(k) | Math.Add(total) | Update(total)}})}} silent: true)\ntotal"
    );
    assert_eq!(
      completed(&run(&text, &no_defines()), "root"),
      Var::Int(1),
      "{case}"
    );
    // Cancellation: the caller suspends after the call; nothing leaks.
    otter_core::shards::take_probe_events();
    let text = format!(
      "{each}@wire(main {{ Probe(\"caller\") 0 | Var(total) [1 2] | Each(n: 2 action: {{{framed} Math.Add(total) | Update(total)}}) Pause(10.0) }} looped: true)\n@mesh(m) @schedule(m main) @run(m)"
    );
    let mut session = otter_lang::Session::new();
    session
      .reload(Source::new("t.shs", &text), &catalog(), &no_defines())
      .unwrap_or_else(|(_, d)| panic!("load failed: {d:?}"));
    session.tick();
    session.stop();
    let events = otter_core::shards::take_probe_events();
    let count = |kind| events.iter().filter(|e| e.kind == kind).count();
    use otter_core::shards::ProbeEventKind::*;
    assert_eq!(count(Instantiate), count(Cleanup), "{case}");
  }
}

#[test]
fn a_flow_parameter_passes_on_to_functions_and_native_shards() {
  let text = format!(
    "{RETRY}@fn(Twice input: None output: None params: {{action: Flow(input: None)}} {{ Retry(times: 2 action: action) When({{true}} action) none }})\n0 | Var(n)\nTwice(action: {{Inc(n)}})\nn"
  );
  assert_eq!(completed(&run(&text, &no_defines()), "root"), Var::Int(2));
}

#[test]
fn output_none_discards_a_functions_output() {
  // As a block for an `output: None` parameter: a body that ends on a
  // value outputs None, inlined or framed, the value released.
  for (body, logged) in [
    ("\"kept\" | Log(\"in\") [1 2]", true),
    ("Pause \"kept\" | Log(\"in\") [1 2]", true),
    ("\"kept\" 1 | Math.Add(1)", false),
  ] {
    let text = format!(
      "@fn(Note input: None output: None params: {{}} {{ {body} }})\nNote | Log(\"out\")\nNote | IsNone"
    );
    let (report, lines) = otter_core::log::capture(|| run(&text, &no_defines()));
    assert_eq!(completed(&report, "root"), Var::Bool(true), "{body}");
    let expected: &[&str] = if logged {
      &["in: kept", "out: none", "in: kept"]
    } else {
      &["out: none"]
    };
    assert_eq!(lines, expected, "{body}");
  }
  // A declared output other than None is still checked.
  let d = check_errors("@fn(One input: None output: String params: {} { 1 })\nOne");
  assert_eq!(d[0].code, "output-type-mismatch", "{d:?}");
}

#[test]
fn count_runs_in_the_vm_for_every_kind() {
  // Inlined into the caller (its call site reports `inlined`), and the
  // same at compose time, where a string's length is metered.
  let text = "@fn(Len input: Any output: Int params: {} { Count })
[1 2 3] | Len | Log
\"h\u{e9}llo\" | Len | Log
{a: 1 b: 2} | Len | Log
\"abc\" | StringToBytes | Len | Log
#(\"h\u{e9}llo\" | Count) | Log";
  // (The function on its own fails: its declared input is `Any`.)
  let report = check(text);
  let paths: Vec<&str> = report.wires[0]
    .occurrences
    .iter()
    .filter_map(|o| o.occurrence.call.as_ref().map(|c| c.path))
    .collect();
  assert_eq!(paths, ["inlined"; 4]);
  let (_, lines) = otter_core::log::capture(|| run(text, &no_defines()));
  assert_eq!(lines, ["3", "5", "2", "3", "5"]);
}

#[test]
fn check_reports_how_each_call_site_runs() {
  let adds = vec!["Math.Add(1)"; 30].join(" ");
  let text = format!(
    "@fn(Each input: [Int] output: [Int] params: {{action: Flow(input: Int)}} {{
  = xs 0 | Var(i) xs | Count = n
  While({{i | IsLess(n)}} {{ xs | Take(i) | Run(action) Inc(i) }})
  xs
}})
@fn(Noisy input: Int output: Int params: {{}} {{ Log(\"n\") }})
@fn(Long input: Int output: Int params: {{}} {{ {adds} }})
@fn(Counter stateful: true input: None output: Int params: {{}} {{ Keep(c 0) Inc(c) Get(c) }})
0 | Var(total)
[1 2 3] | Each(action: {{Math.Add(total) | Update(total)}})
[1 2 3] | Each(action: {{Log | Math.Add(total) | Update(total)}})
1 | Noisy
1 | Long
Counter"
  );
  let report = check(&text);
  assert!(report.ok(), "{}", report.to_json());
  let sites: Vec<(u32, String, Option<String>)> = report.wires[0]
    .occurrences
    .iter()
    .filter_map(|o| {
      let call = o.occurrence.call.as_ref()?;
      Some((
        o.line?,
        call.path.to_string(),
        call.reason.as_ref().map(|r| r.to_string()),
      ))
    })
    .collect();
  let reason = |r: &str| Some(r.to_string());
  assert_eq!(
    sites,
    [
      (10, "inlined".into(), None),
      (
        11,
        "framed".into(),
        reason("the block for action: Log has no VM form")
      ),
      (12, "framed".into(), reason("its body: Log has no VM form")),
      (
        13,
        "vm".into(),
        reason("its body is 30 instructions, over the budget of 24")
      ),
      (
        14,
        "framed".into(),
        reason("it is stateful (each site keeps an instance)")
      ),
    ]
  );
  // In JSON (a small program: the whole report above is tens of KB, more
  // than the device's largest free block).
  let json = check(
    "@fn(Noisy input: Int output: Int params: {} { Log(\"n\") })\n@fn(One input: Int output: Int params: {} { Math.Add(1) })\n1 | Noisy | One",
  )
  .to_json();
  assert!(json.contains("\"call\":{\"path\":\"inlined\"}"), "{json}");
  assert!(
    json.contains("\"call\":{\"path\":\"framed\",\"reason\":\"its body: Log has no VM form\"}"),
    "{json}"
  );
}

#[test]
fn a_block_cannot_own_state() {
  // A block starts fresh each time it runs: a stateful call inside it is
  // refused, at a wire's top level and inside a stateful function, as a
  // `Once` is. Before, the callee's state survived between runs of one
  // `Run` and not between two.
  const COUNTER: &str =
    "@fn(Counter stateful: true input: None output: Int params: {} { Keep(n 0) Inc(n) Get(n) })\n";
  let d = check_errors(&format!(
    "{RETRY}{COUNTER}Retry(times: 2 action: {{Counter | Log}})"
  ));
  assert_eq!(d[0].code, "stateful-call-in-stateless", "{d:?}");
  assert_eq!(d[0].shard.as_deref(), Some("Counter"));
  assert!(d[0].message.contains("starts fresh"), "{}", d[0].message);
  let d = check_errors(&format!(
    "{RETRY}{COUNTER}@fn(Outer stateful: true input: None output: Int params: {{}} {{ Retry(times: 2 action: {{Counter | Log}}) Counter }})\nOuter"
  ));
  assert_eq!(d[0].code, "stateful-call-in-stateless", "{d:?}");
  let d = check_errors(&format!(
    "{RETRY}Retry(times: 2 action: {{Once({{1 | Log}})}})"
  ));
  assert_eq!(d[0].code, "once-in-stateless", "{d:?}");
  // Outside the block, the caller owns the instance and keeps it.
  let text = format!(
    "{RETRY}{COUNTER}0 | Var(last)\nRepeat({{Counter | Update(last)}} times: 3)\nRetry(times: 2 action: {{last | Log}})\nlast"
  );
  assert_eq!(completed(&run(&text, &no_defines()), "root"), Var::Int(3));
}

#[test]
fn return_and_stop_inside_blocks() {
  // A Return leaving the block is refused, located at the Return.
  let d = check_errors(&format!("{RETRY}Retry(times: 2 action: {{Return}})"));
  assert_eq!((d[0].code, at(&d[0])), ("control-in-flow", (11, 25)));
  // A Return inside a function the block calls ends that function.
  let text = format!(
    "{RETRY}@fn(Early input: None output: Int params: {{}} {{ 1 | Return 2 }})\n0 | Var(n)\nRetry(times: 2 action: {{Early | Update(n)}})\nn"
  );
  assert_eq!(completed(&run(&text, &no_defines()), "root"), Var::Int(1));
  // Stop inside a block ends the wire.
  let text = format!("{RETRY}Retry(times: 2 action: {{Stop}})\n\"after\" | Log");
  let (report, lines) = otter_core::log::capture(|| run(&text, &no_defines()));
  assert!(lines.is_empty(), "{lines:?}");
  assert!(
    matches!(&report.outcomes[0].1, Some(Outcome::Stopped)),
    "{:?}",
    report.outcomes
  );
}

#[test]
fn a_flow_parameter_cannot_escape_its_call() {
  let header = "@fn(F input: Int output: Int params: {action: Flow(input: Int output: Int)} {";
  for (body, column) in [
    ("action = f 1", 79),                     // bound with `=`
    ("action | Var(f) 1", 79),                // bound with Var
    ("action | Return", 79),                  // returned
    ("[action] | Count", 79),                 // stored in a collection
    ("1 | Var(xs) [1] | Push(action) 1", 97), // assigned
  ] {
    let d = check_errors(&format!(
      "{header} {body} }})\n1 | F(action: {{Math.Add(1)}})"
    ));
    assert_eq!(d[0].code, "flow-escapes", "{body}: {:?}", d[0]);
    assert_eq!(at(&d[0]), (1, column), "{body}");
  }
  // Kept, or written to a mesh variable, from a stateful function.
  let d = check_errors(
    "@fn(F stateful: true input: Int output: Int params: {action: Flow(input: Int output: Int)} { Keep(action 0) })\n1 | F(action: {Math.Add(1)})",
  );
  assert_eq!(d[0].code, "duplicate-binding", "{:?}", d[0]);
  let d = check_errors(
    "@fn(F input: Int output: Int params: {action: Flow(input: Int output: Int)} { action | Update(level) })\n1 | F(action: {Math.Add(1)})",
  );
  assert_eq!(d[0].code, "flow-escapes", "{:?}", d[0]);
}

#[test]
fn the_same_function_has_the_effects_of_each_sites_block() {
  let report = check(
    "@fn(Twice input: Int output: Int params: {action: Flow(input: Int output: Int)} { Run(action) | Run(action) })\n1 | Twice(action: {Math.Add(1)})\n2 | Twice(action: {Pause Math.Add(1)})",
  );
  assert!(report.ok(), "{}", report.to_json());
  let sites: Vec<bool> = report.wires[0]
    .occurrences
    .iter()
    .filter(|o| o.occurrence.path.len() == 2)
    .filter(|o| {
      matches!(&o.occurrence.path[1], otter_core::diagnostic::PathStep::Shard { name, .. } if name == "Twice")
    })
    .map(|o| o.occurrence.effects.suspends)
    .collect();
  assert_eq!(sites, [false, true]);
  let json = report.to_json();
  assert!(json.contains("\"suspends\":true"), "{json}");
}

#[test]
fn ignored_inputs_and_outputs_of_blocks() {
  // `input: None`: the block receives none, whatever Run's input; `output:
  // None`: the block's output is discarded and Run outputs none.
  let text = "@fn(Both input: Int output: None params: {action: Flow(input: None output: None)} { Run(action) })\n0 | Var(n)\n5 | Both(action: {Inc(n) n | Math.Add(10)}) = out\n[out n]";
  assert_eq!(
    completed(&run(text, &no_defines()), "root"),
    Var::from_seq(std::sync::Arc::new(vec![Var::None, Var::Int(1)]))
  );
}

// --- `@include` and `@read`, and Bytes ---

fn files(entries: &[(&str, &str)]) -> otter_lang::MemoryFiles {
  let mut files = otter_lang::MemoryFiles::new();
  for (name, text) in entries {
    files.insert(name, text.as_bytes());
  }
  files
}

fn load_with(text: &str, files: &otter_lang::MemoryFiles) -> Result<Program, Vec<Diagnostic>> {
  Program::load_with(
    Source::new("main.shs", text),
    &catalog(),
    &no_defines(),
    files,
  )
  .map_err(|(_, d)| d)
}

#[test]
fn an_included_file_declares_once_wherever_it_is_named() {
  let files = files(&[
    (
      "lib/flow.shs",
      "@include(\"../lib/flow.shs\")\n@include(\"more.shs\")\n@fn(Twice input: Int output: Int params: {} { Math.Multiply(2) })",
    ),
    ("lib/more.shs", "@const(base 10)\n@include(\"flow.shs\")"),
  ]);
  let program = load_with(
    "@include(\"lib/flow.shs\")\n@include(\"./lib/more.shs\")\n@base | Twice",
    &files,
  )
  .unwrap_or_else(|d| panic!("{d:?}"));
  assert_eq!(program.files, ["lib/flow.shs", "lib/more.shs"]);
  let report = program.run().unwrap_or_else(|d| panic!("{d:?}"));
  assert_eq!(completed(&report, "root"), Var::Int(20));
}

#[test]
fn problems_in_included_files_are_located_there() {
  let files = files(&[
    (
      "lib.shs",
      "// helpers\n@fn(Bad input: Int output: Int params: {} {\n  \"x\" | Math.Add(1)\n})",
    ),
    (
      "broken.shs",
      "@fn(B input: Int output: Int params: {} {\n  Math.Add(\n})",
    ),
  ]);
  // A compose error inside an included function.
  let program = load_with("@include(\"lib.shs\")\n1 | Bad", &files).unwrap();
  let d = &program.compose()[0];
  assert_eq!(
    (d.code, d.file.as_deref(), at(d)),
    ("input-type-mismatch", Some("lib.shs"), (3, 9))
  );
  // A syntax error in an included file.
  let d = load_with("@include(\"broken.shs\")", &files)
    .err()
    .expect("load errors");
  assert_eq!(
    (d[0].code, d[0].file.as_deref(), at(&d[0])),
    ("unclosed", Some("broken.shs"), (2, 11))
  );
  // A missing file, and an include inside a wire.
  let d = load_with("1 | Log\n@include(\"nope.shs\")", &files)
    .err()
    .expect("load errors");
  assert_eq!(
    (d[0].code, d[0].file.as_deref(), at(&d[0])),
    ("file-not-found", Some("main.shs"), (2, 1))
  );
  let d = load_with("When({true} {@include(\"lib.shs\")})", &files)
    .err()
    .expect("load errors");
  assert_eq!((d[0].code, at(&d[0])), ("unsupported", (1, 14)));
}

#[test]
fn read_gives_a_files_text_or_bytes_as_a_literal() {
  let mut files = files(&[("data/hello.txt", "hi\n")]);
  files.insert("data/blob.bin", [0u8, 255, 16]);
  let text = r#"@const(greeting @read("data/hello.txt"))
@read("data/blob.bin" bytes: true) = blob
[@greeting
 blob | Count
 blob | ToHex
 "hi\n" | StringToBytes | BytesToString | Is(@greeting)
 blob | Is(@read("data/blob.bin" bytes: true))]"#;
  let program = load_with(text, &files).unwrap_or_else(|d| panic!("{d:?}"));
  assert_eq!(program.files, ["data/hello.txt", "data/blob.bin"]);
  let report = program.run().unwrap_or_else(|d| panic!("{d:?}"));
  assert_eq!(
    completed(&report, "root"),
    Var::from_seq(std::sync::Arc::new(vec![
      Var::string("hi\n"),
      Var::Int(3),
      Var::string("00ff10"),
      Var::Bool(true),
      Var::Bool(true),
    ]))
  );
  // Text that is not UTF-8 needs `bytes: true`.
  let d = load_with(r#"@read("data/blob.bin") | Log"#, &files)
    .err()
    .expect("load errors");
  assert_eq!((d[0].code, at(&d[0])), ("not-utf8", (1, 1)));
}

#[test]
fn bytes_are_a_type_with_their_own_printing() {
  let report = check(
    "@fn(Size input: Bytes output: Int params: {} { Count })\n\"abc\" | StringToBytes | Size\n#( \"ab\" | StringToBytes ) | Log",
  );
  assert!(report.ok(), "{}", report.to_json());
  let (report, lines) = otter_core::log::capture(|| {
    run(
      "\"ab\" | StringToBytes | Log\n[1] | Count | Log",
      &no_defines(),
    )
  });
  assert!(report.succeeded());
  assert_eq!(lines, ["@bytes(6162)", "1"]);
  let d = check_errors("1 | BytesToString");
  assert_eq!(d[0].code, "input-type-mismatch");
}

#[test]
fn a_session_records_the_files_a_revision_read() {
  let mut session = otter_lang::Session::new();
  session.set_files(files(&[("lib.shs", "@const(x 1)"), ("v.txt", "v")]));
  session
    .reload(
      Source::new("main.shs", "@include(\"lib.shs\")\n@read(\"v.txt\") | Log"),
      &catalog(),
      &no_defines(),
    )
    .unwrap_or_else(|(_, d)| panic!("{d:?}"));
  assert_eq!(session.files_read(), ["lib.shs", "v.txt"]);
  // A rejected revision still says what it read.
  assert!(
    session
      .reload(
        Source::new("main.shs", "@include(\"lib.shs\")\nNope"),
        &catalog(),
        &no_defines(),
      )
      .is_err()
  );
  assert_eq!(session.files_read(), ["lib.shs"]);
}

#[cfg(not(any(target_arch = "wasm32", target_os = "espidf")))]
#[test]
fn file_watcher_reloads_when_an_included_file_changes() {
  use otter_lang::{FileWatcher, WatchControl, WatchEvent};
  use std::cell::Cell;
  use std::time::{Duration, Instant};
  let dir = std::env::temp_dir().join(format!("shards-watch-include-{}", std::process::id()));
  std::fs::create_dir_all(&dir).unwrap();
  struct Remove(std::path::PathBuf);
  impl Drop for Remove {
    fn drop(&mut self) {
      let _ = std::fs::remove_dir_all(&self.0);
    }
  }
  let _remove = Remove(dir.clone());
  let (main, lib) = (dir.join("main.shs"), dir.join("lib.shs"));
  let lib_source =
    |value| format!("@fn(Step input: None output: Int params: {{}} {{{value} Log}})");
  std::fs::write(&lib, lib_source(10)).unwrap();
  std::fs::write(
    &main,
    "@include(\"lib.shs\")\n@wire(main {Step} looped: true)\n@mesh(m) @schedule(m main) @run(m fps: 0.1)",
  )
  .unwrap();
  let command = Cell::new(WatchControl::Continue);
  let mut revisions = 0;
  let start = Instant::now();
  let mut session = otter_lang::Session::new();
  let (_, lines) = otter_core::log::capture(|| {
    FileWatcher::new(&main).run(
      &mut session,
      &catalog(),
      &no_defines(),
      || {
        assert!(
          start.elapsed() < Duration::from_secs(5),
          "watcher did not exit"
        );
        command.replace(WatchControl::Continue)
      },
      |event| match event {
        WatchEvent::Reloaded { .. } => {
          revisions += 1;
          match revisions {
            // Only the included file changes.
            1 => std::fs::write(&lib, lib_source(20)).unwrap(),
            2 => command.set(WatchControl::Stop),
            _ => panic!("unexpected reload"),
          }
        }
        WatchEvent::Rejected { diagnostics, .. } => panic!("{diagnostics:?}"),
        WatchEvent::ReadError(error) => panic!("{error}"),
        _ => {}
      },
    )
  });
  assert_eq!(revisions, 2);
  assert_eq!(lines, ["10", "20"]);
}

/// The watcher's view of the files a program reads (review
/// 2026-10-10-e982fe9-codex-3bec98): exact bytes, files a load looked for
/// and did not find, and files a load first named.
#[cfg(not(any(target_arch = "wasm32", target_os = "espidf")))]
mod watched_files {
  use otter_core::Catalog;
  use otter_lang::{FileWatcher, Session, WatchEvent};
  use std::{collections::HashMap, path::PathBuf, time::Duration};

  struct Fixture {
    dir: PathBuf,
    watcher: FileWatcher,
    session: Session,
    catalog: Catalog,
  }
  impl Fixture {
    fn new(name: &str, script: &str) -> Self {
      let dir = std::env::temp_dir().join(format!("shards-watched-{}-{name}", std::process::id()));
      std::fs::create_dir_all(&dir).unwrap();
      let main = dir.join("main.shs");
      std::fs::write(&main, script).unwrap();
      Self {
        dir,
        watcher: FileWatcher::new(main),
        session: Session::new(),
        catalog: Catalog::new(&[otter_core::shards::CATALOG]).unwrap(),
      }
    }
    fn poll(&mut self, restart: bool) -> (usize, usize) {
      let (mut loaded, mut rejected) = (0, 0);
      self.watcher.poll(
        &mut self.session,
        &self.catalog,
        &HashMap::new(),
        restart,
        |event| match event {
          WatchEvent::Reloaded { .. } => loaded += 1,
          WatchEvent::Rejected { .. } => rejected += 1,
          WatchEvent::ReadError(e) => panic!("{e}"),
          _ => {}
        },
      );
      (loaded, rejected)
    }
    fn settle(&mut self) -> (usize, usize) {
      let mut total = (0, 0);
      for _ in 0..4 {
        std::thread::sleep(Duration::from_millis(110));
        let got = self.poll(false);
        total.0 += got.0;
        total.1 += got.1;
      }
      total
    }
  }
  impl Drop for Fixture {
    fn drop(&mut self) {
      let _ = std::fs::remove_dir_all(&self.dir);
    }
  }

  #[test]
  fn binary_edit_is_a_new_revision() {
    let mut f = Fixture::new("binary", "@read(\"data.bin\" bytes: true) | ToHex");
    std::fs::write(f.dir.join("data.bin"), [0xff]).unwrap();
    assert_eq!(f.poll(true), (1, 0));
    f.settle(); // Establish the dependency list in the watcher's observation.
    std::fs::write(f.dir.join("data.bin"), [0xfe]).unwrap();
    assert_eq!(
      f.settle(),
      (1, 0),
      "different invalid UTF-8 bytes must reload"
    );
  }

  #[test]
  fn creating_a_missing_include_retries_the_revision() {
    let mut f = Fixture::new("missing", "@include(\"lib.shs\")\n1");
    assert_eq!(f.poll(true), (0, 1));
    f.settle();
    std::fs::write(f.dir.join("lib.shs"), "").unwrap();
    assert_eq!(
      f.settle(),
      (1, 0),
      "creating the missing file must retry the failed load"
    );
  }

  #[test]
  fn unchanged_dependencies_do_not_reload_twice() {
    let mut f = Fixture::new(
      "unchanged",
      "@include(\"lib.shs\")\n1 | Log | Math.Divide(0)",
    );
    std::fs::write(f.dir.join("lib.shs"), "").unwrap();
    let (revisions, logs) = otter_core::log::capture(|| f.settle());
    eprintln!("revisions={revisions:?}, logs={logs:?}");
    assert_eq!(
      logs,
      ["1"],
      "an unchanged failing entry must not repeat its effects"
    );
    assert_eq!(
      revisions,
      (1, 0),
      "one initial load; no source file changed"
    );
  }

  #[test]
  fn a_file_created_earlier_in_the_search_order_is_a_change() {
    // `lib.shs` is found through an include path; creating one next to the
    // script, which is searched first, changes what loads.
    let mut f = Fixture::new("search", "@include(\"lib.shs\")\n@v | Log");
    let include = f.dir.join("include");
    std::fs::create_dir_all(&include).unwrap();
    std::fs::write(include.join("lib.shs"), "@const(v 1)").unwrap();
    f.session.set_files(otter_lang::FsFiles {
      include_paths: vec![include],
    });
    let (first, lines) = otter_core::log::capture(|| {
      let first = f.poll(true);
      f.session.tick();
      first
    });
    assert_eq!((first, lines), ((1, 0), vec!["1".to_string()]));
    f.settle();
    std::fs::write(f.dir.join("lib.shs"), "@const(v 2)").unwrap();
    let (revisions, lines) = otter_core::log::capture(|| {
      let revisions = f.settle();
      f.session.tick();
      revisions
    });
    assert_eq!(revisions, (1, 0));
    assert!(lines.contains(&"2".to_string()), "{lines:?}");
  }
}

/// Host objects and services (docs/host-requirements.md H1, H3): a fake
/// session opened through a service, used and released by scripts.
mod host_session {
  use std::sync::Arc;
  use std::sync::Mutex;
  use std::sync::atomic::{AtomicUsize, Ordering};

  use otter_core::describe::{
    Forms, InputDesc, OutputDesc, ParamDecl, Params, Requirement, ShardDesc, Targets, TypeName,
  };
  use otter_core::instance::{InstanceCtx, LeafCtx};
  use otter_core::object::{Object, ObjectType};
  use otter_core::shard::Flow;
  use otter_core::shards::leaf::{LeafShard, leaf_type};
  use otter_core::{Args, ComposeCtx, Composed, Error, Result, ShardType, Type, Var};

  pub static SESSION: ObjectType = ObjectType::new("Test.Session");

  /// The service: what the host owns. Counts opened and released sessions
  /// and records what they were sent.
  #[derive(Default)]
  pub struct Sessions {
    pub opened: AtomicUsize,
    pub released: Arc<AtomicUsize>,
    pub sent: Mutex<Vec<String>>,
  }

  /// The object scripts hold.
  pub struct Session {
    number: usize,
    released: Arc<AtomicUsize>,
  }

  impl Object for Session {
    fn object_type(&self) -> &'static ObjectType {
      &SESSION
    }
  }

  impl Drop for Session {
    fn drop(&mut self) {
      self.released.fetch_add(1, Ordering::SeqCst);
    }
  }

  const fn effects() -> otter_core::signature::Effects {
    otter_core::signature::Effects {
      io: true,
      ..otter_core::signature::Effects::NONE
    }
  }

  pub struct Open;

  impl LeafShard for Open {
    type Compiled = ();
    type State = ();
    const DESC: ShardDesc = ShardDesc {
      params: Params::Declared(&[]),
      input: InputDesc::Ignored,
      output: OutputDesc::Fixed(TypeName::Object(&SESSION)),
      targets: Targets::All,
      effects: effects(),
      lifetime: otter_core::signature::Lifetime::Stateless,
      ..ShardDesc::undocumented("Test.Open", 1)
    };

    fn compose(_: &Args, _: &mut ComposeCtx<'_>) -> Result<Composed<()>> {
      Ok(Composed {
        compiled: (),
        output: SESSION.to_type(),
      })
    }

    fn instantiate(_: &(), _: &mut InstanceCtx) -> Result<()> {
      Ok(())
    }

    fn activate(_: &(), _: &mut (), ctx: &mut impl LeafCtx, _: &Var) -> Result<Flow> {
      let Some(sessions) = ctx.services().get::<Sessions>() else {
        return Err(Error::Activation("Test.Open: no Sessions service".into()));
      };
      let number = sessions.opened.fetch_add(1, Ordering::SeqCst) + 1;
      Ok(Flow::Next(Var::object(Session {
        number,
        released: sessions.released.clone(),
      })))
    }
  }

  static SEND_PARAMS: &[ParamDecl] = &[ParamDecl::new(
    "text",
    "",
    Forms::LITERAL,
    &[TypeName::String],
    Requirement::Required,
  )];

  pub struct Send;

  impl LeafShard for Send {
    type Compiled = Arc<str>;
    type State = ();
    const DESC: ShardDesc = ShardDesc {
      params: Params::Declared(SEND_PARAMS),
      input: InputDesc::Types(&[TypeName::Object(&SESSION)]),
      output: OutputDesc::Passthrough,
      targets: Targets::All,
      effects: effects(),
      lifetime: otter_core::signature::Lifetime::Stateless,
      ..ShardDesc::undocumented("Test.Send", 1)
    };

    fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Arc<str>>> {
      Ok(Composed {
        compiled: Arc::from(args.string("text").expect("decoded")),
        output: ctx.input(),
      })
    }

    fn instantiate(_: &Arc<str>, _: &mut InstanceCtx) -> Result<()> {
      Ok(())
    }

    fn activate(text: &Arc<str>, _: &mut (), ctx: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
      let session = input.as_object::<Session>().expect("typed by compose");
      let sessions = ctx.services().get::<Sessions>().expect("opened through it");
      sessions
        .sent
        .lock()
        .unwrap()
        .push(format!("{}: {text}", session.number));
      Ok(Flow::Next(input.clone()))
    }
  }

  pub static OPEN: ShardType = leaf_type::<Open>();
  pub static SEND: ShardType = leaf_type::<Send>();

  pub fn catalog() -> otter_core::Catalog {
    otter_core::Catalog::new(&[otter_core::shards::CATALOG, &[&OPEN, &SEND]])
      .and_then(|c| c.with_objects(&[&[&SESSION]]))
      .unwrap()
  }

  /// A session over `text` with a fresh `Sessions` service, ticked until
  /// it finishes (at most `ticks`); the service and what finished.
  pub fn run(
    text: &str,
    ticks: usize,
  ) -> (
    Arc<Sessions>,
    Vec<otter_lang::Finished>,
    otter_lang::Session,
  ) {
    let sessions = Arc::new(Sessions::default());
    let mut services = otter_core::service::Services::default();
    services.insert_arc(sessions.clone());
    let mut session = otter_lang::Session::new();
    session.set_services(services);
    session
      .reload(
        otter_lang::Source::new("t.shs", text),
        &catalog(),
        &super::no_defines(),
      )
      .unwrap_or_else(|(_, d)| panic!("load failed: {d:?}"));
    let mut finished = Vec::new();
    for _ in 0..ticks {
      finished.extend(session.tick());
      if session.running() == 0 {
        break;
      }
    }
    (sessions, finished, session)
  }

  pub fn counts(s: &Sessions) -> (usize, usize) {
    (
      s.opened.load(Ordering::SeqCst),
      s.released.load(Ordering::SeqCst),
    )
  }

  #[allow(dead_code)]
  pub fn object_type() -> Type {
    SESSION.to_type()
  }
}

#[test]
fn a_host_object_is_opened_used_and_released_by_a_script() {
  let text = r#"@fn(Talk input: Test.Session output: Test.Session params: {} { Test.Send("hi") | Test.Send("bye") })
Test.Open | Var(s)
s | Talk
Test.Open | Talk
none"#;
  let (sessions, finished, _session) = host_session::run(text, 10);
  assert!(
    matches!(&finished[..], [f] if matches!(f.outcome, Outcome::Completed(_))),
    "{finished:?}"
  );
  assert_eq!(
    host_session::counts(&sessions),
    (2, 2),
    "opened and released"
  );
  assert_eq!(
    *sessions.sent.lock().unwrap(),
    ["1: hi", "1: bye", "2: hi", "2: bye"]
  );
}

#[test]
fn a_host_object_is_released_on_every_exit() {
  let cases = [
    // A failure after opening.
    ("error", "Test.Open | Var(s)\n\"x\" | ParseInt"),
    // Stop.
    ("stop", "Test.Open | Var(s)\nStop"),
    // Return out of a function holding one in a local.
    (
      "return",
      "@fn(F input: None output: Int params: {} { Test.Open | Var(s) 1 | Return })\nF",
    ),
    // A looped wire: each iteration's local goes at the next iteration.
    (
      "loop",
      "@wire(main { Test.Open | Var(s) Pause s | Test.Send(\"x\") } looped: true)\n@mesh(m) @schedule(m main) @run(m)",
    ),
  ];
  for (exit, text) in cases {
    // Odd: the loop is paused inside its third iteration, holding a session.
    let (sessions, _, mut session) = host_session::run(text, 5);
    let (opened, released) = host_session::counts(&sessions);
    assert!(opened >= 1, "{exit}");
    if exit == "loop" {
      // Every iteration but the one in flight has released its session.
      assert_eq!((opened, released), (3, 2), "{exit}");
      // Cancelling the running wire releases the last one.
      session.stop();
      assert_eq!(
        host_session::counts(&sessions),
        (opened, opened),
        "{exit}: cancelled"
      );
    } else {
      assert_eq!((opened, released), (1, 1), "{exit}");
    }
  }
}

#[test]
fn a_service_is_looked_up_at_activation() {
  // Without the service, the shard fails at run time with its own message;
  // compose does not need it.
  let program = Program::load(
    Source::new("t.shs", "Test.Open | Var(s)\n1"),
    &host_session::catalog(),
    &no_defines(),
  )
  .unwrap_or_else(|(_, d)| panic!("{d:?}"));
  let report = program.run().unwrap();
  match report.outcomes.iter().find(|(w, _)| w == "root") {
    Some((_, Some(Outcome::Failed(e)))) => {
      assert!(e.to_string().contains("no Sessions service"), "{e}")
    }
    other => panic!("{other:?}"),
  }
}

#[test]
fn object_types_compose_by_name() {
  let catalog = host_session::catalog();
  let errors = |text: &str| {
    otter_lang::check_with(
      Source::new("t.shs", text),
      &catalog,
      &no_defines(),
      &otter_lang::MemoryFiles::default(),
    )
    .diagnostics
  };
  // A session where an Int is expected, and an Int where a session is.
  let d = errors("@fn(F input: Int output: Int params: {} { input })\nTest.Open | F");
  assert_eq!(d.len(), 1, "{d:?}");
  assert!(d[0].message.contains("Test.Session"), "{}", d[0].message);
  let d = errors("1 | Test.Send(\"x\")");
  assert_eq!(d.len(), 1, "{d:?}");
  assert_eq!(d[0].code, "input-type-mismatch");
  // An unknown type name suggests the object type.
  let d = errors("@fn(F input: Test.Sesion output: None params: {} { none })\nnone");
  assert_eq!(d[0].code, "unknown-type", "{d:?}");
  assert!(
    d[0].did_you_mean.iter().any(|s| s == "Test.Session"),
    "{d:?}"
  );
  // Without the type in the catalog, the name is unknown.
  let d = load_errors("@fn(F input: Test.Session output: None params: {} { none })\nnone");
  assert_eq!(d[0].code, "unknown-type");
}

#[test]
fn a_catalog_refuses_duplicate_and_unqualified_object_types() {
  static PLAIN: otter_core::object::ObjectType = otter_core::object::ObjectType::new("Session");
  static LOWER: otter_core::object::ObjectType =
    otter_core::object::ObjectType::new("Test.session");
  let base = || Catalog::new(&[otter_core::shards::CATALOG]).unwrap();
  assert!(
    base()
      .with_objects(&[&[&host_session::SESSION], &[&host_session::SESSION]])
      .is_err()
  );
  assert!(base().with_objects(&[&[&PLAIN]]).is_err());
  assert!(base().with_objects(&[&[&LOWER]]).is_err());
  // Names a script could not write (review 2026-10-11-b263ee9-codex-a6c596#F4).
  static SPACE: otter_core::object::ObjectType =
    otter_core::object::ObjectType::new("Test.Bad Name");
  static PUNCT: otter_core::object::ObjectType = otter_core::object::ObjectType::new("Test.Bad!");
  static EMPTY: otter_core::object::ObjectType = otter_core::object::ObjectType::new("Test.");
  static FINE: otter_core::object::ObjectType =
    otter_core::object::ObjectType::new("Test.Good_Name-2");
  for bad in [&SPACE, &PUNCT, &EMPTY] {
    assert!(base().with_objects(&[&[bad]]).is_err(), "{}", bad.name);
  }
  assert!(base().with_objects(&[&[&FINE]]).is_ok());
}

/// Guards in a control shard's `State` (docs/host-requirements.md H4): the
/// engine resets the state on every exit, so a guard's `Drop` runs on
/// success, error, `Stop`, `Return` passing through, and cancellation.
mod host_guard {
  use std::sync::Arc;
  use std::sync::atomic::{AtomicUsize, Ordering};

  use otter_core::describe::{
    Forms, InputDesc, OutputDesc, ParamDecl, Params, Requirement, ShardDesc, Targets,
  };
  use otter_core::flow::CompiledFlow;
  use otter_core::instance::LeafCtx;
  use otter_core::{ActivationCtx, Args, ComposeCtx, Composed, ControlShard, ControlStep};
  use otter_core::{Result, ShardType, Var};

  /// Taken and released guards, through the mesh's services.
  #[derive(Default)]
  pub struct Ledger {
    pub taken: AtomicUsize,
    pub released: AtomicUsize,
  }

  /// Releases on drop, as a transaction rolls back or a dialog dismisses.
  pub struct Guard(Arc<Ledger>);

  impl Drop for Guard {
    fn drop(&mut self) {
      self.0.released.fetch_add(1, Ordering::SeqCst);
    }
  }

  static PARAMS: &[ParamDecl] = &[ParamDecl::new(
    "action",
    "",
    Forms::FLOW,
    &[],
    Requirement::Required,
  )];

  pub static GUARDED: ShardType = ShardType::new(ShardDesc {
    params: Params::Declared(PARAMS),
    input: InputDesc::Any,
    output: OutputDesc::Passthrough,
    targets: Targets::All,
    effects: otter_core::signature::Effects::NONE,
    lifetime: otter_core::signature::Lifetime::Stateless,
    ..ShardDesc::undocumented("Test.Guarded", 1)
  })
  .controlled_by::<Guarded>();

  pub struct Guarded;

  impl ControlShard for Guarded {
    type Compiled = Vec<CompiledFlow>;
    type State = Option<Guard>;
    const NAME: &'static str = "Test.Guarded";

    fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Vec<CompiledFlow>>> {
      let input = ctx.input();
      let action = ctx.compose_flow(args.flow("action").expect("decoded"), input)?;
      Ok(Composed {
        compiled: vec![action],
        output: input,
      })
    }

    fn flows(compiled: &Vec<CompiledFlow>) -> &[CompiledFlow] {
      compiled
    }

    fn resume(
      _: &Vec<CompiledFlow>,
      guard: &mut Option<Guard>,
      ctx: &mut ActivationCtx<'_>,
      input: &Var,
      completion: Option<Result<Var>>,
    ) -> Result<ControlStep> {
      match completion {
        None => {
          let ledger = ctx.services().get_arc::<Ledger>().expect("a ledger");
          ledger.taken.fetch_add(1, Ordering::SeqCst);
          *guard = Some(Guard(ledger));
          Ok(ControlStep::Enter(0, input.clone()))
        }
        Some(Ok(_)) => Ok(ControlStep::Complete(input.clone())),
        Some(Err(err)) => Err(err),
      }
    }
  }

  pub fn run(text: &str, ticks: usize) -> (Arc<Ledger>, otter_lang::Session) {
    let ledger = Arc::new(Ledger::default());
    let mut services = otter_core::service::Services::default();
    services.insert_arc(ledger.clone());
    let catalog = otter_core::Catalog::new(&[otter_core::shards::CATALOG, &[&GUARDED]]).unwrap();
    let mut session = otter_lang::Session::new();
    session.set_services(services);
    session
      .reload(
        otter_lang::Source::new("t.shs", text),
        &catalog,
        &super::no_defines(),
      )
      .unwrap_or_else(|(_, d)| panic!("load failed: {d:?}"));
    for _ in 0..ticks {
      session.tick();
      if session.running() == 0 {
        break;
      }
    }
    (ledger, session)
  }

  pub fn counts(l: &Ledger) -> (usize, usize) {
    (
      l.taken.load(Ordering::SeqCst),
      l.released.load(Ordering::SeqCst),
    )
  }
}

#[test]
fn a_guard_in_control_state_is_released_on_every_exit() {
  let looped = |body: &str| {
    format!("@wire(main {{ {body} }} looped: true)\n@mesh(m) @schedule(m main) @run(m)")
  };
  let cases = [
    (
      "success",
      "1 | Test.Guarded({ Math.Add(1) })".to_string(),
      3,
      (1, 1),
    ),
    (
      "error",
      "1 | Test.Guarded({ \"x\" | ParseInt })".to_string(),
      3,
      (1, 1),
    ),
    ("stop", "1 | Test.Guarded({ Stop })".to_string(), 3, (1, 1)),
    (
      "return",
      "@fn(F input: Int output: Int params: {} { Test.Guarded({ Return }) Math.Add(1) })\n1 | F"
        .to_string(),
      3,
      (1, 1),
    ),
  ];
  for (exit, text, ticks, expected) in cases {
    let (ledger, _session) = host_guard::run(&text, ticks);
    assert_eq!(host_guard::counts(&ledger), expected, "{exit}");
  }
  // Suspended inside the flow, the guard is held; cancelling releases it.
  let (ledger, mut session) = host_guard::run(&looped("1 | Test.Guarded({ Pause })"), 1);
  assert_eq!(host_guard::counts(&ledger), (1, 0), "held while suspended");
  session.stop();
  assert_eq!(
    host_guard::counts(&ledger),
    (1, 1),
    "released on cancellation"
  );
}

/// The value a top-level program completes with.
fn root_value(text: &str) -> Var {
  completed(&run(text, &no_defines()), "root")
}

/// The activation error a top-level program fails with.
fn root_failure(text: &str) -> String {
  match run(text, &no_defines()).outcomes.into_iter().next() {
    Some((_, Some(Outcome::Failed(e)))) => e.to_string(),
    other => panic!("expected a failure, got {other:?}"),
  }
}

#[test]
fn string_shards_split_join_replace_and_test() {
  let v = root_value(
    r#"
"a,,b" | String.Split(",") = parts
parts | String.Join("+") = joined
"a-b-a" | String.Replace(find: "a" replacement: "xy") = replaced
"x=1;y=2" | String.Split(";" keep-separator: true) = kept
"" | String.Split(",") = empty
" hi\n" | String.Trim = trimmed
"Straße" | String.ToUpper = upper
"ÀB" | String.ToLower = lower
"hello" | String.Contains("ell") = c
"hello" | String.Starts("he") = s
"hello" | String.Ends("xo") = e
"," = sep
["p" "q"] | String.Join(sep) = by-var
[parts joined replaced kept empty trimmed upper lower c s e by-var]
"#,
  );
  assert_eq!(
    v.to_string(),
    r#"[["a" "" "b"] "a++b" "xy-b-xy" ["x=1;" "y=2"] [""] "hi" "STRASSE" "àb" true true false "p,q"]"#
  );
  assert!(root_failure(r#"["a" 1] | String.Join"#).contains("element 1 is Int"));
  let d = load_or_compose_errors(r#""a" | String.Split("")"#);
  assert_eq!(d[0].code, "invalid-argument-value");
}

/// Load or compose errors of a source that must not compile.
fn load_or_compose_errors(text: &str) -> Vec<Diagnostic> {
  let report = check(text);
  assert!(!report.ok(), "expected errors");
  report.diagnostics
}

#[test]
fn slice_takes_characters_elements_and_bytes() {
  let v = root_value(
    r#"
"héllo" | Slice(from: 1 to: 3) = a
"héllo" | Slice(from: -2) = b
[1 2 3 4] | Slice(to: -1) = c
[1 2 3] | Slice(from: 5) = d
"abc" | StringToBytes | Slice(from: 1) | BytesToString = e
1 = from
"abc" | Slice(from: from) = f
[a b c d e f]
"#,
  );
  assert_eq!(v.to_string(), r#"["él" "lo" [1 2 3] [] "bc" "bc"]"#);
}

#[test]
fn encodings_round_trip() {
  let v = root_value(
    r#"
"hello" | ToBase64 = b64
b64 | FromBase64 | BytesToString = back
300 | ToLEB128 | ToHex = leb
-65 | ToLEB128(signed: true) | FromLEB128(signed: true) = signed
[(1 | ToLEB128) ("ab" | StringToBytes)] | Bytes.Join | ToHex = joined
"a b/é" | String.EncodeURI = uri
uri | String.DecodeURI = decoded
[b64 back leb signed joined uri decoded]
"#,
  );
  assert_eq!(
    v.to_string(),
    r#"["aGVsbG8=" "hello" "ac02" -65 "016162" "a%20b%2F%C3%A9" "a b/é"]"#
  );
  assert!(root_failure("-1 | ToLEB128").contains("negative"));
  assert!(root_failure(r#""@@" | FromBase64"#).contains("FromBase64"));
}

#[test]
fn json_reads_and_writes_with_ints_and_floats_apart() {
  let v = root_value(
    r#"
"{\"b\": [1, 2.5, null, true], \"a\": \"x\"}" | FromJson = parsed
parsed | ExpectTable | Take("b") | ExpectSeq | Take(0) = first
first | IsInt = is-int
parsed | ToJson = text
{n: 2.0 s: [1 "q"]} | ToJson(indent: 1) = pretty
[is-int text pretty]
"#,
  );
  assert_eq!(
    v.to_string(),
    r#"[true "{\"a\":\"x\",\"b\":[1,2.5,null,true]}" "{\n \"n\": 2.0,\n \"s\": [\n  1,\n  \"q\"\n ]\n}"]"#
  );
  assert!(root_failure(r#""{" | FromJson"#).contains("invalid JSON at byte 1"));
  assert!(root_failure(r#""x" | StringToBytes | ToJson"#).contains("no JSON form"));
}

#[test]
fn type_tests_fail_sort_and_scalar_math() {
  let v = root_value(
    r#"
"[1, \"a\", [], {}]" | FromJson | ExpectSeq = xs
[(xs | Take(0) | IsInt) (xs | Take(1) | IsString) (xs | Take(2) | IsSeq) (xs | Take(3) | IsTable) (xs | Take(0) | IsFloat)] = tests
[[3 1] [1 2] [3 0]] | Sort = pairs
[2.5 1 -3] | Sort(desc: true) = numbers
[(-7 | Mod(3)) (7.5 | Mod(2)) (3 | Max(9)) (3 | Min(2.5))] = math
// Vectors mix with numbers and same-size vectors per component, as Add does.
[4 2 3] | ToFloat3 = f3
[([5.5 -3] | ToFloat2 | Mod(2)) ([1 5 3] | ToFloat3 | Max(f3)) ([1 5] | ToFloat2 | Min(2))] = vectors
// Constant work, so they evaluate at compose time.
#( 17 | Mod(5) | Max(1) | Min(10) ) = folded
[tests pairs numbers math vectors folded]
"#,
  );
  assert_eq!(
    v.to_string(),
    "[[true true true true false] [[1 2] [3 0] [3 1]] [2.5 1 -3] [-1 1.5 9 2.5] [@f2(1.5 -1.0) @f3(4.0 5.0 3.0) @f2(1.0 2.0)] 2]"
  );
  let d = load_or_compose_errors(r#""a" | Max(1)"#);
  assert_eq!(d[0].code, "input-type-mismatch");
  let d = load_or_compose_errors("[1 2 3] | ToFloat3 = v\n[1 2] | ToFloat2 | Min(v)");
  assert!(
    d[0]
      .message
      .starts_with("Math.Min cannot take Float2 with Float3"),
    "{}",
    d[0].message
  );
  assert_eq!(
    root_failure(r#"Maybe({"inner" | Fail} {"caught"} silent: true) | Log  "out" | Fail"#),
    "activation error: out"
  );
  assert!(root_failure(r#"[1 "a"] | Sort"#).contains("cannot order"));
  assert_eq!(
    root_failure("1 | Mod(0)"),
    "activation error: Math.Mod: division by zero"
  );
}

/// Review findings on the text shards: malformed input is an error, never
/// a wrong value or a panic.
#[test]
fn text_shards_reject_malformed_input() {
  // Sort's comparator stays a total order with NaN, so Rust's sort cannot
  // panic on it; the NaN is still an activation error.
  let nans = (0..30)
    .map(|i| match i % 3 {
      0 => "nan".to_string(),
      _ => format!("{}.5", (i * 7) % 30),
    })
    .collect::<Vec<_>>()
    .join(" ");
  let message = root_failure(&format!("0.0 | Div(0.0) = nan\n[{nans}] | Sort"));
  assert!(message.contains("NaN"), "{message}");
  // The tenth signed byte may only extend the sign: 2^63 does not fit.
  assert!(
    root_failure(r#""gICAgICAgICAAQ==" | FromBase64 | FromLEB128(signed: true)"#)
      .contains("does not fit")
  );
  let v = root_value(
    r#"
"gICAgICAgICAfw==" | FromBase64 | FromLEB128(signed: true) = min
"////////////fw==" | FromBase64 | FromLEB128(signed: true) = minus-one
-9223372036854775807 | Sub(1) | Mod(-1) = rem
"20260115" | Date.Parse(format: "%Y%m%d") | Date.Format(format: "%Y-%m-%d") = compact
[0.1 0.2] | ToFloat2 | ToJson = floats
[min minus-one rem compact floats]
"#,
  );
  assert_eq!(
    v.to_string(),
    r#"[-9223372036854775808 -1 0 "2026-01-15" "[0.1,0.2]"]"#
  );
  for (script, expected) in [
    (r#""\"\\u+041\"" | FromJson"#, "hex digits"),
    (r#""%+F" | String.DecodeURI"#, "invalid escape"),
    (r#""QUJD=" | FromBase64"#, "padding"),
    (r#""QR==" | FromBase64"#, "bits set"),
  ] {
    let message = root_failure(script);
    assert!(message.contains(expected), "{script}: {message}");
  }
}

#[test]
fn dates_parse_and_format_in_utc() {
  let v = root_value(
    r#"
"2026-11-16" | Date.Parse(format: "%Y-%m-%d") = t
t | Add(86400) | Date.Format(format: "%Y-%m-%d") = next
0 | Date.Format = epoch
[t next epoch]
"#,
  );
  assert_eq!(
    v.to_string(),
    r#"[1794787200 "2026-11-17" "1970-01-01 00:00:00"]"#
  );
  assert!(root_failure(r#""2026-02-30" | Date.Parse(format: "%Y-%m-%d")"#).contains("not a date"));
  let d = load_or_compose_errors(r#"0 | Date.Format(format: "%A")"#);
  assert_eq!(d[0].code, "invalid-argument-value");
}

mod tools {
  use super::*;
  use otter_lang::{CallOptions, MemoryFiles, Tool, ToolError, ToolInput};
  use std::sync::Arc;
  use std::sync::atomic::AtomicBool;

  const ECHO: &str = r#"
@fn(Echo input: {input: {} env: {} working-dir: String} output: String params: {} {
  input.input = args
  args.name | If({IsString} {ExpectString} {"nobody"}) = name
  input.env.greeting | If({IsString} {ExpectString} {"Hello"}) = greeting
  args.fail | When({Is(true)} {"asked to fail" | Fail})
  args.wait | When({Is(true)} {"waiting" | Log  Repeat({Pause} forever: true)})
  args.stop | When({Is(true)} {"stopping" | Log  Stop})
  args.spin | When({Is(true)} {"spinning" | Log  0 | Repeat({1} forever: true)})
  f"{greeting}, {name}!" | Log
  f"{greeting}, {name}! (in {input.working-dir})"
})
{definition: {name: "echo" description: "Greets." parameters: {type: "object" properties: {name: {type: "string"}}}} use: "Echo"}
"#;

  fn load(text: &str) -> Result<Tool, ToolError> {
    Tool::load_with(
      Source::new("tool.shs", text),
      &catalog(),
      &no_defines(),
      &MemoryFiles::default(),
    )
  }

  #[test]
  fn a_tool_describes_itself_and_runs_with_its_input() {
    let tool = load(ECHO).unwrap();
    let d = tool.definition();
    assert_eq!(
      (d.name.as_str(), d.description.as_str()),
      ("echo", "Greets.")
    );
    assert_eq!(
      d.parameters,
      r#"{"properties":{"name":{"type":"string"}},"type":"object"}"#
    );
    assert_eq!(tool.entry(), "Echo");
    let input = ToolInput::from_json(r#"{"name": "Ann"}"#)
      .unwrap()
      .with_env(Var::table([("greeting", Var::string("Hi"))]))
      .with_working_dir("/w");
    let out = tool.call(&input).unwrap();
    assert_eq!(out.text, "Hi, Ann! (in /w)");
    assert_eq!(out.log, ["Hi, Ann!"]);
    assert_eq!(
      tool.call(&ToolInput::from_json("").unwrap()).unwrap().text,
      "Hello, nobody! (in )"
    );
  }

  /// Calls are independent and can run on several threads at once.
  #[cfg(not(any(target_arch = "wasm32", target_os = "espidf")))]
  #[test]
  fn tool_calls_run_concurrently_on_threads() {
    let tool = load(ECHO).unwrap();
    let threads: Vec<_> = (0..3)
      .map(|_| {
        let tool = tool.clone();
        std::thread::spawn(move || tool.call(&ToolInput::from_json("").unwrap()).unwrap().text)
      })
      .collect();
    for t in threads {
      assert_eq!(t.join().unwrap(), "Hello, nobody! (in )");
    }
  }

  #[test]
  fn tool_failures_are_reported_not_panicked() {
    let tool = load(ECHO).unwrap();
    match tool.call(&ToolInput::from_json(r#"{"fail": true}"#).unwrap()) {
      Err(ToolError::Failed { message, .. }) => assert_eq!(message, "asked to fail"),
      other => panic!("{other:?}"),
    }
    assert!(matches!(
      ToolInput::from_json("[1]"),
      Err(ToolError::Input(_))
    ));
    let wait = ToolInput::from_json(r#"{"wait": true}"#).unwrap();
    let timeout = CallOptions {
      timeout: Some(std::time::Duration::from_millis(30)),
      ..CallOptions::default()
    };
    // The log of a call that did not finish still comes back.
    match tool.call_with(&wait, &timeout) {
      Err(ToolError::TimedOut { log, .. }) => assert_eq!(log, ["waiting"]),
      other => panic!("{other:?}"),
    }
    let cancel = Arc::new(AtomicBool::new(true));
    let cancelled = CallOptions {
      cancel: Some(cancel),
      ..CallOptions::default()
    };
    match tool.call_with(&wait, &cancelled) {
      Err(ToolError::Cancelled { log }) => assert_eq!(log, ["waiting"]),
      other => panic!("{other:?}"),
    }
    match tool.call(&ToolInput::from_json(r#"{"stop": true}"#).unwrap()) {
      Err(ToolError::Stopped { log }) => assert_eq!(log, ["stopping"]),
      other => panic!("{other:?}"),
    }
  }

  #[cfg(not(any(target_arch = "wasm32", target_os = "espidf")))]
  fn timeout_long() -> CallOptions {
    CallOptions {
      timeout: Some(std::time::Duration::from_secs(60)),
      ..CallOptions::default()
    }
  }

  /// A loop that never suspends runs inside one tick: with a time limit,
  /// a cancellation flag or fuel the call runs metered, and each stops it.
  #[cfg(not(any(target_arch = "wasm32", target_os = "espidf")))]
  #[test]
  fn a_call_that_never_suspends_still_stops() {
    let tool = load(ECHO).unwrap();
    let spin = ToolInput::from_json(r#"{"spin": true}"#).unwrap();
    let started = std::time::Instant::now();
    let timeout = CallOptions {
      timeout: Some(std::time::Duration::from_millis(50)),
      ..CallOptions::default()
    };
    match tool.call_with(&spin, &timeout) {
      Err(ToolError::TimedOut { log, .. }) => assert_eq!(log, ["spinning"]),
      other => panic!("{other:?}"),
    }
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    let cancel = Arc::new(AtomicBool::new(false));
    let setter = {
      let cancel = cancel.clone();
      std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(50));
        cancel.store(true, std::sync::atomic::Ordering::Release);
      })
    };
    let cancelled = CallOptions {
      cancel: Some(cancel),
      ..CallOptions::default()
    };
    match tool.call_with(&spin, &cancelled) {
      Err(ToolError::Cancelled { log }) => assert_eq!(log, ["spinning"]),
      other => panic!("{other:?}"),
    }
    setter.join().unwrap();
    let fuel = CallOptions {
      fuel: Some(100_000),
      ..CallOptions::default()
    };
    match tool.call_with(&spin, &fuel) {
      Err(ToolError::Failed { message, log }) => {
        assert!(message.contains("ran out of fuel"), "{message}");
        assert_eq!(log, ["spinning"]);
      }
      other => panic!("{other:?}"),
    }
    // A budget counts work only: values nested deeper than compose-time
    // evaluation allows, or large shared values pushed, are as without one.
    let deep = load(
      "@fn(Deep input: {input: {} env: {} working-dir: String} output: String params: {} {\n  none | ToAny | Var(v)\n  Repeat({[v] | ToAny | Update(v)} times: 70)\n  [] | Var(big)  Repeat({1 | Push(big)} times: 20000)\n  [] | Var(into)  Repeat({big | Push(into)} times: 200)\n  \"done\"\n})\n{definition: {name: \"deep\"} use: \"Deep\"}",
    )
    .unwrap();
    let started = std::time::Instant::now();
    assert_eq!(
      deep
        .call_with(&ToolInput::from_json("{}").unwrap(), &timeout_long())
        .unwrap()
        .text,
      "done"
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    // Fuel is counted from each call's start, and a budgeted call that
    // finishes in it returns as usual.
    let out = tool
      .call_with(&ToolInput::from_json(r#"{"name": "Ann"}"#).unwrap(), &fuel)
      .unwrap();
    assert_eq!(out.text, "Hello, Ann! (in )");
  }

  /// `Once(every:)` reads the clock; a plain `Once` does not.
  #[test]
  fn once_every_declares_the_clock() {
    let tool = |once: &str| {
      load(&format!(
        "@fn(T stateful: true input: {{input: {{}} env: {{}} working-dir: String}} output: String params: {{}} {{ {once} \"ok\" }})\n{{definition: {{name: \"t\"}} use: \"T\"}}"
      ))
      .unwrap()
    };
    assert!(tool("Once({1} every: 1.5)").effects().time);
    assert!(!tool("Once({1})").effects().time);
  }

  /// The call ends when its function does, not when every wire it spawned
  /// does: a looping child must not keep the call waiting.
  #[test]
  fn a_call_returns_while_its_spawned_wires_still_run() {
    let tool = load(
      "@wire(bg { Pause } looped: true)\n@fn(T input: {input: {} env: {} working-dir: String} output: String params: {} { Spawn(bg) \"done\" })\n{definition: {name: \"t\"} use: \"T\"}",
    )
    .unwrap();
    let options = CallOptions {
      timeout: Some(std::time::Duration::from_secs(5)),
      ..CallOptions::default()
    };
    let out = tool.call_with(&ToolInput::from_json("{}").unwrap(), &options);
    assert_eq!(out.unwrap().text, "done");
  }

  #[test]
  fn scripts_that_are_not_tools_are_rejected_at_load() {
    let compile = load(
      "@fn(F input: Int output: String params: {} { Math.Add(1) })\n{definition: {name: \"f\"} use: \"F\"}",
    );
    match compile {
      Err(ToolError::Compile {
        diagnostics,
        rendered,
      }) => {
        assert!(!diagnostics.is_empty());
        assert!(rendered.contains("tool.shs:1:"), "{rendered}");
      }
      Err(other) => panic!("{other}"),
      Ok(_) => panic!("loaded"),
    }
    let cases = [
      (
        "@fn(F input: Int output: Int params: {} { Math.Add(1) })",
        "no top-level code",
      ),
      ("{definition: {name: \"f\"}}", "`use`"),
      (
        "{definition: {description: \"d\"} use: \"F\"}",
        "definition.name",
      ),
      (
        "\"x\" | Log  {definition: {name: \"f\"} use: \"F\"}",
        "no I/O",
      ),
    ];
    for (text, expected) in cases {
      match load(text) {
        Err(ToolError::Definition(m)) => assert!(m.contains(expected), "{m}"),
        Err(other) => panic!("{text}: {other}"),
        Ok(_) => panic!("{text}: loaded"),
      }
    }
    // The function must accept the call table.
    let wrong = load(
      "@fn(F input: Int output: Int params: {} { Math.Add(1) })\n{definition: {name: \"f\"} use: \"F\"}",
    );
    assert!(
      matches!(wrong, Err(ToolError::Compile { .. })),
      "{:?}",
      wrong.err()
    );
    let missing = load("{definition: {name: \"f\"} use: \"Nope\"}");
    assert!(
      matches!(missing, Err(ToolError::Compile { .. })),
      "{:?}",
      missing.err()
    );
  }

  #[test]
  fn a_tool_can_include_its_library() {
    let mut files = MemoryFiles::default();
    files.insert(
      "lib.shs",
      "@fn(Twice input: String output: String params: {} { = s  f\"{s}{s}\" })",
    );
    let tool = Tool::load_with(
      Source::new("tool.shs", "@include(\"lib.shs\")\n@fn(T input: {input: {} env: {} working-dir: String} output: String params: {} { \"ab\" | Twice })\n{definition: {name: \"t\"} use: \"T\"}"),
      &catalog(),
      &no_defines(),
      &files,
    )
    .unwrap();
    assert_eq!(tool.files(), ["lib.shs"]);
    assert_eq!(
      tool
        .call(&ToolInput::from_json("{}").unwrap())
        .unwrap()
        .text,
      "abab"
    );
  }
}

#[test]
fn a_tool_reports_the_effects_of_its_calls() {
  use otter_lang::{MemoryFiles, Tool};
  let load = |body: &str| {
    Tool::load_with(
      Source::new("t.shs", format!("@fn(T input: {{input: {{}} env: {{}} working-dir: String}} output: String params: {{}} {{ {body} }})\n{{definition: {{name: \"t\"}} use: \"T\"}}")),
      &catalog(),
      &no_defines(),
      &MemoryFiles::default(),
    )
    .unwrap()
  };
  assert_eq!(
    load("\"x\"").effects(),
    otter_core::signature::Effects::NONE
  );
  let waits = load("Pause \"x\"").effects();
  assert!(waits.suspends && waits.time, "{waits:?}");
  assert!(load("\"x\" | Log").effects().io);
}

// --- the rest of the core catalog (docs/host-requirements.md §3) ---

#[test]
fn code_points_hex_and_byte_ints() {
  let v = root_value(
    r#"
"hé" | String.CodePoints = cps
cps | String.FromCodePoints = back
"c328" | HexToBytes | String.CleanUTF8 = cleaned
"ok" | String.CleanUTF8 = text
"0xABC" | FromHex | BytesToInts = ints
[1 2 255] | IntsToBytes | ToHex = hex
[cps back cleaned text ints hex]
"#,
  );
  assert_eq!(
    v.to_string(),
    r#"[[104 233] "hé" "�(" "ok" [10 188] "0102ff"]"#
  );
  for (script, expected) in [
    ("[55296] | String.FromCodePoints", "element 0 (55296)"),
    ("[1 256] | IntsToBytes", "element 1 (256)"),
    (r#""0x1g" | HexToBytes"#, "'g' at 1"),
  ] {
    let message = root_failure(script);
    assert!(message.contains(expected), "{script}: {message}");
  }
  // `[Any]` is narrowed first.
  let d = load_or_compose_errors(r#""[1]" | FromJson | ExpectSeq | IntsToBytes"#);
  assert_eq!(d[0].code, "input-type-mismatch");
}

#[test]
fn sequence_variables_change_in_place() {
  let v = root_value(
    r#"
[1 2] | Var(xs)
3 | AppendTo(xs)
0 | PrependTo(xs)
[4 5] | Extend(xs) = extended
PopFront(xs) = first
DropFront(xs)
"b" | Var(s)
"c" | AppendTo(s)
"a" | PrependTo(s)
"01" | HexToBytes | Var(b)
"02" | HexToBytes | AppendTo(b)
Remove(xs {IsMore(3)}) = kept
[] | ExpectIntSeq | Var(empty)
DropFront(empty)
{a: 1} | ToAnyTable | Var(t)
[xs extended first s (b | ToHex) kept empty] = before
Clear(xs)
Clear(t)
[before xs t]
"#,
  );
  assert_eq!(
    v.to_string(),
    r#"[[[2 3] [4 5] 0 "abc" "0102" [2 3] []] [] {}]"#
  );
  assert_eq!(
    root_failure("[] | ExpectIntSeq | Var(xs)\nPopFront(xs)"),
    "activation error: PopFront: the sequence is empty"
  );
  // A failing predicate leaves the variable as it was.
  let v = root_value(
    "[1 2 3] | Var(xs)\nMaybe({Remove(xs {Assert.IsNot(2) | IsMore(1)})} silent: true)\nxs",
  );
  assert_eq!(v.to_string(), "[1 2 3]");
  for (script, code) in [
    ("[1] = xs\n2 | AppendTo(xs)", "immutable-binding"),
    (
      "[1] | Var(xs)\n\"a\" | AppendTo(xs)",
      "variable-type-mismatch",
    ),
    ("1 | Var(n)\n2 | AppendTo(n)", "variable-type-mismatch"),
    (
      "[1] | Var(xs)\n[\"a\"] | Extend(xs)",
      "variable-type-mismatch",
    ),
    ("{a: 1} | Var(t)\nClear(t)", "variable-type-mismatch"),
    ("\"a\" | Var(s)\nPopFront(s)", "variable-type-mismatch"),
    ("[1] | Var(xs)\nRemove(xs {Add(1)})", "predicate-not-bool"),
    ("PopFront(nope)", "unknown-variable"),
  ] {
    let d = load_or_compose_errors(script);
    assert_eq!(d[0].code, code, "{script}: {}", d[0].message);
  }
}

#[test]
fn reverse_and_index_of() {
  let v = root_value(
    r#"
[1 2 3] | Reverse = r
"héllo" | Reverse = s
"0102" | HexToBytes | Reverse | ToHex = b
[0 1 1 0 1 1] | IndexOf([1 1]) = run
[0 1 1 0 1 1] | IndexOf([1 1] all: true) = runs
[[1] [2]] | IndexOf([2]) = element
[1 2.5] | IndexOf(2.5) = mixed
[1 2] | IndexOf(5) = missing
[r s b run runs element mixed missing]
"#,
  );
  assert_eq!(v.to_string(), r#"[[3 2 1] "olléh" "0201" 1 [1 4] 1 1 -1]"#);
  for script in [r#"[1 2] | IndexOf("a")"#, "[1 2] | IndexOf([])"] {
    assert!(!check(script).ok(), "{script}");
  }
}

#[test]
fn checks_assertions_and_type_changes() {
  let v = root_value(
    r#"
[(true | IsTrue) (true | IsFalse) (false | IsFalse)] = bools
5 | Pass | Assert.Is(5.0) | Assert.IsNot(4) = passed
none | ToAny | Var(anything)
"text" | Update(anything)
{a: 1} | ToAnyTable | Var(t)
{b: "x"} | Update(t)
"[[1.5], [\"a\"]]" | FromJson | ExpectSeq = rows
rows | Take(0) | ExpectFloatSeq = floats
rows | Take(1) | ExpectStringSeq = strings
[bools passed (anything | ExpectString) t floats strings]
"#,
  );
  assert_eq!(
    v.to_string(),
    r#"[[true false true] 5 "text" {b: "x"} [1.5] ["a"]]"#
  );
  assert_eq!(
    root_failure("[1 2] | Assert.Is([1 3])"),
    "activation error: Assert.Is: expected [1 3], got [1 2]"
  );
  assert!(root_failure("1 | Assert.IsNot(1.0)").contains("anything but 1.0"));
  assert!(
    root_failure(r#""[1, \"a\"]" | FromJson | ExpectIntSeq"#).contains("element 1 is String")
  );
  for (script, code) in [
    (r#"1 | Assert.Is("a")"#, "input-type-mismatch"),
    ("1 | ExpectIntSeq", "input-type-mismatch"),
    ("1 | IsTrue", "input-type-mismatch"),
    ("1 | ToAnyTable", "input-type-mismatch"),
    // Any must be narrowed before typed use.
    ("1 | ToAny | Add(1)", "input-type-mismatch"),
  ] {
    let d = load_or_compose_errors(script);
    assert_eq!(d[0].code, code, "{script}: {}", d[0].message);
  }
}

#[test]
fn cond_runs_the_first_true_branch_and_when_not_the_false_one() {
  let v = root_value(
    r#"
0 | Var(checked)
5 | Cond({Inc(checked) | false} {"a"} {Inc(checked) | true} {"b"} {Inc(checked) | true} {"c"} passthrough: false) = picked
-1 | Cond({IsMore(0)} {"positive"} passthrough: false) = unmatched
5 | Cond({true} {"ignored"}) | Add(1) = passed
[] | ExpectIntSeq | Var(seen)
3 | WhenNot({IsMore(4)} {AppendTo(seen)})
7 | WhenNot({IsMore(4)} {AppendTo(seen)})
#( 2 | Cond({IsLess(0)} {"neg"} {IsMore(0)} {"pos"} passthrough: false) ) = folded
[picked checked unmatched passed seen folded]
"#,
  );
  assert_eq!(v.to_string(), r#"["b" 2 -1 6 [3] "pos"]"#);
  // Without passthrough the output is the actions' and the input's union.
  let d = load_or_compose_errors(r#"1 | Cond({true} {"a"} passthrough: false) | String.Trim"#);
  assert_eq!(d[0].code, "input-type-mismatch");
  for (script, code) in [
    ("1 | Cond({true})", "cond-pairs"),
    ("1 | Cond({1} {2})", "predicate-not-bool"),
    ("1 | WhenNot({1} {Pass})", "predicate-not-bool"),
  ] {
    let d = load_or_compose_errors(script);
    assert_eq!(d[0].code, code, "{script}: {}", d[0].message);
  }
  // A problem inside a branch is located there.
  let d = load_or_compose_errors("1 | Cond({true} {\"a\"} {IsMore(\"x\")} {\"b\"})");
  assert_eq!((d[0].line, d[0].column), (Some(1), Some(24)));
}

#[test]
fn msg_writes_at_a_level_and_epoch_counts_from_1970() {
  let (report, lines) = otter_core::log::capture(|| {
    run(
      r#"
"in" | Msg("plain") | Msg("careful" level: "warning") | Msg("hidden" level: "debug")
"x" = text
Msg(text level: "error")
[Time.Epoch Time.EpochMs]
"#,
      &no_defines(),
    )
  });
  assert_eq!(lines, ["plain", "[warning] careful", "[error] x"]);
  let v = completed(&report, "root");
  let (Some(Var::Int(s)), Some(Var::Int(ms))) = (
    v.as_seq().and_then(|s| s.first()).cloned(),
    v.as_seq().and_then(|s| s.get(1)).cloned(),
  ) else {
    panic!("{v}")
  };
  // After 2026-01-01 (a device without a time source counts from 1970),
  // and the same instant within a second.
  assert!(
    s > 1_767_225_600 || (cfg!(target_os = "espidf") && s >= 0),
    "{s}"
  );
  assert!((ms / 1000 - s).abs() <= 1, "{s} {ms}");
  let d = load_or_compose_errors(r#"Msg("x" level: "loud")"#);
  assert_eq!(d[0].code, "invalid-argument-value");
}

/// Past 16 nested flows, compose finds which argument holds a flow through
/// an index instead of a scan: problems and occurrences in any of them are
/// still located at their own source.
#[test]
fn many_flow_arguments_are_located_like_few() {
  use otter_core::diagnostic::PathStep;
  // The bad flow at each position, among few flows and among many.
  for flows in [3, 40] {
    for bad in [0, flows / 2, flows - 1] {
      let args: Vec<&str> = (0..flows)
        .map(|i| {
          if i == bad {
            "{Add(\"a\")}"
          } else {
            "{IsMore(0)}"
          }
        })
        .collect();
      let all = format!("1 | All({})", args.join(" "));
      let pairs: Vec<String> = args.iter().map(|p| format!("{p} {{\"x\"}}")).collect();
      let cond = format!("1 | Cond({})", pairs.join(" "));
      let cases: Vec<String> = (0..flows)
        .map(|i| match i == bad {
          true => format!("{i} {{Add(\"a\")}}"),
          false => format!("{i} {{\"x\"}}"),
        })
        .collect();
      let match_ = format!("1 | Match([{}] default: {{\"y\"}})", cases.join(" "));
      for script in [all, cond, match_] {
        let d = load_or_compose_errors(&script);
        let column = script.find("Add(\"a\")").unwrap() as u32 + 1;
        assert_eq!(at(&d[0]), (1, column), "{flows} flows, bad {bad}: {script}");
      }
    }
  }
  // Every occurrence in 40 flows is located at its own shard.
  let script = format!("1 | All({})", "{IsMore(0)} ".repeat(40));
  let report = check(&script);
  assert!(report.ok(), "{}", report.to_json());
  let mut columns: Vec<u32> = report.wires[0]
    .occurrences
    .iter()
    .filter(
      |o| matches!(o.occurrence.path.last(), Some(PathStep::Shard {name, ..}) if name == "IsMore"),
    )
    .map(|o| o.column.unwrap())
    .collect();
  columns.sort();
  let expected: Vec<u32> = script
    .match_indices("IsMore")
    .map(|(i, _)| i as u32 + 1)
    .collect();
  assert_eq!(columns, expected);
}
