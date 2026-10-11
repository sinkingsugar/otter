//! Acceptance tests for the §5 prototype (design doc) and the shard contract.

use otter_core::shards::defs::*;
use otter_core::shards::sim::{self, RequestState};
use otter_core::shards::{ProbeEvent, ProbeEventKind, take_probe_events};
use otter_core::{
  ComposeCache, Float2, Float3, Float4, FunctionDef, Outcome, ParamValue, Type, Var, WakeMode,
  WireDef, bench,
};

use otter_core::Mesh;

fn wire(name: &str, looped: bool, flow: Vec<otter_core::ShardDef>) -> WireDef {
  WireDef {
    name: name.into(),
    looped,
    flow,
  }
}

fn bench_mesh(n: i64) -> Mesh {
  let mut mesh = Mesh::new();
  for (name, value, mutable) in bench::mesh_vars(n) {
    mesh.declare_var(name, value, mutable);
  }
  for def in bench::functions() {
    mesh.add_function(def);
  }
  for def in bench::wires() {
    mesh.add_wire(def);
  }
  mesh
}

fn events_for(events: &[ProbeEvent], tag: &str, instance: u64, kind: ProbeEventKind) -> usize {
  events
    .iter()
    .filter(|e| e.tag == tag && e.instance == instance && e.kind == kind)
    .count()
}

fn check_discarded_constructor(table: bool) {
  use otter_core::ShardDef;
  use otter_core::shards::{data, values};
  use std::sync::Arc;

  fn owners(value: &Var) -> usize {
    match value {
      Var::Seq(v) => Arc::strong_count(v),
      Var::Table(v) => v.storage_owners(),
      Var::String(v) => Arc::strong_count(v),
      _ => unreachable!(),
    }
  }

  for value in [
    Var::from_seq(Arc::new(vec![Var::Int(7)])),
    Var::table([("field", Var::Int(7))]),
    Var::string("captured value"),
  ] {
    let constructor = if table {
      ShardDef::new(
        &data::TABLE_MAKE,
        vec![
          val(Var::from_seq(Arc::new(vec![Var::string("field")]))),
          var("captured"),
        ],
      )
    } else {
      ShardDef::new(&data::SEQ_MAKE, vec![var("captured")])
    };
    let mut mesh = Mesh::new();
    mesh.declare_var("captured", value.clone(), true);
    mesh.add_wire(wire(
      "main",
      false,
      vec![constructor, ShardDef::new(&values::COUNT, vec![]), pause()],
    ));
    let compiled = mesh.compile("main", Type::none()).unwrap();
    let id = mesh.spawn(&compiled, Var::None).unwrap();
    let before = owners(&value);
    mesh.tick();
    assert_eq!(
      mesh.outcome(id),
      None,
      "constructor state must still be alive"
    );
    assert_eq!(
      owners(&value),
      before,
      "discarded output still pins {value:?}"
    );
    mesh.tick();
    assert_eq!(mesh.outcome(id), Some(&Outcome::Completed(Var::Int(1))));
  }
}

#[test]
fn discarded_sequence_constructor_releases_captured_values() {
  check_discarded_constructor(false);
}

#[test]
fn discarded_table_constructor_releases_captured_values() {
  check_discarded_constructor(true);
}

#[test]
fn discarded_inline_input_does_not_copy_captured_sequence() {
  use otter_core::ShardDef;
  use otter_core::shards::data;
  use std::sync::Arc;

  for table in [false, true] {
    let mut mesh = Mesh::new();
    mesh.declare_var("acc", Var::from_seq(Arc::new(vec![Var::Int(7)])), true);
    let allocation = match &mesh.get_var("acc").unwrap() {
      Var::Seq(items) => Arc::as_ptr(items),
      _ => unreachable!(),
    };
    let constructor = if table {
      ShardDef::new(
        &data::TABLE_MAKE,
        vec![
          val(Var::from_seq(Arc::new(vec![Var::string("field")]))),
          var("acc"),
        ],
      )
    } else {
      ShardDef::new(&data::SEQ_MAKE, vec![var("acc")])
    };
    mesh.add_wire(wire(
      "main",
      false,
      vec![
        konst(Var::None),
        constructor,
        konst(Var::Int(1)),
        ShardDef::new(&data::PUSH, vec![var("acc")]),
        pause(),
      ],
    ));
    let compiled = mesh.compile("main", Type::none()).unwrap();
    let id = mesh.spawn(&compiled, Var::None).unwrap();
    mesh.tick();
    assert_eq!(mesh.outcome(id), None);
    let acc = mesh.get_var("acc");
    let Some(Var::Seq(items)) = &acc else {
      unreachable!()
    };
    assert_eq!(items.as_slice(), &[Var::Int(7), Var::Int(1)]);
    assert_eq!(
      Arc::as_ptr(items),
      allocation,
      "obsolete input forced a copy"
    );
  }
}

#[test]
fn inline_output_is_a_snapshot_across_suspend_and_nested_writes() {
  for boundary in [
    pause(),
    sub(vec![konst(Var::string("nested")), update("shared")]),
  ] {
    let mut mesh = Mesh::new();
    mesh.declare_var("shared", Var::string("original"), true);
    mesh.add_wire(wire("reader", false, vec![get("shared"), boundary]));
    mesh.add_wire(wire(
      "writer",
      false,
      vec![konst(Var::string("writer")), update("shared")],
    ));
    let reader = mesh.compile("reader", Type::none()).unwrap();
    let writer = mesh.compile("writer", Type::none()).unwrap();
    let id = mesh.spawn(&reader, Var::None).unwrap();
    mesh.spawn(&writer, Var::None).unwrap();
    mesh.tick();
    mesh.tick();
    assert_eq!(
      mesh.outcome(id),
      Some(&Outcome::Completed(Var::string("original")))
    );
    assert_eq!(mesh.get_var("shared"), Some(Var::string("writer")));
  }
}

#[test]
fn inline_assignment_preserves_other_aliases() {
  let mut mesh = Mesh::new();
  mesh.add_wire(wire(
    "main",
    false,
    vec![
      konst(Var::Int(7)),
      declare("a"),
      declare("b"),
      update("a"),
      add(var("a")),
      update("a"),
      inc("a"),
      get("b"),
    ],
  ));
  let compiled = mesh.compile("main", Type::none()).unwrap();
  let id = mesh.spawn(&compiled, Var::None).unwrap();
  mesh.tick();
  assert_eq!(mesh.outcome(id), Some(&Outcome::Completed(Var::Int(7))));
}

#[test]
fn call_sites_check_the_reload_registry_once_per_revision() {
  use otter_core::hash::HashSet;
  let declare = |mesh: &mut Mesh, both: bool| {
    // The probe keeps the body on the frame path (a small straight-line
    // body would be inlined into the caller, with no call site to check);
    // its events are drained after every tick below, since hundreds of
    // calls per tick would otherwise grow the probe log past the device's
    // heap.
    let mut body = vec![inc("a"), probe("f")];
    if both {
      body.push(inc("b"));
    }
    mesh.add_function(
      FunctionDef::new("F", Type::none(), Type::int())
        .uses(&["a", "b"])
        .mutates(&["a", "b"])
        .body(body),
    );
    mesh.add_wire(wire(
      "main",
      true,
      vec![repeat(vec![call("F", vec![])], val(Var::Int(100)))],
    ));
  };
  let mut mesh = Mesh::new();
  mesh.declare_var("a", Var::Int(0), true);
  mesh.declare_var("b", Var::Int(0), true);
  declare(&mut mesh, true);
  let main = mesh.compile("main", Type::none()).unwrap();
  mesh.spawn(&main, Var::None).unwrap();
  mesh.tick();
  otter_core::shards::take_probe_events();
  // No reload installed yet: a call never consults the registry.
  assert_eq!(mesh.reload_lookups(), 0);
  assert_eq!(mesh.get_var("a"), Some(Var::Int(100)));
  assert_eq!(mesh.get_var("b"), Some(Var::Int(100)));

  let reload = |mesh: &mut Mesh, both: bool| {
    let mut next = mesh.revision();
    declare(&mut next, both);
    next.compile("main", Type::none()).unwrap();
    mesh
      .validate_reload(&mut next, &HashSet::default())
      .unwrap();
    mesh.install_revision(next, &HashSet::default());
  };
  // An unchanged reload: one lookup for the call site, then plain calls.
  reload(&mut mesh, true);
  for _ in 0..3 {
    mesh.tick();
    otter_core::shards::take_probe_events();
  }
  assert_eq!(mesh.reload_lookups(), 1);
  assert_eq!(mesh.get_var("a"), Some(Var::Int(400)));

  // An edited body (reaching less of the mesh) is selected at the next
  // call after the reload, with one more lookup.
  reload(&mut mesh, false);
  for _ in 0..3 {
    mesh.tick();
    otter_core::shards::take_probe_events();
  }
  assert_eq!(mesh.reload_lookups(), 2);
  assert_eq!(mesh.get_var("a"), Some(Var::Int(700)));
  assert_eq!(mesh.get_var("b"), Some(Var::Int(400)));
}

#[test]
fn spawned_instances_share_one_compose() {
  // The classic ESP32 fits the gate's 100 entity instances with under 6 KiB
  // of heap to spare, and the 32-byte value adds about 150 bytes each.
  let n: i64 = if cfg!(target_os = "espidf") { 64 } else { 100 };
  let mut mesh = bench_mesh(n);
  let spawner = mesh.compile("spawner", Type::none()).unwrap();
  // The spawner and the entity it spawns, each composed once.
  let after_compile = mesh.cache_stats();
  assert_eq!(after_compile.wire_composes, 2);

  mesh.spawn(&spawner, Var::None).unwrap();
  for _ in 0..10 {
    mesh.tick();
    if mesh.get_var("ready-count") == Some(Var::Int(n)) {
      break;
    }
  }
  assert_eq!(mesh.get_var("ready-count"), Some(Var::Int(n)));
  // The instances were created and activated without any further compose.
  assert_eq!(
    mesh.cache_stats().wire_composes,
    after_compile.wire_composes
  );
  assert_eq!(
    mesh.cache_stats().shard_composes,
    after_compile.shard_composes
  );

  // Compiling the entity again is a cache hit returning the shared artifact.
  let hits = mesh.cache_stats().hits;
  mesh.compile("entity", Type::none()).unwrap();
  assert_eq!(mesh.cache_stats().hits, hits + 1);
  assert_eq!(
    mesh.cache_stats().wire_composes,
    after_compile.wire_composes
  );
}

#[test]
fn instances_have_isolated_state() {
  let mut mesh = Mesh::new();
  mesh.declare_var("total", Var::Int(0), true);
  // Counts its input down to zero, one step per tick, adding 1 to the mesh
  // total per step. Shared locals would corrupt the sum.
  mesh.add_wire(wire(
    "worker",
    false,
    vec![
      declare("left"),
      while_(
        vec![get("left"), is_more_equal(val(Var::Int(1)))],
        vec![
          get("left"),
          add(val(Var::Int(-1))),
          update("left"),
          inc("total"),
          pause(),
        ],
      ),
    ],
  ));
  let worker = mesh.compile("worker", Type::int()).unwrap();
  for k in 1..=10 {
    mesh.spawn(&worker, Var::Int(k)).unwrap();
  }
  mesh.run(100);
  assert_eq!(mesh.running(), 0);
  assert_eq!(mesh.get_var("total"), Some(Var::Int(55)));
  assert_eq!(mesh.cache_stats().wire_composes, 1);
}

#[test]
fn cache_reuses_equivalent_compose_and_misses_on_changed_dependencies() {
  let mut mesh = Mesh::new();
  mesh.add_wire(wire(
    "setter",
    false,
    vec![konst(Var::Int(5)), declare("g")],
  ));
  mesh.add_function(
    FunctionDef::new("Sub", Type::none(), Type::int()).body(vec![konst(Var::Int(1))]),
  );
  mesh.add_wire(wire("caller", false, vec![call("Sub", vec![])]));

  // Equivalent compose: hit.
  let local = mesh.compile("setter", Type::none()).unwrap();
  mesh.compile("setter", Type::none()).unwrap();
  assert_eq!(mesh.cache_stats().wire_composes, 1);
  assert_eq!(mesh.cache_stats().hits, 1);

  // "g" was absent, so Var declared a local. Declaring a mesh variable
  // "g" invalidates that recorded absence: the artifact is stale, and
  // composing again reports the clash (no shadowing).
  mesh.declare_var("g", Var::Int(0), true);
  assert!(
    mesh.spawn(&local, Var::None).is_err(),
    "stale artifact must be rejected"
  );
  assert_eq!(compose_error(&mut mesh, "setter").0, "duplicate-binding");
  // A wire assigning the mesh variable binds it.
  mesh.add_wire(wire(
    "updater",
    false,
    vec![konst(Var::Int(5)), update("g")],
  ));
  let bound = mesh.compile("updater", Type::none()).unwrap();
  assert_eq!(mesh.cache_stats().wire_composes, 2);
  let id = mesh.spawn(&bound, Var::None).unwrap();
  mesh.run(10);
  assert_eq!(mesh.outcome(id), Some(&Outcome::Completed(Var::Int(5))));
  assert_eq!(mesh.get_var("g"), Some(Var::Int(5)));

  // A called function's definition is a recorded dependency: miss.
  let first = mesh.compile("caller", Type::none()).unwrap();
  mesh.add_function(
    FunctionDef::new("Sub", Type::none(), Type::int()).body(vec![konst(Var::Int(2))]),
  );
  let second = mesh.compile("caller", Type::none()).unwrap();
  assert!(!std::sync::Arc::ptr_eq(&first, &second));
  let id = mesh.spawn(&second, Var::None).unwrap();
  mesh.run(10);
  assert_eq!(mesh.outcome(id), Some(&Outcome::Completed(Var::Int(2))));

  // A different input type is a different primary key: miss.
  let composes = mesh.cache_stats().wire_composes;
  mesh.add_wire(wire("echo", false, vec![]));
  mesh.compile("echo", Type::int()).unwrap();
  mesh.compile("echo", Type::float()).unwrap();
  assert_eq!(mesh.cache_stats().wire_composes, composes + 2);
}

#[test]
fn forced_hash_collisions_never_reuse_the_wrong_artifact() {
  let mut mesh = Mesh::with_cache(ComposeCache::with_hash_fn(|_, _| 0));
  mesh.add_wire(wire("one", false, vec![konst(Var::Int(1))]));
  mesh.add_wire(wire("two", false, vec![konst(Var::Int(2))]));
  let one = mesh.compile("one", Type::none()).unwrap();
  let two = mesh.compile("two", Type::none()).unwrap();
  assert_eq!(mesh.cache_stats().wire_composes, 2);
  mesh.compile("one", Type::none()).unwrap();
  assert_eq!(mesh.cache_stats().hits, 1);

  let a = mesh.spawn(&one, Var::None).unwrap();
  let b = mesh.spawn(&two, Var::None).unwrap();
  mesh.run(10);
  assert_eq!(mesh.outcome(a), Some(&Outcome::Completed(Var::Int(1))));
  assert_eq!(mesh.outcome(b), Some(&Outcome::Completed(Var::Int(2))));
}

/// Design doc §5, nested suspension acceptance test.
#[test]
fn nested_suspension_cancel_and_resume() {
  take_probe_events();
  let mut mesh = Mesh::new();
  mesh.add_function(
    FunctionDef::new("Inner", Type::int(), Type::int()).body(vec![
      probe("inner"),
      add(val(Var::Int(1))),
      pause(),
      add(val(Var::Int(10))),
      pause(),
      add(val(Var::Int(100))),
    ]),
  );
  mesh.add_wire(wire(
    "outer",
    false,
    vec![
      probe("outer"),
      declare("x"),
      get("x"),
      call("Inner", vec![]),
      update("x"),
      get("x"),
    ],
  ));
  let outer = mesh.compile("outer", Type::int()).unwrap();

  // Two instances of one shared compiled wire, suspended inside the nested
  // invocation at different points, with distinct state.
  let a = mesh.spawn(&outer, Var::Int(0)).unwrap();
  mesh.tick(); // a: at the first pause
  let b = mesh.spawn(&outer, Var::Int(1000)).unwrap();
  mesh.tick(); // a: at the second pause; b: at the first

  let before_cancel = take_probe_events();
  assert_eq!(
    events_for(&before_cancel, "inner", a, ProbeEventKind::Activate),
    1
  );
  assert_eq!(
    events_for(&before_cancel, "inner", b, ProbeEventKind::Activate),
    1
  );

  // Cancel a: its suspended nested execution unwinds, then cleanup runs
  // exactly once for each of its shard states.
  mesh.cancel(a);
  assert_eq!(mesh.outcome(a), Some(&Outcome::Cancelled));
  let on_cancel = take_probe_events();
  assert_eq!(
    events_for(&on_cancel, "outer", a, ProbeEventKind::Cleanup),
    1
  );
  assert_eq!(
    events_for(&on_cancel, "inner", a, ProbeEventKind::Cleanup),
    1
  );
  assert!(
    on_cancel.iter().all(|e| e.instance == a),
    "cancelling a must not touch b"
  );

  // b resumes unaffected and completes with its own result.
  mesh.run(10);
  assert_eq!(mesh.outcome(b), Some(&Outcome::Completed(Var::Int(1111))));
  let after = take_probe_events();
  assert_eq!(events_for(&after, "outer", b, ProbeEventKind::Cleanup), 1);
  assert_eq!(events_for(&after, "inner", b, ProbeEventKind::Cleanup), 1);
  assert!(
    after.iter().all(|e| e.instance == b),
    "a must never resume after cancel"
  );

  // Further ticks resume neither, and repeat no cleanup.
  for _ in 0..5 {
    mesh.tick();
  }
  assert!(take_probe_events().is_empty());
}

#[test]
fn failed_instantiation_releases_what_it_acquired() {
  take_probe_events();
  let mut mesh = Mesh::new();
  mesh.add_wire(wire(
    "broken",
    false,
    vec![
      probe("first"),
      probe_mode("second", "fail-instantiate"),
      probe("never"),
    ],
  ));
  let broken = mesh.compile("broken", Type::none()).unwrap();
  let id = mesh.spawn(&broken, Var::None).unwrap();
  mesh.run(5);
  assert!(matches!(mesh.outcome(id), Some(Outcome::Failed(_))));
  let events = take_probe_events();
  assert_eq!(
    events_for(&events, "first", id, ProbeEventKind::Instantiate),
    1
  );
  assert_eq!(events_for(&events, "first", id, ProbeEventKind::Cleanup), 1);
  assert_eq!(events.len(), 2, "unexpected events: {events:?}");
}

#[test]
fn loop_iterations_are_not_termination() {
  take_probe_events();
  let mut mesh = Mesh::new();
  mesh.add_wire(wire("looper", true, vec![probe("loop")]));
  let looper = mesh.compile("looper", Type::none()).unwrap();
  let id = mesh.spawn(&looper, Var::None).unwrap();
  for _ in 0..3 {
    mesh.tick();
  }
  let events = take_probe_events();
  assert_eq!(
    events_for(&events, "loop", id, ProbeEventKind::Instantiate),
    1
  );
  assert_eq!(events_for(&events, "loop", id, ProbeEventKind::Activate), 3);
  assert_eq!(events_for(&events, "loop", id, ProbeEventKind::Cleanup), 0);

  // Dropping the mesh cancels the instance: cleanup exactly once.
  drop(mesh);
  let events = take_probe_events();
  assert_eq!(events_for(&events, "loop", id, ProbeEventKind::Cleanup), 1);
  assert_eq!(events.len(), 1);
}

#[test]
fn meshes_sharing_a_compiled_wire_keep_separate_mesh_frames() {
  let mut one = bench_mesh(0);
  let mut two = bench_mesh(0);
  let entity = one.compile("entity", Type::none()).unwrap();
  for _ in 0..3 {
    one.spawn(&entity, Var::None).unwrap();
  }
  for _ in 0..5 {
    two.spawn(&entity, Var::None).unwrap();
  }
  one.tick();
  two.tick();
  assert_eq!(one.get_var("ready-count"), Some(Var::Int(3)));
  assert_eq!(two.get_var("ready-count"), Some(Var::Int(5)));

  // A mesh whose layout differs from what the wire was compiled against
  // rejects it.
  let mut other = Mesh::new();
  other.declare_var("ready-count", Var::Float(0.0), true);
  for def in bench::functions() {
    other.add_function(def);
  }
  for def in bench::wires() {
    other.add_wire(def);
  }
  assert!(other.spawn(&entity, Var::None).is_err());
}

// --- review findings (Astra, 2026-10-04) ---

/// The compose error's code and message (structured or plain).
fn compose_error(mesh: &mut Mesh, name: &str) -> (String, String) {
  match mesh.compile(name, Type::none()) {
    Err(otter_core::Error::Compose(msg)) => (String::new(), msg),
    Err(otter_core::Error::Diagnostic(d)) => (d.code.to_string(), d.message),
    other => panic!(
      "expected a compose error, got {:?}",
      other.map(|w| w.flow.output)
    ),
  }
}

#[test]
fn names_declared_in_blocks_do_not_escape() {
  let mut mesh = Mesh::new();
  // Declared in a branch, then read after it (golden-path.md §3.2).
  mesh.add_wire(wire(
    "branch",
    false,
    vec![
      when(
        vec![konst(Var::Bool(false))],
        vec![konst(Var::Int(7)), declare("x")],
      ),
      get("x"),
    ],
  ));
  let (code, message) = compose_error(&mut mesh, "branch");
  assert_eq!(code, "unknown-variable");
  assert!(message.contains("unknown variable x"));

  // Declared in a loop body.
  mesh.add_wire(wire(
    "zero-loop",
    false,
    vec![
      repeat(vec![konst(Var::Int(1)), declare("y")], val(Var::Int(0))),
      get("y"),
    ],
  ));
  let (code, message) = compose_error(&mut mesh, "zero-loop");
  assert_eq!(code, "unknown-variable");
  assert!(message.contains("unknown variable y"));

  // Valid patterns: used only inside the branch, declared again after
  // it (the block's name is gone), or declared before it and updated
  // inside.
  mesh.add_wire(wire(
    "valid",
    false,
    vec![
      when(
        vec![konst(Var::Bool(true))],
        vec![konst(Var::Int(1)), declare("a"), get("a")],
      ),
      konst(Var::Int(2)),
      declare("a"),
      get("a"),
      konst(Var::Int(0)),
      declare("b"),
      when(
        vec![konst(Var::Bool(true))],
        vec![konst(Var::Int(5)), update("b")],
      ),
      get("b"),
    ],
  ));
  let valid = mesh.compile("valid", Type::none()).unwrap();
  let id = mesh.spawn(&valid, Var::None).unwrap();
  mesh.run(5);
  assert_eq!(mesh.outcome(id), Some(&Outcome::Completed(Var::Int(5))));
}

/// Frontend temporaries (`%` names) stay visible after the block declaring
/// them, but a block that might not run leaves them uninitialized: an
/// assignment counts after it only when nothing conditional encloses it.
#[test]
fn assignments_in_branches_do_not_initialize_after_them() {
  let branch = |flow: Vec<otter_core::ShardDef>| when(vec![konst(Var::Bool(true))], flow);
  let mut mesh = Mesh::new();
  let uninitialized = [
    // Declared in a branch.
    vec![branch(vec![konst(Var::Int(1)), declare("%a")]), get("%a")],
    // Declared in a branch, assigned in another.
    vec![
      branch(vec![konst(Var::Int(1)), declare("%a")]),
      branch(vec![konst(Var::Int(2)), update("%a")]),
      get("%a"),
    ],
    // Assigned in a nested branch, read in the enclosing one.
    vec![
      branch(vec![konst(Var::Int(1)), declare("%a")]),
      branch(vec![
        branch(vec![konst(Var::Int(2)), update("%a")]),
        get("%a"),
      ]),
    ],
  ];
  for (i, flow) in uninitialized.into_iter().enumerate() {
    let name = format!("uninitialized-{i}");
    mesh.add_wire(wire(&name, false, flow));
    let (code, _) = compose_error(&mut mesh, &name);
    assert_eq!(code, "possibly-uninitialized", "{name}");
  }
  // Assigned unconditionally after the branch, or in the enclosing branch
  // before a nested one: initialized from there on.
  let initialized = [
    (
      vec![
        branch(vec![konst(Var::Int(1)), declare("%a")]),
        konst(Var::Int(2)),
        update("%a"),
        get("%a"),
      ],
      Var::Int(2),
    ),
    (
      vec![
        branch(vec![konst(Var::Int(1)), declare("%a")]),
        branch(vec![
          konst(Var::Int(2)),
          update("%a"),
          branch(vec![konst(Var::Int(3)), update("%a")]),
          get("%a"),
        ]),
      ],
      // When outputs its input.
      Var::None,
    ),
  ];
  for (i, (flow, expected)) in initialized.into_iter().enumerate() {
    let name = format!("initialized-{i}");
    mesh.add_wire(wire(&name, false, flow));
    let compiled = mesh
      .compile(&name, Type::none())
      .unwrap_or_else(|e| panic!("{name}: {e:?}"));
    let id = mesh.spawn(&compiled, Var::None).unwrap();
    mesh.run(5);
    assert_eq!(
      mesh.outcome(id),
      Some(&Outcome::Completed(expected)),
      "{name}"
    );
  }
}

// --- occurrence paths in diagnostics ---

fn diagnostic(mesh: &mut Mesh, name: &str) -> otter_core::Diagnostic {
  match mesh.compile(name, Type::none()) {
    Err(otter_core::Error::Diagnostic(d)) => *d,
    other => panic!(
      "expected a diagnostic, got {:?}",
      other.map(|w| w.flow.output)
    ),
  }
}

#[test]
fn diagnostics_carry_the_occurrence_path() {
  // Vectors of different sizes do not mix.
  let bad_add = || {
    vec![
      konst(Var::Float3(Float3([0.0; 3]))),
      add(val(Var::Float2(Float2([0.0; 2])))),
    ]
  };
  let mut mesh = Mesh::new();
  // Two Adds: the valid one at the top, the failing one in a When body.
  mesh.add_wire(wire(
    "nested",
    false,
    vec![
      konst(Var::Int(1)),
      add(val(Var::Int(1))),
      when(vec![konst(Var::Bool(true))], bad_add()),
    ],
  ));
  let d = diagnostic(&mut mesh, "nested");
  assert_eq!(d.code, "input-type-mismatch");
  assert_eq!(d.path_string(), "nested/2:When/action/1:Math.Add");

  // Through a call: the body starts its own steps, under the function.
  mesh.add_wire(wire("sub", false, bad_add()));
  mesh.add_function(FunctionDef::new("Sub", Type::int(), Type::int()).body(bad_add()));
  mesh.add_wire(wire(
    "caller",
    false,
    vec![konst(Var::Int(0)), call("Sub", vec![])],
  ));
  let d = diagnostic(&mut mesh, "caller");
  assert_eq!(d.path_string(), "caller/1:Sub/Sub()/1:Math.Add");
  assert_eq!(
    d.shard.as_deref(),
    Some("Math.Add"),
    "the failing shard keeps ownership"
  );

  // Through Spawn, which composes the wire through the cache.
  mesh.add_wire(wire("spawner", false, vec![spawn("sub")]));
  let d = diagnostic(&mut mesh, "spawner");
  assert_eq!(d.path_string(), "spawner/0:Spawn/wire/sub/1:Math.Add");

  // A bad reference ends at the referencing shard.
  mesh.add_wire(wire("dangling", false, vec![spawn("missing")]));
  let d = diagnostic(&mut mesh, "dangling");
  assert_eq!(d.code, "unknown-wire");
  assert_eq!(d.path_string(), "dangling/0:Spawn");
  assert_eq!(d.param.as_deref(), Some("wire"));
  mesh.add_wire(wire("uncalled", false, vec![call("Missing", vec![])]));
  let d = diagnostic(&mut mesh, "uncalled");
  assert_eq!(d.code, "unknown-function");
  assert_eq!(d.path_string(), "uncalled/0:Missing");

  // Argument decoding errors have a path too.
  mesh.add_wire(wire(
    "decode",
    false,
    vec![once(vec![otter_core::ShardDef::new(
      &otter_core::shards::ADD,
      vec![],
    )])],
  ));
  let d = diagnostic(&mut mesh, "decode");
  assert_eq!(d.code, "missing-argument");
  assert_eq!(d.path_string(), "decode/0:Once/action/0:Math.Add");
  assert!(
      d.to_json().contains(
        "\"path\":[{\"wire\":\"decode\"},{\"shard\":0,\"name\":\"Once\"},{\"param\":\"action\"},{\"shard\":0,\"name\":\"Math.Add\"}]"
      ),
      "{}",
      d.to_json()
    );
}

#[test]
fn input_mismatches_say_where_the_input_came_from() {
  use otter_core::diagnostic::InputSource;
  let mut mesh = Mesh::new();
  // Const produces the Float3; Var and When pass it through to Add.
  mesh.add_wire(wire(
    "through",
    false,
    vec![
      konst(Var::Float3(Float3([0.0; 3]))),
      declare("x"),
      when(vec![konst(Var::Bool(true))], vec![]),
      add(val(Var::Float2(Float2([0.0; 2])))),
    ],
  ));
  let d = diagnostic(&mut mesh, "through");
  assert_eq!(
    d.input_from,
    Some(InputSource {
      origin: Some((0, "Const".into())),
      via: vec![(1, "Var".into()), (2, "When".into())],
    })
  );
  assert!(
      d.message.ends_with(
        "(the input comes from 0:Const, through 1:Var, 2:When, which pass their input through unchanged)"
      ),
      "{}",
      d.message
    );
  assert!(
      d.to_json().contains(
        "\"input_from\":{\"origin\":{\"shard\":0,\"name\":\"Const\"},\"via\":[{\"shard\":1,\"name\":\"Var\"},{\"shard\":2,\"name\":\"When\"}]}"
      ),
      "{}",
      d.to_json()
    );

  // The first shard of a nested flow gets the flow's own input.
  mesh.add_wire(wire(
    "nested",
    false,
    vec![
      konst(Var::Float3(Float3([0.0; 3]))),
      when(
        vec![konst(Var::Bool(true))],
        vec![add(val(Var::Float2(Float2([0.0; 2]))))],
      ),
    ],
  ));
  let d = diagnostic(&mut mesh, "nested");
  assert_eq!(d.path_string(), "nested/1:When/action/0:Math.Add");
  assert_eq!(
    d.input_from,
    Some(InputSource {
      origin: None,
      via: vec![]
    })
  );
  assert!(
    d.message.ends_with("(the input is the flow's own input)"),
    "{}",
    d.message
  );

  // Variable assignments are about the input too.
  mesh.add_wire(wire(
    "assign",
    false,
    vec![
      konst(Var::Int(1)),
      declare("v"),
      konst(Var::Float(1.0)),
      update("v"),
    ],
  ));
  let d = diagnostic(&mut mesh, "assign");
  assert_eq!(d.code, "variable-type-mismatch");
  assert_eq!(
    d.input_from.and_then(|s| s.origin),
    Some((2, "Const".into()))
  );
}

#[test]
fn ref_declares_an_immutable_variable() {
  let mut mesh = Mesh::new();
  // Ref in a loop body rebinds each iteration; the value is readable after.
  mesh.add_wire(wire(
    "ok",
    false,
    vec![
      konst(Var::Int(0)),
      declare("n"),
      repeat(vec![inc("n")], val(Var::Int(3))),
      get("n"),
      bind("r"),
      get("r"),
    ],
  ));
  let ok = mesh.compile("ok", Type::none()).unwrap();
  let id = mesh.spawn(&ok, Var::None).unwrap();
  mesh.run(5);
  assert_eq!(mesh.outcome(id), Some(&Outcome::Completed(Var::Int(3))));

  mesh.add_wire(wire(
    "update",
    false,
    vec![
      konst(Var::Int(1)),
      bind("r"),
      konst(Var::Int(2)),
      update("r"),
    ],
  ));
  assert_eq!(compose_error(&mut mesh, "update").0, "immutable-binding");
  mesh.add_wire(wire(
    "twice",
    false,
    vec![konst(Var::Int(1)), bind("r"), bind("r")],
  ));
  let d = diagnostic(&mut mesh, "twice");
  assert_eq!(
    (d.code, d.path_string().as_str()),
    ("duplicate-binding", "twice/2:Bind")
  );
  // The first declaration is the related location.
  let related = d.related.unwrap();
  assert_eq!(
    related.path,
    [
      otter_core::diagnostic::PathStep::Wire("twice".into()),
      otter_core::diagnostic::PathStep::Shard {
        index: 1,
        name: "Bind".into()
      }
    ]
  );
}

#[test]
fn sub_passes_its_input_through_and_resumes_inside() {
  let mut mesh = Mesh::new();
  // The Sub suspends (Pause) mid-flow, resumes there, assigns, and
  // passes its own input (7) through.
  mesh.add_wire(wire(
    "w",
    false,
    vec![
      konst(Var::Seq(Default::default())),
      declare("seen"),
      konst(Var::Int(0)),
      declare("before"),
      declare("after"),
      konst(Var::Int(7)),
      sub(vec![
        konst(Var::Int(1)),
        update("before"),
        pause(),
        konst(Var::Int(5)),
        update("after"),
      ]),
      push("seen"),
      get("after"),
      add(var("before")),
      push("seen"),
      get("seen"),
    ],
  ));
  let w = mesh.compile("w", Type::none()).unwrap();
  let id = mesh.spawn(&w, Var::None).unwrap();
  mesh.run(5);
  assert_eq!(
    mesh.outcome(id),
    Some(&Outcome::Completed(Var::from_seq(std::sync::Arc::new(
      vec![Var::Int(7), Var::Int(6)]
    ))))
  );
}

#[test]
fn take_types_follow_the_input() {
  let mut mesh = Mesh::new();
  let open = Var::table([("a", Var::Int(1))]);
  mesh.add_wire(wire(
    "w",
    false,
    vec![
      konst(Var::Float3(Float3([1.0, 2.0, 3.0]))),
      take(val(Var::Int(2))),
      bind("z"),
      konst(open),
      take(val(Var::string("a"))),
    ],
  ));
  let w = mesh.compile("w", Type::none()).unwrap();
  assert_eq!(w.flow.output, Type::int());
  assert_eq!(w.locals.lookup("z").map(|s| s.ty), Some(Type::float()));
  let id = mesh.spawn(&w, Var::None).unwrap();
  mesh.run(2);
  assert_eq!(mesh.outcome(id), Some(&Outcome::Completed(Var::Int(1))));

  // Out of range is an activation error.
  mesh.add_wire(wire(
    "oob",
    false,
    vec![
      konst(Var::Float2(Float2([0.0, 1.0]))),
      take(val(Var::Int(2))),
    ],
  ));
  let oob = mesh.compile("oob", Type::none()).unwrap();
  let id = mesh.spawn(&oob, Var::None).unwrap();
  mesh.run(2);
  assert!(matches!(mesh.outcome(id), Some(Outcome::Failed(_))));
}

// --- control-flow shards: cancellation, resumption, stops (review 2026-10-05) ---

/// Spawns `flow`, ticks once so it suspends inside, cancels it, and
/// checks every probe inside was cleaned up exactly once.
fn cancel_while_suspended(name: &str, flow: Vec<otter_core::ShardDef>, probes: &[&str]) {
  take_probe_events();
  let mut mesh = Mesh::new();
  mesh.add_wire(wire(name, false, flow));
  let w = mesh.compile(name, Type::none()).unwrap();
  let id = mesh.spawn(&w, Var::None).unwrap();
  mesh.tick();
  assert_eq!(mesh.outcome(id), None, "{name}: should be suspended");
  mesh.cancel(id);
  assert_eq!(mesh.outcome(id), Some(&Outcome::Cancelled), "{name}");
  let events = take_probe_events();
  for tag in probes {
    assert_eq!(
      events_for(&events, tag, id, ProbeEventKind::Cleanup),
      1,
      "{name}: {tag}"
    );
  }
}

#[test]
fn cancelling_inside_each_control_shard_cleans_up_once() {
  cancel_while_suspended(
    "if",
    vec![
      probe("outer"),
      if_(
        vec![konst(Var::Bool(true))],
        vec![probe("branch"), pause()],
        None,
      ),
    ],
    &["outer", "branch"],
  );
  cancel_while_suspended(
    "match",
    vec![
      probe("outer"),
      konst(Var::Int(1)),
      match_(vec![(Var::Int(1), vec![probe("case"), pause()])], vec![]),
    ],
    &["outer", "case"],
  );
  cancel_while_suspended(
    "maybe",
    vec![probe("outer"), maybe(vec![probe("action"), pause()], None)],
    &["outer", "action"],
  );
  cancel_while_suspended(
    "all",
    vec![
      probe("outer"),
      all(vec![ParamValue::Flow(vec![
        probe("cond"),
        pause(),
        konst(Var::Bool(true)),
      ])]),
    ],
    &["outer", "cond"],
  );
  cancel_while_suspended(
    "until",
    vec![
      probe("outer"),
      repeat_until(
        vec![],
        vec![probe("until"), pause(), konst(Var::Bool(false))],
      ),
    ],
    &["outer", "until"],
  );
}

#[test]
fn a_suspending_until_resumes_in_place() {
  let mut mesh = Mesh::new();
  mesh.add_wire(wire(
    "w",
    false,
    vec![
      konst(Var::Int(0)),
      declare("n"),
      repeat_until(
        vec![inc("n")],
        vec![pause(), get("n"), is_more_equal(val(Var::Int(3)))],
      ),
      get("n"),
    ],
  ));
  let w = mesh.compile("w", Type::none()).unwrap();
  let id = mesh.spawn(&w, Var::None).unwrap();
  mesh.run(20);
  assert_eq!(mesh.outcome(id), Some(&Outcome::Completed(Var::Int(3))));
}

#[test]
fn stop_inside_a_condition_stops_the_instance() {
  let mut mesh = Mesh::new();
  mesh.add_wire(wire(
    "w",
    false,
    vec![
      all(vec![ParamValue::Flow(vec![stop()])]),
      konst(Var::Int(1)),
    ],
  ));
  let w = mesh.compile("w", Type::none()).unwrap();
  let id = mesh.spawn(&w, Var::None).unwrap();
  mesh.run(2);
  assert_eq!(mesh.outcome(id), Some(&Outcome::Stopped));
}

#[test]
fn spawn_and_set_var_accept_values_the_type_admits() {
  let mut mesh = Mesh::new();
  mesh.add_wire(wire("w", false, vec![]));
  let maybe_int = Type::union([Type::int(), Type::none()]);
  let w = mesh.compile("w", maybe_int).unwrap();
  assert!(mesh.spawn(&w, Var::Int(1)).is_ok());
  assert!(mesh.spawn(&w, Var::None).is_ok());
  assert!(mesh.spawn(&w, Var::string("no")).is_err());
  // A union-typed mesh variable takes a narrower value without panicking.
  let mixed = Var::from_seq(std::sync::Arc::new(vec![Var::Int(1), Var::string("a")]));
  mesh.declare_var("v", mixed, true);
  let ints = Var::from_seq(std::sync::Arc::new(vec![Var::Int(2)]));
  mesh.set_var("v", ints.clone());
  assert_eq!(mesh.get_var("v"), Some(ints));
}

#[test]
fn take_finished_drains_finished_records_in_one_pass() {
  let mut mesh = Mesh::new();
  mesh.add_wire(wire("done", false, vec![konst(Var::Int(1))]));
  mesh.add_wire(wire("waits", false, vec![pause_secs(3600.0)]));
  let done = mesh.compile("done", Type::none()).unwrap();
  let waits = mesh.compile("waits", Type::none()).unwrap();
  let finished_ids: Vec<_> = (0..3)
    .map(|_| mesh.spawn(&done, Var::None).unwrap())
    .collect();
  let pending = mesh.spawn(&waits, Var::None).unwrap();
  mesh.tick();
  let finished = mesh.take_finished();
  assert_eq!(finished.len(), 3);
  for (id, wire, outcome) in &finished {
    assert!(finished_ids.contains(id));
    assert_eq!(
      (wire.as_str(), outcome),
      ("done", &Outcome::Completed(Var::Int(1)))
    );
  }
  // Finished records are gone; the running one stays.
  assert_eq!(mesh.instance_ids(), vec![pending]);
  assert!(mesh.take_finished().is_empty());
}

// --- values and types (docs/values-and-types.md) ---

#[test]
fn variables_accept_values_by_the_acceptance_rule() {
  let mixed = || Var::from_seq(std::sync::Arc::new(vec![Var::Int(1), Var::string("a")]));
  let point =
    |x: f64, label: &str| Var::table([("x", Var::Float(x)), ("label", Var::string(label))]);
  let mut mesh = Mesh::new();
  // A mixed sequence types as a sequence of the union, so a narrower
  // sequence can update it; a fixed table updates from the same shape.
  mesh.add_wire(wire(
    "accepted",
    false,
    vec![
      konst(mixed()),
      declare("xs"),
      konst(Var::from_seq(std::sync::Arc::new(vec![Var::Int(2)]))),
      update("xs"),
      konst(point(1.0, "a")),
      declare("p"),
      konst(point(2.0, "b")),
      update("p"),
      konst(Var::Float4(Float4([1.0, 2.0, 3.0, 4.0]))),
      declare("pose"),
      get("p"),
    ],
  ));
  let accepted = mesh.compile("accepted", Type::none()).unwrap();
  assert_eq!(accepted.flow.output.to_string(), "{label: String x: Float}");
  let id = mesh.spawn(&accepted, Var::None).unwrap();
  mesh.run(5);
  assert_eq!(mesh.outcome(id), Some(&Outcome::Completed(point(2.0, "b"))));

  // A Float sequence is not an (Int | String) sequence.
  mesh.add_wire(wire(
    "wrong-element",
    false,
    vec![
      konst(mixed()),
      declare("xs"),
      konst(Var::from_seq(std::sync::Arc::new(vec![Var::Float(1.0)]))),
      update("xs"),
    ],
  ));
  let (code, message) = compose_error(&mut mesh, "wrong-element");
  assert_eq!(code, "variable-type-mismatch");
  assert!(message.contains("[(Int | String)]"), "{message}");

  // A fixed table rejects a key it does not have.
  mesh.add_wire(wire(
    "extra-key",
    false,
    vec![
      konst(point(1.0, "a")),
      declare("p"),
      konst(Var::table([
        ("x", Var::Float(1.0)),
        ("label", Var::string("a")),
        ("z", Var::Float(0.0)),
      ])),
      update("p"),
    ],
  ));
  let (code, message) = compose_error(&mut mesh, "extra-key");
  assert_eq!(code, "variable-type-mismatch");
  assert!(message.contains("{label: String x: Float}"), "{message}");
}

#[test]
fn integer_overflow_fails_the_instance_with_cleanup() {
  take_probe_events();
  let mut mesh = Mesh::new();
  mesh.add_wire(wire(
    "overflow",
    false,
    vec![
      probe("resource"),
      konst(Var::Int(i64::MAX)),
      declare("x"),
      inc("x"),
    ],
  ));
  let overflow = mesh.compile("overflow", Type::none()).unwrap();
  let id = mesh.spawn(&overflow, Var::None).unwrap();
  mesh.run(5);
  assert!(
    matches!(mesh.outcome(id), Some(Outcome::Failed(otter_core::Error::Activation(m))) if m.contains("overflow"))
  );
  let events = take_probe_events();
  assert_eq!(
    events_for(&events, "resource", id, ProbeEventKind::Cleanup),
    1
  );
}

#[test]
#[cfg(panic = "unwind")]
fn activation_panic_fails_only_that_instance_and_still_cleans_up() {
  take_probe_events();
  let mut mesh = Mesh::new();
  mesh.add_wire(wire(
    "panicky",
    false,
    vec![
      probe("resource"),
      pause(),
      probe_mode("boom", "panic-activate"),
    ],
  ));
  mesh.add_wire(wire(
    "calm",
    false,
    vec![probe("calm"), pause(), konst(Var::Int(3))],
  ));
  let panicky = mesh.compile("panicky", Type::none()).unwrap();
  let calm = mesh.compile("calm", Type::none()).unwrap();
  let a = mesh.spawn(&panicky, Var::None).unwrap();
  let b = mesh.spawn(&calm, Var::None).unwrap();

  // The panic happens on the second tick, after a suspension. Ticks never
  // panic; the instance fails and its states are cleaned up exactly once.
  mesh.run(10);
  assert!(
    matches!(mesh.outcome(a), Some(Outcome::Failed(otter_core::Error::Activation(m))) if m.contains("panic in activation"))
  );
  assert_eq!(mesh.outcome(b), Some(&Outcome::Completed(Var::Int(3))));
  let events = take_probe_events();
  assert_eq!(
    events_for(&events, "resource", a, ProbeEventKind::Cleanup),
    1
  );
  assert_eq!(events_for(&events, "boom", a, ProbeEventKind::Cleanup), 1);
  assert_eq!(events_for(&events, "calm", b, ProbeEventKind::Cleanup), 1);

  // The failed instance is never resumed again, and dropping the mesh does
  // not panic or repeat cleanup.
  mesh.tick();
  drop(mesh);
  assert!(take_probe_events().is_empty());
}

#[test]
#[cfg(panic = "unwind")]
fn instantiate_panic_releases_what_was_acquired() {
  take_probe_events();
  let mut mesh = Mesh::new();
  mesh.add_wire(wire(
    "broken",
    false,
    vec![
      probe("first"),
      probe_mode("second", "panic-instantiate"),
      probe("never"),
    ],
  ));
  let broken = mesh.compile("broken", Type::none()).unwrap();
  let id = mesh.spawn(&broken, Var::None).unwrap();
  mesh.run(5);
  assert!(matches!(mesh.outcome(id), Some(Outcome::Failed(_))));
  let events = take_probe_events();
  assert_eq!(
    events_for(&events, "first", id, ProbeEventKind::Instantiate),
    1
  );
  assert_eq!(events_for(&events, "first", id, ProbeEventKind::Cleanup), 1);
  assert_eq!(events.len(), 2, "unexpected events: {events:?}");
}

#[test]
#[cfg(panic = "unwind")]
fn cleanup_panic_still_cleans_up_every_other_state() {
  take_probe_events();
  let mut mesh = Mesh::new();
  mesh.add_wire(wire(
    "messy",
    false,
    vec![
      probe("first"),
      probe_mode("second", "panic-cleanup"),
      probe("third"),
    ],
  ));
  let messy = mesh.compile("messy", Type::none()).unwrap();
  let id = mesh.spawn(&messy, Var::None).unwrap();
  mesh.run(5);
  assert!(
    matches!(mesh.outcome(id), Some(Outcome::Failed(otter_core::Error::Activation(m))) if m.contains("panic in cleanup"))
  );
  let events = take_probe_events();
  for tag in ["first", "second", "third"] {
    assert_eq!(
      events_for(&events, tag, id, ProbeEventKind::Cleanup),
      1,
      "{tag}"
    );
  }
  drop(mesh);
  assert!(take_probe_events().is_empty());
}

/// Suspends inside every composite shard; the engine must produce
/// the same result after the same number of ticks.
#[test]
fn suspension_inside_every_composite_resumes_in_place() {
  let mut mesh = Mesh::new();
  mesh.add_wire(wire(
    "composites",
    false,
    vec![
      konst(Var::Int(0)),
      declare("c"),
      once(vec![pause(), get("c"), add(val(Var::Int(1))), update("c")]),
      when(
        vec![konst(Var::Bool(true))],
        vec![pause(), get("c"), add(val(Var::Int(10))), update("c")],
      ),
      repeat(
        vec![pause(), get("c"), add(val(Var::Int(100))), update("c")],
        val(Var::Int(3)),
      ),
      while_(
        vec![get("c"), is_less(val(Var::Int(313)))],
        vec![pause(), inc("c")],
      ),
      get("c"),
    ],
  ));
  let composites = mesh.compile("composites", Type::none()).unwrap();
  let id = mesh.spawn(&composites, Var::None).unwrap();
  // 1 (Once) + 1 (When) + 3 (Repeat) + 2 (While) = 7 suspensions.
  for _ in 0..7 {
    assert_eq!(mesh.tick(), 1);
  }
  assert_eq!(mesh.tick(), 0);
  assert_eq!(mesh.outcome(id), Some(&Outcome::Completed(Var::Int(313))));
}

/// Resuming continues where execution suspended: nodes that already ran
/// are not run again. Cancelling from deep inside cleans up once.
#[test]
fn deep_resume_never_reruns_completed_nodes() {
  take_probe_events();
  let mut mesh = Mesh::new();
  mesh.add_function(
    FunctionDef::new("DeepInner", Type::none(), Type::none()).body(vec![
      probe("inner"),
      while_(vec![konst(Var::Bool(true))], vec![pause()]),
    ]),
  );
  mesh.add_wire(wire(
    "deep",
    false,
    vec![
      probe("outer"),
      when(
        vec![konst(Var::Bool(true))],
        vec![call("DeepInner", vec![])],
      ),
    ],
  ));
  let deep = mesh.compile("deep", Type::none()).unwrap();
  let id = mesh.spawn(&deep, Var::None).unwrap();
  for _ in 0..5 {
    mesh.tick();
  }
  let events = take_probe_events();
  assert_eq!(
    events_for(&events, "outer", id, ProbeEventKind::Activate),
    1
  );
  assert_eq!(
    events_for(&events, "inner", id, ProbeEventKind::Activate),
    1
  );

  mesh.cancel(id);
  assert_eq!(mesh.outcome(id), Some(&Outcome::Cancelled));
  let events = take_probe_events();
  assert_eq!(events_for(&events, "outer", id, ProbeEventKind::Cleanup), 1);
  assert_eq!(events_for(&events, "inner", id, ProbeEventKind::Cleanup), 1);
  assert_eq!(events.len(), 2, "unexpected events: {events:?}");
}

// --- async shards ---

/// Advances the simulated service, then ticks the mesh.
fn tick_with_service(mesh: &mut Mesh) -> usize {
  sim::advance();
  mesh.tick()
}

fn run_with_service(mesh: &mut Mesh, max_ticks: usize) {
  for _ in 0..max_ticks {
    if tick_with_service(mesh) == 0 {
      return;
    }
  }
}

#[test]
fn async_request_completes_through_nested_flows() {
  for mode in [WakeMode::PollEveryTick, WakeMode::OnNotify] {
    sim::reset();
    take_probe_events();
    let mut mesh = Mesh::new();
    mesh.set_wake_mode(mode);
    mesh.add_function(
      FunctionDef::new("Fetch", Type::none(), Type::int()).body(vec![request(3, false, true)]),
    );
    mesh.add_wire(wire(
      "client",
      false,
      vec![
        probe("client"),
        konst(Var::Int(-1)),
        declare("id"),
        when(
          vec![konst(Var::Bool(true))],
          vec![call("Fetch", vec![]), update("id")],
        ),
        get("id"),
      ],
    ));
    let client = mesh.compile("client", Type::none()).unwrap();
    let id = mesh.spawn(&client, Var::None).unwrap();
    run_with_service(&mut mesh, 20);
    assert_eq!(
      mesh.outcome(id),
      Some(&Outcome::Completed(Var::Int(0))),
      "{mode:?}"
    );
    assert_eq!(sim::request_states(), vec![RequestState::Completed]);
    let events = take_probe_events();
    assert_eq!(
      events_for(&events, "client", id, ProbeEventKind::Activate),
      1
    );
    assert_eq!(
      events_for(&events, "client", id, ProbeEventKind::Cleanup),
      1
    );
  }
}

#[test]
fn async_request_failure_fails_the_instance_with_cleanup() {
  sim::reset();
  take_probe_events();
  let mut mesh = Mesh::new();
  mesh.add_wire(wire(
    "failing",
    false,
    vec![probe("client"), request(2, true, true)],
  ));
  let failing = mesh.compile("failing", Type::none()).unwrap();
  let id = mesh.spawn(&failing, Var::None).unwrap();
  run_with_service(&mut mesh, 20);
  assert!(
    matches!(mesh.outcome(id), Some(Outcome::Failed(otter_core::Error::Activation(m))) if m.contains("request 0 failed"))
  );
  assert_eq!(sim::request_states(), vec![RequestState::Failed]);
  let events = take_probe_events();
  assert_eq!(
    events_for(&events, "client", id, ProbeEventKind::Cleanup),
    1
  );
}

#[test]
fn cancel_while_pending_aborts_the_request_before_releasing_resources() {
  sim::reset();
  take_probe_events();
  let mut mesh = Mesh::new();
  mesh.add_wire(wire(
    "slow",
    false,
    vec![probe("resource"), request(100, false, true)],
  ));
  let slow = mesh.compile("slow", Type::none()).unwrap();
  let id = mesh.spawn(&slow, Var::None).unwrap();
  for _ in 0..3 {
    tick_with_service(&mut mesh);
  }
  assert_eq!(sim::request_states(), vec![RequestState::Pending]);

  mesh.cancel(id);
  assert_eq!(mesh.outcome(id), Some(&Outcome::Cancelled));
  assert_eq!(sim::request_states(), vec![RequestState::Aborted]);
  // The pending future is dropped (aborting the request) before the
  // resource held by an earlier shard is released.
  let events: Vec<_> = take_probe_events()
    .into_iter()
    .filter(|e| e.kind != ProbeEventKind::Instantiate && e.kind != ProbeEventKind::Activate)
    .collect();
  assert_eq!(events.len(), 2, "unexpected events: {events:?}");
  assert_eq!(
    (events[0].tag.as_str(), events[0].kind),
    ("request", ProbeEventKind::Aborted)
  );
  assert_eq!(
    (events[1].tag.as_str(), events[1].kind),
    ("resource", ProbeEventKind::Cleanup)
  );
}

#[test]
fn late_wakeup_after_cancel_is_harmless() {
  for mode in [WakeMode::PollEveryTick, WakeMode::OnNotify] {
    sim::reset();
    take_probe_events();
    let mut mesh = Mesh::new();
    mesh.set_wake_mode(mode);
    // Detached on drop: the request keeps running after cancellation and
    // later fires the cancelled instance's waker.
    mesh.add_wire(wire(
      "detached",
      false,
      vec![probe("client"), request(5, false, false)],
    ));
    let detached = mesh.compile("detached", Type::none()).unwrap();
    let id = mesh.spawn(&detached, Var::None).unwrap();
    tick_with_service(&mut mesh);
    tick_with_service(&mut mesh);
    mesh.cancel(id);
    assert_eq!(sim::request_states(), vec![RequestState::Detached]);
    take_probe_events();

    for _ in 0..10 {
      tick_with_service(&mut mesh);
    }
    assert_eq!(sim::request_states(), vec![RequestState::DetachedCompleted]);
    assert_eq!(mesh.outcome(id), Some(&Outcome::Cancelled), "{mode:?}");
    assert!(
      take_probe_events().is_empty(),
      "cancelled instance must not resume"
    );
    assert_eq!(mesh.running(), 0);
  }
}

#[test]
fn notified_waits_poll_only_when_woken() {
  let mut polls = Vec::new();
  for mode in [WakeMode::PollEveryTick, WakeMode::OnNotify] {
    sim::reset();
    let mut mesh = Mesh::new();
    mesh.set_wake_mode(mode);
    mesh.add_wire(wire("waiter", false, vec![request(10, false, true)]));
    let waiter = mesh.compile("waiter", Type::none()).unwrap();
    let id = mesh.spawn(&waiter, Var::None).unwrap();
    run_with_service(&mut mesh, 30);
    assert!(matches!(mesh.outcome(id), Some(Outcome::Completed(_))));
    polls.push(sim::total_polls());
  }
  // Polling every tick re-checks the pending request each tick; notified
  // waiting polls once to start and once after the wakeup.
  assert!(polls[0] >= 10, "poll mode polled {} times", polls[0]);
  assert_eq!(polls[1], 2);
}

#[test]
fn pause_still_resumes_every_tick_in_notify_mode() {
  sim::reset();
  let mut mesh = Mesh::new();
  mesh.set_wake_mode(WakeMode::OnNotify);
  mesh.add_wire(wire(
    "pauser",
    false,
    vec![pause(), pause(), konst(Var::Int(1))],
  ));
  let pauser = mesh.compile("pauser", Type::none()).unwrap();
  let id = mesh.spawn(&pauser, Var::None).unwrap();
  assert_eq!(mesh.tick(), 1);
  assert_eq!(mesh.tick(), 1);
  assert_eq!(mesh.tick(), 0);
  assert_eq!(mesh.outcome(id), Some(&Outcome::Completed(Var::Int(1))));
}

// --- lifecycle under combined failures (review, 2026-10-04) ---

#[cfg(panic = "unwind")]
fn cleanups(events: &[ProbeEvent], tag: &str) -> usize {
  events
    .iter()
    .filter(|e| e.tag == tag && e.kind == ProbeEventKind::Cleanup)
    .count()
}

#[test]
#[cfg(panic = "unwind")]
fn rollback_cleanup_panic_does_not_skip_earlier_states() {
  take_probe_events();
  let mut mesh = Mesh::new();
  mesh.add_wire(wire(
    "rollback",
    false,
    vec![
      probe("first"),
      probe_mode("second", "panic-cleanup"),
      probe_mode("third", "fail-instantiate"),
    ],
  ));
  let rollback = mesh.compile("rollback", Type::none()).unwrap();
  let id = mesh.spawn(&rollback, Var::None).unwrap();
  mesh.tick(); // must not panic
  assert!(
    matches!(mesh.outcome(id), Some(Outcome::Failed(otter_core::Error::Activation(m))) if m.contains("failed to instantiate") && m.contains("rollback"))
  );
  let events = take_probe_events();
  assert_eq!(cleanups(&events, "first"), 1);
  assert_eq!(cleanups(&events, "second"), 1);
}

#[test]
#[cfg(panic = "unwind")]
fn composite_cleanup_panic_does_not_skip_sibling_flows() {
  take_probe_events();
  let mut mesh = Mesh::new();
  mesh.add_wire(wire(
    "composite",
    false,
    vec![when(
      vec![probe("predicate"), konst(Var::Bool(true))],
      vec![probe_mode("body", "panic-cleanup")],
    )],
  ));
  let composite = mesh.compile("composite", Type::none()).unwrap();
  let id = mesh.spawn(&composite, Var::None).unwrap();
  mesh.tick(); // must not panic
  assert!(matches!(mesh.outcome(id), Some(Outcome::Failed(_))));
  let events = take_probe_events();
  assert_eq!(cleanups(&events, "body"), 1);
  assert_eq!(cleanups(&events, "predicate"), 1);
}

#[test]
#[cfg(panic = "unwind")]
fn composite_partial_instantiation_rolls_back_with_cleanup_panics() {
  take_probe_events();
  let mut mesh = Mesh::new();
  // The predicate flow instantiates; the body fails to; rolling back the
  // predicate panics in one of its shards. Every instantiated state is
  // still cleaned up once, and the tick does not panic.
  mesh.add_wire(wire(
    "partial",
    false,
    vec![
      probe("first"),
      when(
        vec![
          probe("pred-a"),
          probe_mode("pred-b", "panic-cleanup"),
          konst(Var::Bool(true)),
        ],
        vec![probe_mode("body", "fail-instantiate")],
      ),
    ],
  ));
  let partial = mesh.compile("partial", Type::none()).unwrap();
  let id = mesh.spawn(&partial, Var::None).unwrap();
  mesh.tick();
  assert!(matches!(mesh.outcome(id), Some(Outcome::Failed(_))));
  let events = take_probe_events();
  for tag in ["first", "pred-a", "pred-b"] {
    assert_eq!(cleanups(&events, tag), 1, "{tag}");
  }
  assert_eq!(cleanups(&events, "body"), 0);
}

#[test]
fn finished_instances_release_input_and_locals() {
  for fail in [false, true] {
    let mut mesh = Mesh::new();
    let mut flow = vec![declare("x")];
    if fail {
      flow.push(probe_mode("broken", "fail-instantiate"));
    }
    flow.push(konst(Var::None));
    mesh.add_wire(wire("holder", false, flow));
    let holder = mesh.compile("holder", Type::string()).unwrap();
    let data: std::sync::Arc<str> = std::sync::Arc::from("owned input buffer");
    let weak = std::sync::Arc::downgrade(&data);
    let id = mesh.spawn(&holder, Var::from_string(data)).unwrap();
    mesh.tick();
    assert!(mesh.outcome(id).is_some());
    assert!(
      weak.upgrade().is_none(),
      "finished instance retained its input or locals (fail = {fail})"
    );
  }
}

#[test]
fn take_outcome_retires_finished_records() {
  let mut mesh = Mesh::new();
  mesh.add_wire(wire("quick", false, vec![konst(Var::Int(7))]));
  mesh.add_wire(wire("slow", false, vec![pause(), pause()]));
  let quick = mesh.compile("quick", Type::none()).unwrap();
  let slow = mesh.compile("slow", Type::none()).unwrap();
  let q = mesh.spawn(&quick, Var::None).unwrap();
  let s = mesh.spawn(&slow, Var::None).unwrap();
  assert_eq!(mesh.take_outcome(q), None, "not finished yet");
  mesh.tick();
  assert_eq!(mesh.take_outcome(s), None, "still running");
  assert_eq!(mesh.take_outcome(q), Some(Outcome::Completed(Var::Int(7))));
  assert_eq!(mesh.take_outcome(q), None, "already retired");
  assert_eq!(mesh.instance_ids(), vec![s]);
  mesh.run(5);
  assert_eq!(mesh.take_outcome(s), Some(Outcome::Completed(Var::None)));
  assert!(mesh.instance_ids().is_empty());
}

// --- Spawned instances (docs/host-requirements.md H5) ---

use otter_core::wire_instance::{Completion, Request, WireInstance};

fn completion(handle: &Var) -> &std::sync::Arc<Completion> {
  handle
    .as_object::<WireInstance>()
    .expect("a Wire.Instance")
    .completion()
}

fn failure_code(outcome: &Outcome) -> &'static str {
  match outcome {
    Outcome::Failed(otter_core::Error::Diagnostic(d)) => d.code,
    other => panic!("expected a diagnostic, got {other:?}"),
  }
}

/// The finished records by wire name (one each).
fn finished_by_wire(mesh: &mut Mesh) -> std::collections::HashMap<String, Outcome> {
  mesh
    .take_finished()
    .into_iter()
    .map(|(_, wire, outcome)| (wire, outcome))
    .collect()
}

/// A mesh holding `wires`, with `sleeper` (runs until stopped) spawned by
/// the host and its handle in the mesh variable `w`. Returns the handle.
fn mesh_with_sleeper(wires: Vec<WireDef>) -> (Mesh, Var) {
  let mut mesh = Mesh::new();
  mesh.add_wire(wire("sleeper", true, vec![pause()]));
  let sleeper = mesh.compile("sleeper", Type::none()).unwrap();
  let id = mesh.spawn(&sleeper, Var::None).unwrap();
  let handle = mesh.handle(id).unwrap();
  mesh.declare_var("w", handle.clone(), false);
  for def in wires {
    mesh.add_wire(def);
  }
  (mesh, handle)
}

fn start(mesh: &mut Mesh, name: &str) -> otter_core::InstanceId {
  let wire = mesh.compile(name, Type::none()).unwrap();
  mesh.spawn(&wire, Var::None).unwrap()
}

/// An owner's end cancels its children, theirs in turn, in the tick it
/// ends, whatever the wake mode; a looped child is not cancelled by its
/// owner's iterations.
#[test]
fn an_owner_ending_cancels_its_tree_in_the_same_tick() {
  for mode in [WakeMode::PollEveryTick, WakeMode::OnNotify] {
    let _ = take_probe_events();
    let mut mesh = Mesh::new();
    mesh.set_wake_mode(mode);
    mesh.add_wire(wire("grand", true, vec![probe("grand"), pause()]));
    mesh.add_wire(wire(
      "child",
      true,
      vec![probe("child"), once(vec![spawn("grand")]), pause()],
    ));
    mesh.add_wire(wire(
      "parent",
      false,
      vec![spawn("child"), pause(), pause(), pause()],
    ));
    start(&mut mesh, "parent");
    // Tick 1 spawns the child, tick 2 the grandchild; the parent ends in
    // tick 4.
    for _ in 0..3 {
      mesh.tick();
    }
    assert_eq!(mesh.running(), 3, "{mode:?}");
    assert_eq!(mesh.tick(), 0, "{mode:?}");
    let finished = finished_by_wire(&mut mesh);
    assert!(matches!(finished["parent"], Outcome::Completed(_)));
    assert_eq!(finished["child"], Outcome::Cancelled, "{mode:?}");
    assert_eq!(finished["grand"], Outcome::Cancelled, "{mode:?}");
    let events = take_probe_events();
    for tag in ["child", "grand"] {
      let cleanups = events
        .iter()
        .filter(|e| e.tag == tag && e.kind == ProbeEventKind::Cleanup)
        .count();
      assert_eq!(cleanups, 1, "{tag} {mode:?}");
    }
  }
}

/// A child spawned in the tick its owner ends is cancelled without being
/// instantiated; a detached one runs on.
#[test]
fn spawn_then_exit_cancels_the_queued_child_unless_detached() {
  let _ = take_probe_events();
  let mut mesh = Mesh::new();
  mesh.add_wire(wire("child", true, vec![probe("child"), pause()]));
  mesh.add_wire(wire("owner", false, vec![spawn("child")]));
  mesh.add_wire(wire("detacher", false, vec![spawn_detached("child")]));
  start(&mut mesh, "owner");
  assert_eq!(mesh.tick(), 0);
  let finished = finished_by_wire(&mut mesh);
  assert_eq!(finished["child"], Outcome::Cancelled);
  assert!(take_probe_events().is_empty(), "never instantiated");

  start(&mut mesh, "detacher");
  for _ in 0..5 {
    assert_eq!(mesh.tick(), 1);
  }
  let finished = finished_by_wire(&mut mesh);
  assert!(matches!(finished["detacher"], Outcome::Completed(_)));
  assert_eq!(mesh.instance_ids().len(), 1);
}

/// A child parked on async work, under `OnNotify`, is cancelled in the
/// tick its owner ends: the request wakes it; the pending future is
/// dropped (here aborting the request).
#[test]
fn a_parked_child_is_cancelled_when_its_owner_ends() {
  sim::reset();
  let mut mesh = Mesh::new();
  mesh.set_wake_mode(WakeMode::OnNotify);
  mesh.add_wire(wire("child", false, vec![request(1000, false, true)]));
  mesh.add_wire(wire("owner", false, vec![spawn("child"), pause(), pause()]));
  start(&mut mesh, "owner");
  mesh.tick(); // The owner spawns the child.
  mesh.tick(); // The child parks on its request.
  assert_eq!(sim::request_states(), vec![RequestState::Pending]);
  assert_eq!(mesh.tick(), 0); // The owner ends.
  let finished = finished_by_wire(&mut mesh);
  assert_eq!(finished["child"], Outcome::Cancelled);
  assert_eq!(sim::request_states(), vec![RequestState::Aborted]);
}

/// Any number of instances wait on one; a waiter that goes away removes
/// its registration; a stop reaches every waiter as a distinct error.
#[test]
fn waiters_share_one_instance_and_leave_cleanly() {
  for mode in [WakeMode::PollEveryTick, WakeMode::OnNotify] {
    let (mut mesh, handle) = mesh_with_sleeper(vec![
      wire("a", false, vec![wait("w")]),
      wire("b", false, vec![wait("w")]),
      wire("gone", false, vec![wait("w")]),
    ]);
    mesh.set_wake_mode(mode);
    let ids: Vec<_> = ["a", "b", "gone"]
      .into_iter()
      .map(|w| start(&mut mesh, w))
      .collect();
    mesh.tick();
    mesh.tick();
    assert_eq!(completion(&handle).waiter_count(), 3);
    mesh.cancel(ids[2]);
    assert_eq!(completion(&handle).waiter_count(), 2, "{mode:?}");
    // The host stops the sleeper through its handle.
    completion(&handle).request(Request::Stop);
    mesh.tick();
    assert_eq!(completion(&handle).outcome(), Some(Outcome::Stopped));
    mesh.tick();
    let finished = finished_by_wire(&mut mesh);
    for w in ["a", "b"] {
      assert_eq!(failure_code(&finished[w]), "wire-stopped", "{w} {mode:?}");
    }
    assert_eq!(finished["gone"], Outcome::Cancelled);
    assert_eq!(mesh.running(), 0);
  }
}

/// What Wait, Peek and IsRunning see for every way an instance ends:
/// its value, its error, a stop, a cancellation, and a cleanup that
/// panicked (the outcome is published after cleanup, so it is the panic).
#[test]
fn wait_and_peek_see_every_ending() {
  let divide_by_zero =
    otter_core::ShardDef::new(&otter_core::shards::math::DIVIDE, vec![val(Var::Int(0))]);
  #[allow(unused_mut)]
  let mut cases: Vec<(&str, Vec<otter_core::ShardDef>, bool)> = vec![
    ("completes", vec![pause(), konst(Var::Int(7))], false),
    (
      "fails",
      vec![pause(), konst(Var::Int(1)), divide_by_zero],
      false,
    ),
    ("stopped", vec![pause_secs(100.0)], true),
  ];
  // Where panics unwind (not on WASI, which aborts).
  #[cfg(panic = "unwind")]
  cases.push((
    "stuck",
    vec![probe_mode("c", "panic-cleanup"), pause_secs(100.0)],
    true,
  ));
  for (name, flow, stop) in cases {
    let mut mesh = Mesh::new();
    mesh.add_wire(wire("target", false, flow));
    let mut waiter = vec![
      spawn("target"),
      declare("t"),
      is_running("t"),
      declare("before"),
    ];
    if stop {
      // Once the target has run (and has state to clean up): it starts at
      // the end of this tick and first runs in the next, after the waiter.
      waiter.push(pause());
      waiter.push(pause());
      waiter.push(stop_instance("t"));
    }
    waiter.push(wait("t"));
    waiter.push(declare("waited"));
    waiter.push(is_running("t"));
    waiter.push(declare("after"));
    waiter.push(peek("t"));
    mesh.add_wire(wire("waiter", false, waiter));
    let id = start(&mut mesh, "waiter");
    mesh.run(10);
    let outcome = mesh.take_outcome(id).unwrap();
    match name {
      "completes" => assert_eq!(outcome, Outcome::Completed(Var::Int(7))),
      "fails" => assert!(
        matches!(&outcome, Outcome::Failed(e) if e.to_string().contains("zero")),
        "{outcome:?}"
      ),
      "stopped" => assert_eq!(failure_code(&outcome), "wire-stopped"),
      _ => assert!(
        matches!(&outcome, Outcome::Failed(e) if e.to_string().contains("panic in cleanup")),
        "{outcome:?}"
      ),
    }
  }
}

/// A cancelled target fails Wait with its own code; Peek reads none while
/// it runs.
#[test]
fn waiting_on_a_cancelled_instance_is_a_distinct_error() {
  let (mut mesh, handle) = mesh_with_sleeper(vec![
    wire("peeker", false, vec![peek("w")]),
    wire("waiter", false, vec![wait("w")]),
  ]);
  let peeker = start(&mut mesh, "peeker");
  let waiter = start(&mut mesh, "waiter");
  mesh.tick();
  assert_eq!(
    mesh.take_outcome(peeker),
    Some(Outcome::Completed(Var::None))
  );
  let sleeper = mesh.instance_ids()[0];
  mesh.cancel(sleeper);
  assert_eq!(completion(&handle).outcome(), Some(Outcome::Cancelled));
  mesh.tick();
  assert_eq!(
    failure_code(&mesh.take_outcome(waiter).unwrap()),
    "wire-cancelled"
  );
}

/// A timeout fails Wait once it passes, under either wake mode (a timed
/// wait is resumed every tick, not only when woken).
#[test]
fn wait_times_out() {
  for mode in [WakeMode::PollEveryTick, WakeMode::OnNotify] {
    let (mut mesh, handle) =
      mesh_with_sleeper(vec![wire("waiter", false, vec![wait_timeout("w", 0.0)])]);
    mesh.set_wake_mode(mode);
    let waiter = start(&mut mesh, "waiter");
    mesh.tick();
    mesh.tick();
    assert_eq!(
      failure_code(&mesh.take_outcome(waiter).unwrap()),
      "wait-timeout",
      "{mode:?}"
    );
    assert_eq!(completion(&handle).waiter_count(), 0);
    assert!(!completion(&handle).is_finished());
  }
}

/// A looped owner's iterations do not end its children, and the children
/// it keeps are only the running ones.
#[test]
fn a_looped_owner_keeps_only_running_children() {
  let mut mesh = Mesh::new();
  mesh.add_wire(wire("quick", false, vec![konst(Var::Int(1))]));
  mesh.add_wire(wire("sleeper", true, vec![pause()]));
  mesh.add_wire(wire(
    "owner",
    true,
    vec![once(vec![spawn("sleeper")]), spawn("quick"), pause()],
  ));
  let owner = start(&mut mesh, "owner");
  for _ in 0..50 {
    mesh.tick();
    mesh.take_finished();
  }
  let handle = mesh.handle(owner).unwrap();
  // The sleeper, plus at most the quick child of this iteration and the
  // last.
  assert!(completion(&handle).child_count() <= 3);
  assert_eq!(mesh.running(), 2, "the owner and its sleeper");
}

/// Handles held by the host or in mesh variables stay valid: across a
/// preserving reload they keep naming the original instance (retained:
/// still running; cancelled by the reload: Cancelled), and dropping the
/// mesh cancels what still runs.
#[test]
fn handles_survive_reload_and_mesh_destruction() {
  let mut mesh = Mesh::new();
  mesh.add_wire(wire("kept", true, vec![pause()]));
  mesh.add_wire(wire("replaced", true, vec![pause()]));
  let kept = start(&mut mesh, "kept");
  let replaced = start(&mut mesh, "replaced");
  let (kept_handle, replaced_handle) = (mesh.handle(kept).unwrap(), mesh.handle(replaced).unwrap());
  mesh.declare_var("held", replaced_handle.clone(), false);
  mesh.tick();

  let mut next = mesh.revision();
  next.add_wire(wire("kept", true, vec![pause()]));
  next.add_wire(wire("replaced", true, vec![pause(), pause()]));
  let removed = [replaced].into_iter().collect();
  mesh.validate_reload(&mut next, &removed).unwrap();
  mesh.install_revision(next, &removed);
  assert!(!completion(&kept_handle).is_finished());
  assert_eq!(
    completion(&replaced_handle).outcome(),
    Some(Outcome::Cancelled)
  );
  let held = mesh.get_var("held").unwrap();
  assert!(std::sync::Arc::ptr_eq(
    completion(&held),
    completion(&replaced_handle)
  ));
  mesh.take_finished();
  mesh.tick();
  assert!(!completion(&kept_handle).is_finished());

  drop(mesh);
  assert_eq!(completion(&kept_handle).outcome(), Some(Outcome::Cancelled));
}

/// An instance cannot wait for itself.
#[test]
fn waiting_for_oneself_is_an_error() {
  let (mut mesh, _) = mesh_with_sleeper(vec![wire("selfish", false, vec![pause(), wait("w")])]);
  let id = start(&mut mesh, "selfish");
  mesh.set_var("w", mesh.handle(id).unwrap());
  mesh.run(5);
  let outcome = mesh.take_outcome(id).unwrap();
  assert!(
    matches!(&outcome, Outcome::Failed(e) if e.to_string().contains("cannot wait for itself")),
    "{outcome:?}"
  );
}
