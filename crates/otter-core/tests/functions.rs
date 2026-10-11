//! Script functions at the core level (golden path §3 to §6, tests A to D,
//! G and I): shared bodies, fresh locals, components, scope, mesh access and
//! purity, through the definition helpers. The source syntax is covered by
//! the frontend suite.

use otter_core::shards::defs::*;
use otter_core::shards::{ProbeEventKind, take_probe_events};
use otter_core::{Arg, FunctionDef, Mesh, Outcome, ParamValue, ShardDef, Type, Var, WireDef};

fn wire(name: &str, looped: bool, flow: Vec<ShardDef>) -> WireDef {
  WireDef {
    name: name.into(),
    looped,
    flow,
  }
}

fn named(name: &str, value: ParamValue) -> Arg {
  Arg::named(name, value)
}

/// Runs `root` (non-looped) to completion and returns its outcome and the
/// log lines.
fn run_logging(mesh: &mut Mesh, ticks: usize) -> (Outcome, Vec<String>) {
  let root = mesh.compile("root", Type::none()).unwrap();
  let id = mesh.spawn(&root, Var::None).unwrap();
  let ((), lines) = otter_core::log::capture(|| {
    mesh.run(ticks);
  });
  (mesh.take_outcome(id).expect("finished"), lines)
}

fn compile_error(mesh: &mut Mesh) -> otter_core::Diagnostic {
  let err = mesh
    .compile("root", Type::none())
    .err()
    .expect("compose error");
  err.diagnostic().cloned().expect("structured diagnostic")
}

/// `@fn(Scale input: Float output: Float params: {factor: Float} { Math.Multiply(factor) })`.
fn scale() -> FunctionDef {
  FunctionDef::new("Scale", Type::float(), Type::float())
    .param("factor", Type::float())
    .body(vec![ShardDef::new(
      &otter_core::shards::math::MULTIPLY,
      vec![var("factor")],
    )])
}

// A. Shared body, runtime parameters.
#[test]
fn a_call_site_shares_one_body_and_passes_runtime_arguments() {
  let mut mesh = Mesh::new();
  mesh.add_function(scale());
  mesh.add_wire(wire(
    "root",
    false,
    vec![
      konst(Var::Float(3.0)),
      call("Scale", vec![named("factor", val(Var::Float(2.0)))]),
      log(),
      konst(Var::Float(3.0)),
      call("Scale", vec![named("factor", val(Var::Float(4.0)))]),
      log(),
    ],
  ));
  let (outcome, lines) = run_logging(&mut mesh, 10);
  assert_eq!(outcome, Outcome::Completed(Var::Float(12.0)));
  assert_eq!(lines, ["6", "12"]);
  // One compose of Scale for two call sites; a different runtime argument
  // composes nothing.
  assert_eq!(mesh.cache_stats().function_composes, 1);
}

// B. No caller capture.
#[test]
fn a_function_does_not_see_the_callers_locals() {
  let mut mesh = Mesh::new();
  mesh
    .add_function(FunctionDef::new("Bad", Type::int(), Type::int()).body(vec![add(var("secret"))]));
  mesh.add_wire(wire(
    "root",
    false,
    vec![konst(Var::Int(10)), bind("secret"), call("Bad", vec![])],
  ));
  let d = compile_error(&mut mesh);
  assert_eq!(d.code, "unknown-variable");
  assert_eq!(d.shard.as_deref(), Some("Math.Add"));
  assert!(
    d.message.contains("pass it as a parameter"),
    "{}",
    d.message
  );
  assert!(d.message.contains("params: {secret: Int}"), "{}", d.message);
  assert_eq!(
    d.path,
    vec![
      otter_core::diagnostic::PathStep::Wire("root".into()),
      otter_core::diagnostic::PathStep::Shard {
        index: 2,
        name: "Bad".into()
      },
      otter_core::diagnostic::PathStep::Function("Bad".into()),
      otter_core::diagnostic::PathStep::Shard {
        index: 0,
        name: "Math.Add".into()
      },
    ]
  );
}

// C. Fresh locals, and init/cleanup once per invocation.
#[test]
fn a_stateless_function_starts_with_fresh_locals_each_invocation() {
  take_probe_events();
  let mut mesh = Mesh::new();
  mesh.add_function(
    FunctionDef::new("Count", Type::none(), Type::int()).body(vec![
      probe("inner"),
      konst(Var::Int(0)),
      declare("n"),
      pause(),
      get("n"),
      add(val(Var::Int(1))),
      update("n"),
      get("n"),
    ]),
  );
  mesh.add_wire(wire(
    "root",
    false,
    vec![repeat(vec![call("Count", vec![]), log()], val(Var::Int(3)))],
  ));
  let (outcome, lines) = run_logging(&mut mesh, 20);
  assert_eq!(outcome, Outcome::Completed(Var::None));
  assert_eq!(lines, ["1", "1", "1"]);
  let events = take_probe_events();
  let count = |kind| events.iter().filter(|e| e.kind == kind).count();
  // Three invocations, each suspended once: instantiated and cleaned up
  // three times, not six.
  assert_eq!(count(ProbeEventKind::Instantiate), 3);
  assert_eq!(count(ProbeEventKind::Cleanup), 3);
  assert_eq!(count(ProbeEventKind::Activate), 3);
}

// D. Components.
#[test]
fn a_stateful_function_keeps_one_instance_per_call_site() {
  let mut mesh = Mesh::new();
  mesh.add_function(
    FunctionDef::new("Counter", Type::none(), Type::int())
      .param("step", Type::int())
      .stateful()
      .body(vec![
        keep("n", Var::Int(0)),
        get("n"),
        add(var("step")),
        update("n"),
      ]),
  );
  mesh.add_wire(wire(
    "root",
    true,
    vec![
      call("Counter", vec![named("step", val(Var::Int(1)))]),
      log(),
      call("Counter", vec![named("step", val(Var::Int(10)))]),
      log(),
    ],
  ));
  let root = mesh.compile("root", Type::none()).unwrap();
  let id = mesh.spawn(&root, Var::None).unwrap();
  let ((), lines) = otter_core::log::capture(|| {
    mesh.tick();
    mesh.tick();
  });
  assert_eq!(lines, ["1", "10", "2", "20"]);
  assert_eq!(mesh.cache_stats().function_composes, 1);
  mesh.cancel(id);
  assert_eq!(mesh.take_outcome(id), Some(Outcome::Cancelled));
}

#[test]
fn keep_once_and_stateful_calls_need_a_stateful_owner() {
  let mut mesh = Mesh::new();
  mesh.add_function(
    FunctionDef::new("Counter", Type::none(), Type::int())
      .body(vec![keep("n", Var::Int(0)), get("n")]),
  );
  mesh.add_wire(wire("root", false, vec![call("Counter", vec![])]));
  let d = compile_error(&mut mesh);
  assert_eq!(
    (d.code, d.shard.as_deref()),
    ("keep-in-stateless", Some("Keep"))
  );
  assert!(d.message.contains("stateful: true"), "{}", d.message);

  let mut mesh = Mesh::new();
  mesh.add_function(
    FunctionDef::new("Setup", Type::none(), Type::none())
      .body(vec![once(vec![konst(Var::Int(1))])]),
  );
  mesh.add_wire(wire("root", false, vec![call("Setup", vec![])]));
  assert_eq!(compile_error(&mut mesh).code, "once-in-stateless");

  let mut mesh = Mesh::new();
  mesh.add_function(
    FunctionDef::new("Component", Type::none(), Type::none())
      .stateful()
      .body(vec![keep("n", Var::Int(0))]),
  );
  mesh.add_function(
    FunctionDef::new("Plain", Type::none(), Type::none()).body(vec![call("Component", vec![])]),
  );
  mesh.add_wire(wire("root", false, vec![call("Plain", vec![])]));
  let d = compile_error(&mut mesh);
  assert_eq!(
    (d.code, d.shard.as_deref()),
    ("stateful-call-in-stateless", Some("Component"))
  );
  // A stateful caller may own it.
  let mut mesh = Mesh::new();
  mesh.add_function(
    FunctionDef::new("Component", Type::none(), Type::int())
      .stateful()
      .body(vec![keep("n", Var::Int(0)), inc("n")]),
  );
  mesh.add_function(
    FunctionDef::new("Owner", Type::none(), Type::int())
      .stateful()
      .body(vec![call("Component", vec![])]),
  );
  mesh.add_wire(wire(
    "root",
    false,
    vec![
      call("Owner", vec![]),
      call("Owner", vec![]),
      call("Owner", vec![]),
    ],
  ));
  let (outcome, _) = run_logging(&mut mesh, 10);
  // One call site of Owner at a time: the third Owner call site is its own
  // component, so each site counts from 1.
  assert_eq!(outcome, Outcome::Completed(Var::Int(1)));
}

#[test]
fn a_component_at_one_site_counts_across_iterations_in_a_looped_owner() {
  let mut mesh = Mesh::new();
  mesh.add_function(
    FunctionDef::new("Component", Type::none(), Type::int())
      .stateful()
      .body(vec![keep("n", Var::Int(0)), inc("n")]),
  );
  mesh.add_function(
    FunctionDef::new("Owner", Type::none(), Type::int())
      .stateful()
      .body(vec![call("Component", vec![])]),
  );
  mesh.add_wire(wire("root", true, vec![call("Owner", vec![]), log()]));
  let root = mesh.compile("root", Type::none()).unwrap();
  mesh.spawn(&root, Var::None).unwrap();
  let ((), lines) = otter_core::log::capture(|| {
    mesh.run(3);
  });
  assert_eq!(lines, ["1", "2", "3"]);
}

// G. Stable parameters across suspension.
#[test]
fn parameters_are_snapshots_across_suspension() {
  let mut mesh = Mesh::new();
  mesh.declare_var("gain", Var::Int(2), true);
  mesh.add_function(
    FunctionDef::new("Later", Type::int(), Type::int())
      .param("amount", Type::int())
      .body(vec![pause(), add(var("amount"))]),
  );
  mesh.add_function(
    FunctionDef::new("Apply", Type::int(), Type::int())
      .uses(&["gain"])
      .body(vec![call("Later", vec![named("amount", var("gain"))])]),
  );
  mesh.add_wire(wire(
    "root",
    false,
    vec![konst(Var::Int(3)), call("Apply", vec![])],
  ));
  let root = mesh.compile("root", Type::none()).unwrap();
  let id = mesh.spawn(&root, Var::None).unwrap();
  mesh.tick(); // Suspended inside Later with amount = 2.
  assert!(mesh.outcome(id).is_none());
  mesh.set_var("gain", Var::Int(100));
  mesh.run(5);
  assert_eq!(mesh.take_outcome(id), Some(Outcome::Completed(Var::Int(5))));
}

// I. Effects and mesh access.
#[test]
fn mesh_access_is_declared_never_inferred() {
  // A read without `uses`.
  let mut mesh = Mesh::new();
  mesh.declare_var("gain", Var::Int(2), true);
  mesh
    .add_function(FunctionDef::new("Read", Type::int(), Type::int()).body(vec![add(var("gain"))]));
  mesh.add_wire(wire(
    "root",
    false,
    vec![konst(Var::Int(1)), call("Read", vec![])],
  ));
  let d = compile_error(&mut mesh);
  assert_eq!(d.code, "undeclared-mesh-access");
  assert!(d.message.contains("uses: [gain]"), "{}", d.message);

  // A write with `uses` only, and a read with `mutates` only.
  let mut mesh = Mesh::new();
  mesh.declare_var("count", Var::Int(0), true);
  mesh.add_function(
    FunctionDef::new("Write", Type::int(), Type::int())
      .uses(&["count"])
      .body(vec![update("count")]),
  );
  mesh.add_wire(wire(
    "root",
    false,
    vec![konst(Var::Int(1)), call("Write", vec![])],
  ));
  let d = compile_error(&mut mesh);
  assert_eq!(
    (d.code, d.shard.as_deref()),
    ("undeclared-mesh-access", Some("Update"))
  );
  assert!(d.message.contains("mutates: [count]"), "{}", d.message);
  let mut mesh = Mesh::new();
  mesh.declare_var("count", Var::Int(0), true);
  mesh.add_function(
    FunctionDef::new("Bump", Type::none(), Type::int())
      .mutates(&["count"])
      .body(vec![inc("count")]),
  );
  mesh.add_wire(wire("root", false, vec![call("Bump", vec![])]));
  let d = compile_error(&mut mesh);
  assert_eq!(d.code, "undeclared-mesh-access");
  assert!(d.message.contains("uses: [count]"), "{}", d.message);

  // Indirect access through another function must be declared too.
  let mut mesh = Mesh::new();
  mesh.declare_var("gain", Var::Int(2), true);
  mesh.add_function(
    FunctionDef::new("Read", Type::int(), Type::int())
      .uses(&["gain"])
      .body(vec![add(var("gain"))]),
  );
  mesh.add_function(
    FunctionDef::new("Outer", Type::int(), Type::int()).body(vec![call("Read", vec![])]),
  );
  mesh.add_wire(wire(
    "root",
    false,
    vec![konst(Var::Int(1)), call("Outer", vec![])],
  ));
  let d = compile_error(&mut mesh);
  assert_eq!(
    (d.code, d.shard.as_deref()),
    ("undeclared-mesh-access", Some("Read"))
  );
  assert!(d.message.contains("Outer must declare"), "{}", d.message);

  // Declared at every level: reads, writes and read-modify-write run, and
  // the wire's inferred access reports them.
  let mut mesh = Mesh::new();
  mesh.declare_var("count", Var::Int(0), true);
  mesh.add_function(
    FunctionDef::new("Bump", Type::none(), Type::int())
      .uses(&["count"])
      .mutates(&["count"])
      .body(vec![inc("count")]),
  );
  mesh.add_function(
    FunctionDef::new("Outer", Type::none(), Type::int())
      .uses(&["count"])
      .mutates(&["count"])
      .body(vec![call("Bump", vec![])]),
  );
  mesh.add_wire(wire(
    "root",
    false,
    vec![call("Outer", vec![]), call("Outer", vec![])],
  ));
  let root = mesh.compile("root", Type::none()).unwrap();
  assert_eq!(
    root
      .flow
      .analysis
      .mutates
      .iter()
      .map(|a| a.name.as_str())
      .collect::<Vec<_>>(),
    ["count"]
  );
  let (outcome, _) = run_logging(&mut mesh, 10);
  assert_eq!(outcome, Outcome::Completed(Var::Int(2)));
  assert_eq!(mesh.get_var("count"), Some(Var::Int(2)));
}

#[test]
fn purity_is_a_checked_contract() {
  let mut mesh = Mesh::new();
  mesh.add_function(
    FunctionDef::new("Now", Type::none(), Type::float())
      .pure()
      .body(vec![ShardDef::new(
        &otter_core::shards::values::TIME_NOW,
        vec![],
      )]),
  );
  mesh.add_wire(wire("root", false, vec![call("Now", vec![])]));
  let d = compile_error(&mut mesh);
  assert_eq!((d.code, d.shard.as_deref()), ("not-pure", Some("Now")));
  assert!(
    d.message.contains("Time.Now has the effect `time`"),
    "{}",
    d.message
  );

  // An undescribed host shard has unknown effects: never pure.
  let mut mesh = Mesh::new();
  mesh.add_function(
    FunctionDef::new("Host", Type::none(), Type::none())
      .pure()
      .body(vec![ShardDef::new(&UNKNOWN_EFFECTS, vec![])]),
  );
  mesh.add_wire(wire("root", false, vec![call("Host", vec![])]));
  let d = compile_error(&mut mesh);
  assert_eq!(d.code, "not-pure");
  assert!(d.message.contains("`unknown`"), "{}", d.message);

  // Suspension and mesh access are not pure either; arithmetic is.
  let mut mesh = Mesh::new();
  mesh.add_function(
    FunctionDef::new("Wait", Type::none(), Type::none())
      .pure()
      .body(vec![pause()]),
  );
  mesh.add_wire(wire("root", false, vec![call("Wait", vec![])]));
  assert!(compile_error(&mut mesh).message.contains("`suspends`"));
  let mut mesh = Mesh::new();
  mesh.declare_var("gain", Var::Int(2), true);
  mesh.add_function(
    FunctionDef::new("Read", Type::int(), Type::int())
      .pure()
      .uses(&["gain"])
      .body(vec![add(var("gain"))]),
  );
  mesh.add_wire(wire(
    "root",
    false,
    vec![konst(Var::Int(1)), call("Read", vec![])],
  ));
  assert!(
    compile_error(&mut mesh)
      .message
      .contains("reads mesh variable gain")
  );
  let mut mesh = Mesh::new();
  mesh.add_function(scale().pure());
  mesh.add_wire(wire(
    "root",
    false,
    vec![
      konst(Var::Float(1.5)),
      call("Scale", vec![named("factor", val(Var::Float(2.0)))]),
    ],
  ));
  assert_eq!(
    run_logging(&mut mesh, 5).0,
    Outcome::Completed(Var::Float(3.0))
  );
}

/// A host shard that declares nothing: unknown effects.
static UNKNOWN_EFFECTS: otter_core::ShardType = otter_core::shards::leaf::leaf_type::<Undeclared>();
struct Undeclared;
impl otter_core::shards::leaf::LeafShard for Undeclared {
  type Compiled = ();
  type State = ();
  const DESC: otter_core::ShardDesc = otter_core::ShardDesc::undocumented("Host.Undeclared", 1);
  fn compose(
    _: &otter_core::Args,
    ctx: &mut otter_core::ComposeCtx<'_>,
  ) -> otter_core::Result<otter_core::Composed<()>> {
    Ok(otter_core::Composed {
      compiled: (),
      output: ctx.input(),
    })
  }
  fn instantiate(_: &(), _: &mut otter_core::instance::InstanceCtx) -> otter_core::Result<()> {
    Ok(())
  }
  fn activate(
    _: &(),
    _: &mut (),
    _: &mut impl otter_core::instance::LeafCtx,
    input: &Var,
  ) -> otter_core::Result<otter_core::Flow> {
    Ok(otter_core::Flow::Next(input.clone()))
  }
}

#[test]
fn arguments_are_checked_against_the_signature() {
  let cases: Vec<(Vec<Arg>, &str)> = vec![
    (
      vec![named("factro", val(Var::Float(2.0)))],
      "unknown-argument",
    ),
    (vec![], "missing-argument"),
    (
      vec![named("factor", val(Var::Int(2)))],
      "wrong-argument-type",
    ),
    (
      vec![
        named("factor", val(Var::Float(2.0))),
        named("factor", val(Var::Float(2.0))),
      ],
      "duplicate-argument",
    ),
    (
      vec![
        Arg::pos(val(Var::Float(2.0))),
        Arg::pos(val(Var::Float(2.0))),
      ],
      "too-many-arguments",
    ),
    (
      vec![named("factor", ParamValue::Flow(vec![]))],
      "wrong-argument-form",
    ),
  ];
  for (args, code) in cases {
    let mut mesh = Mesh::new();
    mesh.add_function(scale());
    mesh.add_wire(wire(
      "root",
      false,
      vec![konst(Var::Float(3.0)), call("Scale", args)],
    ));
    let d = compile_error(&mut mesh);
    assert_eq!((d.code, d.shard.as_deref()), (code, Some("Scale")));
  }
  // The input type, the output type and an unknown function.
  let mut mesh = Mesh::new();
  mesh.add_function(scale());
  mesh.add_wire(wire(
    "root",
    false,
    vec![
      konst(Var::Int(3)),
      call("Scale", vec![Arg::pos(val(Var::Float(2.0)))]),
    ],
  ));
  let d = compile_error(&mut mesh);
  assert_eq!(
    (d.kind, d.shard.as_deref()),
    ("input-type-mismatch", Some("Scale"))
  );
  let mut mesh = Mesh::new();
  mesh.add_function(
    FunctionDef::new("Wrong", Type::none(), Type::int()).body(vec![konst(Var::string("x"))]),
  );
  mesh.add_wire(wire("root", false, vec![call("Wrong", vec![])]));
  let d = compile_error(&mut mesh);
  assert_eq!(
    (d.code, d.shard.as_deref()),
    ("output-type-mismatch", Some("Wrong"))
  );
  let mut mesh = Mesh::new();
  mesh.add_wire(wire("root", false, vec![call("Nope", vec![])]));
  assert_eq!(compile_error(&mut mesh).code, "unknown-function");
  // Defaults are typed literals; positional arguments follow the order.
  let mut mesh = Mesh::new();
  mesh.add_function(
    FunctionDef::new("Offset", Type::int(), Type::int())
      .param("by", Type::int())
      .param_default("times", Var::Int(1))
      .body(vec![
        add(var("by")),
        ShardDef::new(&otter_core::shards::math::MULTIPLY, vec![var("times")]),
      ]),
  );
  mesh.add_wire(wire(
    "root",
    false,
    vec![
      konst(Var::Int(1)),
      call("Offset", vec![Arg::pos(val(Var::Int(2)))]),
      call(
        "Offset",
        vec![Arg::pos(val(Var::Int(2))), Arg::pos(val(Var::Int(10)))],
      ),
    ],
  ));
  assert_eq!(
    run_logging(&mut mesh, 5).0,
    Outcome::Completed(Var::Int(50))
  );
}

#[test]
fn return_exits_the_nearest_function_and_input_is_the_entry_value() {
  let mut mesh = Mesh::new();
  mesh.add_function(
    FunctionDef::new("Early", Type::int(), Type::int()).body(vec![
      add(val(Var::Int(1))),
      when(
        vec![is_more_equal(val(Var::Int(10)))],
        vec![get("input"), return_()],
      ),
      add(val(Var::Int(100))),
    ]),
  );
  mesh.add_wire(wire(
    "root",
    false,
    vec![
      konst(Var::Int(1)),
      call("Early", vec![]),
      log(),
      konst(Var::Int(20)),
      call("Early", vec![]),
      log(),
    ],
  ));
  let (outcome, lines) = run_logging(&mut mesh, 10);
  assert_eq!(lines, ["102", "20"]);
  assert_eq!(outcome, Outcome::Completed(Var::Int(20)));
  // Return's value must fit the declared output.
  let mut mesh = Mesh::new();
  mesh.add_function(
    FunctionDef::new("Bad", Type::int(), Type::int())
      .body(vec![konst(Var::string("x")), return_()]),
  );
  mesh.add_wire(wire(
    "root",
    false,
    vec![konst(Var::Int(1)), call("Bad", vec![])],
  ));
  let d = compile_error(&mut mesh);
  assert_eq!(
    (d.code, d.shard.as_deref()),
    ("return-type-mismatch", Some("Return"))
  );
}

#[test]
fn return_at_a_looped_root_ends_the_iteration() {
  let mut mesh = Mesh::new();
  mesh.add_wire(wire(
    "root",
    true,
    vec![
      keep("n", Var::Int(0)),
      inc("n"),
      log(),
      return_(),
      konst(Var::string("never")),
      log(),
    ],
  ));
  let root = mesh.compile("root", Type::none()).unwrap();
  let id = mesh.spawn(&root, Var::None).unwrap();
  let ((), lines) = otter_core::log::capture(|| {
    mesh.run(2);
  });
  assert_eq!(lines, ["1", "2"]);
  assert!(mesh.outcome(id).is_none());
  mesh.add_wire(wire(
    "once",
    false,
    vec![konst(Var::Int(7)), return_(), konst(Var::Int(8))],
  ));
  let once = mesh.compile("once", Type::none()).unwrap();
  let id = mesh.spawn(&once, Var::None).unwrap();
  mesh.run(2);
  assert_eq!(mesh.take_outcome(id), Some(Outcome::Completed(Var::Int(7))));
}

#[test]
fn stateless_functions_recurse_directly_and_mutually() {
  use otter_core::shards::math::{MULTIPLY, SUBTRACT};
  use otter_core::shards::values::IS;
  // Factorial: `If({IsLess(2)} {1} {input | Sub(1) | Fact | Multiply(input)})`.
  let mut mesh = Mesh::new();
  mesh.add_function(
    FunctionDef::new("Fact", Type::int(), Type::int()).body(vec![if_(
      vec![is_less(val(Var::Int(2)))],
      vec![konst(Var::Int(1))],
      Some(vec![
        ShardDef::new(&SUBTRACT, vec![val(Var::Int(1))]),
        call("Fact", vec![]),
        ShardDef::new(&MULTIPLY, vec![var("input")]),
      ]),
    )]),
  );
  mesh.add_wire(wire(
    "root",
    false,
    vec![konst(Var::Int(5)), call("Fact", vec![])],
  ));
  assert_eq!(
    run_logging(&mut mesh, 5).0,
    Outcome::Completed(Var::Int(120))
  );
  assert_eq!(
    mesh.cache_stats().function_composes,
    2,
    "one first pass, one final"
  );

  // Mutual recursion, with a suspension inside one member.
  let mut mesh = Mesh::new();
  mesh.add_function(
    FunctionDef::new("Even", Type::int(), Type::bool()).body(vec![if_(
      vec![ShardDef::new(&IS, vec![val(Var::Int(0))])],
      vec![konst(Var::Bool(true))],
      Some(vec![
        ShardDef::new(&SUBTRACT, vec![val(Var::Int(1))]),
        call("Odd", vec![]),
      ]),
    )]),
  );
  mesh.add_function(
    FunctionDef::new("Odd", Type::int(), Type::bool()).body(vec![
      pause(),
      if_(
        vec![ShardDef::new(&IS, vec![val(Var::Int(0))])],
        vec![konst(Var::Bool(false))],
        Some(vec![
          ShardDef::new(&SUBTRACT, vec![val(Var::Int(1))]),
          call("Even", vec![]),
        ]),
      ),
    ]),
  );
  mesh.add_wire(wire(
    "root",
    false,
    vec![
      konst(Var::Int(7)),
      call("Even", vec![]),
      log(),
      konst(Var::Int(6)),
      call("Even", vec![]),
      log(),
    ],
  ));
  let (outcome, lines) = run_logging(&mut mesh, 50);
  assert_eq!(outcome, Outcome::Completed(Var::Bool(true)));
  assert_eq!(lines, ["false", "true"]);
  // Effects are the group's: Even suspends through Odd.
  let even = mesh.compile_function("Even").unwrap();
  assert!(even.flow.analysis.effects.suspends);
  let odd = mesh.compile_function("Odd").unwrap();
  assert!(odd.flow.analysis.effects.suspends);
  let mut mesh = Mesh::new();
  mesh.add_function(
    FunctionDef::new("Ping", Type::int(), Type::int())
      .pure()
      .body(vec![call("Pong", vec![])]),
  );
  mesh.add_function(
    FunctionDef::new("Pong", Type::int(), Type::int()).body(vec![
      pause(),
      if_(
        vec![is_less(val(Var::Int(1)))],
        vec![konst(Var::Int(0))],
        Some(vec![
          ShardDef::new(&SUBTRACT, vec![val(Var::Int(1))]),
          call("Ping", vec![]),
        ]),
      ),
    ]),
  );
  mesh.add_wire(wire(
    "root",
    false,
    vec![konst(Var::Int(3)), call("Ping", vec![])],
  ));
  let d = compile_error(&mut mesh);
  assert_eq!((d.code, d.shard.as_deref()), ("not-pure", Some("Ping")));
  assert!(d.message.contains("`suspends`"), "{}", d.message);
}

#[test]
fn recursion_hits_the_call_depth_limit_and_cleans_every_frame_once() {
  take_probe_events();
  let mut mesh = Mesh::new();
  mesh.set_max_call_depth(8);
  mesh.add_function(
    FunctionDef::new("Down", Type::int(), Type::int())
      .body(vec![probe("down"), call("Down", vec![])]),
  );
  mesh.add_wire(wire(
    "root",
    false,
    vec![konst(Var::Int(1)), call("Down", vec![])],
  ));
  let (outcome, _) = run_logging(&mut mesh, 5);
  let Outcome::Failed(err) = outcome else {
    panic!("expected the depth limit, got {outcome:?}");
  };
  let d = err.diagnostic().unwrap();
  assert_eq!(
    (d.code, d.shard.as_deref()),
    ("recursion-limit", Some("Down"))
  );
  assert!(d.message.contains("depth 8"), "{}", d.message);
  let events = take_probe_events();
  let count = |kind| events.iter().filter(|e| e.kind == kind).count();
  assert_eq!(count(ProbeEventKind::Instantiate), 8);
  assert_eq!(count(ProbeEventKind::Cleanup), 8);
}

#[test]
fn a_cycle_through_a_stateful_function_is_rejected() {
  let mut mesh = Mesh::new();
  mesh.add_function(
    FunctionDef::new("Even", Type::int(), Type::int())
      .stateful()
      .body(vec![call("Odd", vec![])]),
  );
  mesh.add_function(
    FunctionDef::new("Odd", Type::int(), Type::int())
      .stateful()
      .body(vec![call("Even", vec![])]),
  );
  mesh.add_wire(wire(
    "root",
    false,
    vec![konst(Var::Int(1)), call("Even", vec![])],
  ));
  let d = compile_error(&mut mesh);
  assert_eq!(
    (d.code, d.shard.as_deref()),
    ("recursive-stateful", Some("Even"))
  );
}

/// The limit counts invocation frames entered. A call inlined at compose
/// (a small straight-line body) enters none, so `C` activates through its
/// shard here to keep the chain on the frame path.
#[test]
fn the_call_depth_limit_applies_to_function_entries() {
  let mut mesh = Mesh::new();
  mesh.set_max_call_depth(2);
  mesh.add_function(
    FunctionDef::new("C", Type::int(), Type::int()).body(vec![add(val(Var::Int(1))), log()]),
  );
  mesh.add_function(FunctionDef::new("B", Type::int(), Type::int()).body(vec![call("C", vec![])]));
  mesh.add_function(FunctionDef::new("A", Type::int(), Type::int()).body(vec![call("B", vec![])]));
  mesh.add_wire(wire(
    "root",
    false,
    vec![konst(Var::Int(0)), call("A", vec![])],
  ));
  let (outcome, _) = run_logging(&mut mesh, 5);
  let Outcome::Failed(err) = outcome else {
    panic!("expected the depth limit, got {outcome:?}");
  };
  let d = err.diagnostic().unwrap();
  assert_eq!((d.code, d.shard.as_deref()), ("recursion-limit", Some("C")));
  mesh.set_max_call_depth(3);
  assert_eq!(run_logging(&mut mesh, 5).0, Outcome::Completed(Var::Int(1)));
}

#[test]
fn cancellation_cleans_an_in_flight_stateless_invocation_once() {
  take_probe_events();
  let mut mesh = Mesh::new();
  mesh.add_function(
    FunctionDef::new("Slow", Type::none(), Type::none())
      .body(vec![probe("inner"), pause_secs(10.0)]),
  );
  mesh.add_wire(wire(
    "root",
    false,
    vec![probe("outer"), call("Slow", vec![])],
  ));
  let root = mesh.compile("root", Type::none()).unwrap();
  let id = mesh.spawn(&root, Var::None).unwrap();
  mesh.tick();
  mesh.cancel(id);
  let events = take_probe_events();
  for tag in ["inner", "outer"] {
    assert_eq!(
      events
        .iter()
        .filter(|e| e.tag == tag && e.kind == ProbeEventKind::Cleanup)
        .count(),
      1,
      "{tag}"
    );
  }
  assert_eq!(mesh.take_outcome(id), Some(Outcome::Cancelled));
}

#[test]
fn a_failing_invocation_is_cleaned_and_maybe_catches_it() {
  take_probe_events();
  let mut mesh = Mesh::new();
  mesh.add_function(
    FunctionDef::new("Fails", Type::int(), Type::int()).body(vec![
      probe("inner"),
      konst(Var::from_seq(std::sync::Arc::new(vec![]))),
      take(val(Var::Int(3))),
      konst(Var::Int(1)),
    ]),
  );
  mesh.add_wire(wire(
    "root",
    true,
    vec![
      keep("n", Var::Int(0)),
      inc("n"),
      maybe(vec![call("Fails", vec![])], None),
      get("n"),
      log(),
    ],
  ));
  let root = mesh.compile("root", Type::none()).unwrap();
  mesh.spawn(&root, Var::None).unwrap();
  let ((), lines) = otter_core::log::capture(|| {
    mesh.run(2);
  });
  assert_eq!(lines, ["1", "2"]);
  let events = take_probe_events();
  let count = |kind| events.iter().filter(|e| e.kind == kind).count();
  assert_eq!(count(ProbeEventKind::Instantiate), 2);
  assert_eq!(count(ProbeEventKind::Cleanup), 2);
}

#[test]
fn uncalled_functions_can_be_checked_on_their_own() {
  let mut mesh = Mesh::new();
  mesh.add_function(scale());
  let compiled = mesh.compile_function("Scale").unwrap();
  assert_eq!(compiled.def.name, "Scale");
  let signature = compiled.signature();
  assert_eq!(signature.params.as_ref().map(Vec::len), Some(1));
  assert!(!signature.effects.suspends);
  mesh
    .add_function(FunctionDef::new("Bad", Type::int(), Type::int()).body(vec![add(var("secret"))]));
  let err = mesh.compile_function("Bad").err().expect("compose error");
  assert_eq!(err.diagnostic().unwrap().code, "unknown-variable");
}

// Golden path §3.4: a wire's ordinary locals are fresh per root iteration;
// only `Keep` slots survive.
#[test]
fn a_looped_wire_starts_each_iteration_with_fresh_locals() {
  let mut mesh = Mesh::new();
  mesh.add_wire(wire(
    "root",
    true,
    vec![
      keep("kept", Var::Int(1)),
      konst(Var::from_seq(std::sync::Arc::new(vec![Var::Int(5); 64]))),
      declare("scratch"),
      inc("kept"),
    ],
  ));
  let root = mesh.compile("root", Type::none()).unwrap();
  let id = mesh.spawn(&root, Var::None).unwrap();
  mesh.tick();
  let locals = mesh.instance_locals(id).unwrap().to_vec();
  assert_eq!(locals.len(), 2);
  assert_eq!(
    locals[0],
    Var::Int(2),
    "the Keep slot survives the iteration"
  );
  assert_eq!(locals[1], Var::None, "the scratch local is released");
  mesh.tick();
  assert_eq!(mesh.instance_locals(id).unwrap()[0], Var::Int(3));
  mesh.cancel(id);
}

/// A call to a small stateless straight-line body is inlined at compose:
/// the callee's code runs on hidden slots of the caller's frame, arguments
/// are bound by instructions, a callee's own inlined calls come along, and
/// nothing is entered. A body over the budget stays a VM call.
#[test]
fn small_straight_line_functions_are_inlined_into_their_callers() {
  let mut mesh = Mesh::new();
  mesh.add_function(
    FunctionDef::new("Scale", Type::int(), Type::int())
      .param("by", Type::int())
      .body(vec![
        ShardDef::new(&otter_core::shards::math::MULTIPLY, vec![var("by")]),
        add(val(Var::Int(1))),
      ]),
  );
  mesh.add_function(
    FunctionDef::new("Twice", Type::int(), Type::int()).body(vec![
      call("Scale", vec![Arg::named("by", val(Var::Int(2)))]),
      call("Scale", vec![Arg::named("by", val(Var::Int(2)))]),
    ]),
  );
  mesh.add_function(
    FunctionDef::new("Ten", Type::none(), Type::int()).body(vec![konst(Var::Int(10))]),
  );
  mesh.add_function(
    FunctionDef::new("Big", Type::int(), Type::int())
      .body((0..30).map(|_| add(val(Var::Int(1)))).collect()),
  );
  mesh.add_wire(wire(
    "root",
    false,
    vec![
      konst(Var::Int(3)),
      call("Twice", vec![]),
      declare("x"),
      call("Ten", vec![]),
      add(var("x")),
      log(),
    ],
  ));
  mesh.add_wire(wire(
    "big",
    false,
    vec![konst(Var::Int(0)), call("Big", vec![]), log()],
  ));
  let compiled = mesh.compile("root", Type::none()).unwrap();
  let kinds = compiled.flow.instruction_kinds();
  assert!(!kinds.contains(&"vm-call"), "{kinds:?}");
  assert_eq!(
    kinds.iter().filter(|k| **k == "fallback").count(),
    1,
    "only Log activates through the engine: {kinds:?}"
  );
  let big = mesh.compile("big", Type::none()).unwrap();
  assert!(big.flow.instruction_kinds().contains(&"vm-call"));
  let before = mesh.composite_dispatches();
  let (outcome, lines) = run_logging(&mut mesh, 3);
  // Twice: (3 * 2 + 1) * 2 + 1 = 15; Ten ignores its input; 10 + 15.
  assert_eq!(outcome, Outcome::Completed(Var::Int(25)));
  assert_eq!(lines, ["25"]);
  assert_eq!(mesh.composite_dispatches() - before, 0);
}

/// `When` and `While` are flat code with jumps: no composite step for the
/// predicate or the action, the input passed through, errors raised like
/// any VM error.
#[test]
fn straight_line_branches_run_inside_the_vm() {
  let mut mesh = Mesh::new();
  mesh.add_wire(wire(
    "root",
    false,
    vec![
      konst(Var::Int(0)),
      declare("n"),
      while_(vec![get("n"), is_less(val(Var::Int(10)))], vec![inc("n")]),
      get("n"),
      when(
        vec![is_more_equal(val(Var::Int(10)))],
        vec![konst(Var::Int(100)), update("n")],
      ),
      when(
        vec![is_less(val(Var::Int(0)))],
        vec![konst(Var::Int(-1)), update("n")],
      ),
      get("n"),
      log(),
      maybe(
        vec![when(
          vec![konst(Var::Bool(true))],
          vec![
            konst(Var::Int(1)),
            ShardDef::new(&otter_core::shards::math::DIVIDE, vec![val(Var::Int(0))]),
          ],
        )],
        Some(vec![konst(Var::Int(-1))]),
      ),
    ],
  ));
  let compiled = mesh.compile("root", Type::none()).unwrap();
  let kinds = compiled.flow.instruction_kinds();
  assert_eq!(
    kinds.iter().filter(|k| **k == "fallback").count(),
    2,
    "only Log and Maybe activate through the engine: {kinds:?}"
  );
  assert!(
    kinds.contains(&"jump-if-not") && kinds.contains(&"jump"),
    "{kinds:?}"
  );
  let before = mesh.composite_dispatches();
  let (outcome, lines) = run_logging(&mut mesh, 5);
  assert_eq!(outcome, Outcome::Completed(Var::Int(-1)));
  assert_eq!(lines, ["100"]);
  // Only Maybe takes composite steps (its entry and the caught failure).
  let steps = mesh.composite_dispatches() - before;
  assert!(steps <= 3, "{steps} composite steps");
}

/// A call to a body of straight-line VM code runs inside the caller's
/// step: one composite step per call, nothing entered, the same result and
/// the same error behavior as the frame path.
#[test]
fn a_straight_line_function_runs_inside_the_callers_step() {
  let mut mesh = Mesh::new();
  mesh.add_function(
    FunctionDef::new("Scale", Type::int(), Type::int())
      .param("by", Type::int())
      .body(vec![
        ShardDef::new(&otter_core::shards::math::MULTIPLY, vec![var("by")]),
        add(val(Var::Int(1))),
      ]),
  );
  mesh.add_function(
    FunctionDef::new("Halve", Type::int(), Type::int()).body(vec![ShardDef::new(
      &otter_core::shards::math::DIVIDE,
      vec![val(Var::Int(0))],
    )]),
  );
  mesh.add_wire(wire(
    "root",
    false,
    vec![
      konst(Var::Int(0)),
      declare("acc"),
      repeat(
        vec![
          get("acc"),
          call("Scale", vec![Arg::named("by", val(Var::Int(2)))]),
          update("acc"),
        ],
        val(Var::Int(10)),
      ),
      get("acc"),
      maybe(vec![call("Halve", vec![])], Some(vec![konst(Var::Int(-1))])),
      log(),
    ],
  ));
  let before = mesh.composite_dispatches();
  let (outcome, lines) = run_logging(&mut mesh, 5);
  // 0 -> 1 -> 3 -> 7 -> ... (2n + 1), ten times: 1023; dividing it by zero
  // fails and Maybe falls back.
  assert_eq!(outcome, Outcome::Completed(Var::Int(-1)));
  assert_eq!(lines, ["-1"]);
  // Repeat enters and completes its body (two steps per iteration); the
  // first Scale call takes the frame path (prepare, enter, complete), the
  // other nine one step each; Maybe and its failing call likewise.
  let steps = mesh.composite_dispatches() - before;
  assert!(steps < 60, "{steps} composite steps for ten calls");
}

/// How a call reaches its callee in `a_completed_call_releases_its_input`.
#[derive(Clone, Copy, Debug)]
enum CallPath {
  /// A small straight-line body, inlined at compose.
  Inlined,
  /// An inlined body that fails, caught by `Maybe`.
  InlinedFailing,
  /// A body that activates a shard: the call enters a frame.
  Framed,
  /// A straight-line body past the inlining budget, called twice: the
  /// second call runs inside the VM on the site's kept frame.
  Vm,
}

/// A completed call's values (its input, arguments and locals) end with
/// it, returned or failed: a collection the caller passed in is uniquely
/// owned by the caller again, so pushing to it is not a copy.
fn a_completed_call_releases_its_input(path: CallPath) {
  use std::sync::Arc;
  let mut mesh = Mesh::new();
  mesh.declare_var("acc", Var::from_seq(Arc::new(vec![Var::Int(7)])), true);
  let acc = mesh.get_var("acc");
  let Some(Var::Seq(items)) = &acc else {
    unreachable!()
  };
  let allocation = Arc::as_ptr(items);
  drop(acc);
  let index = if matches!(path, CallPath::InlinedFailing) {
    5
  } else {
    0
  };
  let mut body = vec![get("input"), take(val(Var::Int(index)))];
  match path {
    CallPath::Framed => body.push(log()),
    CallPath::Vm => body.extend((0..25).map(|_| add(val(Var::Int(0))))),
    CallPath::Inlined | CallPath::InlinedFailing => {}
  }
  mesh.add_function(FunctionDef::new("Read", Type::seq(Type::int()), Type::int()).body(body));
  let read = match path {
    CallPath::InlinedFailing => maybe(
      vec![get("acc"), call("Read", vec![])],
      Some(vec![konst(Var::Int(-1))]),
    ),
    CallPath::Vm => repeat(vec![get("acc"), call("Read", vec![])], val(Var::Int(2))),
    CallPath::Inlined | CallPath::Framed => call("Read", vec![]),
  };
  // The loop starts from no input, so its saved input holds nothing.
  let mut flow = match path {
    CallPath::Vm => vec![read],
    _ => vec![get("acc"), read],
  };
  flow.extend([
    konst(Var::Int(1)),
    ShardDef::new(&otter_core::shards::data::PUSH, vec![var("acc")]),
    pause(),
  ]);
  mesh.add_wire(wire("root", false, flow));
  let compiled = mesh.compile("root", Type::none()).unwrap();
  let id = mesh.spawn(&compiled, Var::None).unwrap();
  mesh.tick();
  assert_eq!(mesh.outcome(id), None, "{path:?}");
  let acc = mesh.get_var("acc");
  let Some(Var::Seq(items)) = &acc else {
    unreachable!()
  };
  assert_eq!(items.as_slice(), &[Var::Int(7), Var::Int(1)]);
  assert_eq!(
    Arc::as_ptr(items),
    allocation,
    "{path:?}: the completed call still owned the input"
  );
}

#[test]
fn a_completed_inlined_call_releases_its_input() {
  a_completed_call_releases_its_input(CallPath::Inlined);
}

#[test]
fn a_failed_inlined_call_releases_its_input() {
  a_completed_call_releases_its_input(CallPath::InlinedFailing);
}

#[test]
fn a_completed_framed_call_releases_its_input() {
  a_completed_call_releases_its_input(CallPath::Framed);
}

#[test]
fn a_completed_vm_call_releases_its_input() {
  a_completed_call_releases_its_input(CallPath::Vm);
}

/// A cached stateless invocation that fails to start (a leaf's instantiate
/// fails at its second entry) holds nothing: the leaves instantiated for
/// that attempt are cleaned up before `Maybe` takes over.
#[test]
fn a_failed_stateless_reentry_rolls_back_initialized_siblings() {
  use otter_core::args::Args;
  use otter_core::compose::ComposeCtx;
  use otter_core::describe::{InputDesc, OutputDesc, Params, ShardDesc, Targets};
  use otter_core::instance::{InstanceCtx, LeafCtx};
  use otter_core::shards::leaf::{LeafShard, leaf_type};
  use otter_core::{Composed, Flow, Result};
  use std::sync::atomic::{AtomicUsize, Ordering};
  static ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
  struct Flaky;
  impl LeafShard for Flaky {
    type Compiled = ();
    type State = ();
    const DESC: ShardDesc = ShardDesc {
      name: "Host.Flaky",
      version: 1,
      summary: "",
      help: "",
      params: Params::Declared(&[]),
      input: InputDesc::Any,
      output: OutputDesc::Passthrough,
      targets: Targets::All,
      aliases: &[],
      effects: otter_core::signature::Effects::UNKNOWN,
      lifetime: otter_core::signature::Lifetime::Stateful,
    };
    fn compose(_: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<()>> {
      Ok(Composed {
        compiled: (),
        output: ctx.input(),
      })
    }
    fn instantiate(_: &(), _: &mut InstanceCtx) -> Result<()> {
      if ATTEMPTS.fetch_add(1, Ordering::SeqCst) == 1 {
        Err(otter_core::Error::Activation("second entry failed".into()))
      } else {
        Ok(())
      }
    }
    fn activate(_: &(), _: &mut (), _: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
      Ok(Flow::Next(input.clone()))
    }
  }
  static FLAKY: otter_core::ShardType = leaf_type::<Flaky>();
  ATTEMPTS.store(0, Ordering::SeqCst);
  take_probe_events();
  let mut mesh = Mesh::new();
  mesh.add_function(
    FunctionDef::new("Work", Type::none(), Type::int()).body(vec![
      probe("resource"),
      ShardDef::new(&FLAKY, vec![]),
      konst(Var::Int(1)),
    ]),
  );
  mesh.add_wire(wire(
    "root",
    true,
    vec![
      maybe(vec![call("Work", vec![])], Some(vec![konst(Var::Int(-1))])),
      log(),
    ],
  ));
  let root = mesh.compile("root", Type::none()).unwrap();
  mesh.spawn(&root, Var::None).unwrap();
  let (_, lines) = otter_core::log::capture(|| {
    mesh.tick();
    mesh.tick();
  });
  assert_eq!(lines, ["1", "-1"]);
  let events = take_probe_events();
  let count = |kind| {
    events
      .iter()
      .filter(|e| e.tag == "resource" && e.kind == kind)
      .count()
  };
  assert_eq!(count(ProbeEventKind::Instantiate), 2);
  assert_eq!(
    count(ProbeEventKind::Cleanup),
    2,
    "the failed entry kept a sibling it had instantiated"
  );
}

/// A body past the inlining budget called from a function inside a loop:
/// the retained VM call sites run on distinct kept frames.
#[test]
fn nested_vm_calls_use_distinct_frames() {
  let mut mesh = Mesh::new();
  mesh.add_function(
    FunctionDef::new("Big", Type::int(), Type::int())
      .body((0..25).map(|_| add(val(Var::Int(1)))).collect()),
  );
  mesh.add_function(
    FunctionDef::new("Parent", Type::int(), Type::int())
      .body(vec![call("Big", vec![]), add(val(Var::Int(1)))]),
  );
  mesh.add_wire(wire(
    "root",
    false,
    vec![
      konst(Var::Int(0)),
      declare("n"),
      repeat(
        vec![get("n"), call("Parent", vec![]), update("n")],
        val(Var::Int(3)),
      ),
      get("n"),
    ],
  ));
  let (outcome, _) = run_logging(&mut mesh, 5);
  assert_eq!(outcome, Outcome::Completed(Var::Int(78)));
}

/// A flattened composite's saved input (its hidden slot) ends when the
/// composite exits: the value continues in the accumulator only, and a
/// collection it shares with a variable is uniquely owned again.
#[test]
fn a_flattened_composite_releases_its_saved_input() {
  use std::sync::Arc;
  for (name, composite) in [
    (
      "when",
      when(vec![konst(Var::Bool(true))], vec![konst(Var::Int(0))]),
    ),
    (
      "if",
      if_(vec![konst(Var::Bool(true))], vec![konst(Var::Int(0))], None),
    ),
    ("repeat", repeat(vec![konst(Var::Int(0))], val(Var::Int(2)))),
  ] {
    let mut mesh = Mesh::new();
    mesh.declare_var("acc", Var::from_seq(Arc::new(vec![Var::Int(7)])), true);
    let acc = mesh.get_var("acc");
    let Some(Var::Seq(items)) = &acc else {
      unreachable!()
    };
    let allocation = Arc::as_ptr(items);
    drop(acc);
    mesh.add_wire(wire(
      "root",
      false,
      vec![
        get("acc"),
        composite,
        konst(Var::Int(1)),
        ShardDef::new(&otter_core::shards::data::PUSH, vec![var("acc")]),
        pause(),
      ],
    ));
    let compiled = mesh.compile("root", Type::none()).unwrap();
    let id = mesh.spawn(&compiled, Var::None).unwrap();
    mesh.tick();
    assert_eq!(mesh.outcome(id), None, "{name}");
    let acc = mesh.get_var("acc");
    let Some(Var::Seq(items)) = &acc else {
      unreachable!()
    };
    assert_eq!(items.as_slice(), &[Var::Int(7), Var::Int(1)], "{name}");
    assert_eq!(
      Arc::as_ptr(items),
      allocation,
      "{name}: the saved input was kept"
    );
  }
}

// --- Compose-time evaluation (docs/metaprogramming.md §2, M8) ---

/// `@fn(Square input: Int output: Int params: {} { = x  x | Math.Multiply(x) })`.
fn square() -> FunctionDef {
  FunctionDef::new("Square", Type::int(), Type::int()).body(vec![
    bind("x"),
    get("x"),
    ShardDef::new(&otter_core::shards::math::MULTIPLY, vec![var("x")]),
  ])
}

fn sixteen() -> ShardDef {
  evaluated(vec![konst(Var::Int(4)), call("Square", vec![])])
}

#[test]
fn a_repeated_evaluation_is_a_cache_hit_within_the_requesting_limits() {
  let mut mesh = Mesh::new();
  mesh.add_function(square());
  mesh.add_wire(wire("first", false, vec![sixteen()]));
  mesh.add_wire(wire("second", false, vec![sixteen(), log()]));
  let first = mesh.compile("first", Type::none()).unwrap();
  mesh.compile("second", Type::none()).unwrap();
  let stats = mesh.cache_stats();
  assert_eq!((stats.evaluations, stats.evaluation_hits), (1, 1));
  assert_eq!(first.flow.output, Type::int());
  // A context with less fuel than the cached result used rejects the hit,
  // with the error its own evaluation would have reported; so does a
  // recompose of a wire whose code holds that result.
  mesh.set_eval_limits(otter_core::EvalLimits {
    fuel: 3,
    ..Default::default()
  });
  mesh.add_wire(wire("third", false, vec![konst(Var::None), sixteen()]));
  for name in ["third", "first"] {
    let err = mesh.compile(name, Type::none()).err().expect("over budget");
    let d = err.diagnostic().expect("structured");
    assert_eq!(d.code, "expansion-budget", "{name}: {d:?}");
    assert!(d.message.contains("cached"), "{name}: {}", d.message);
  }
  assert_eq!(mesh.cache_stats().evaluations, 1);
}

#[test]
fn an_evaluation_reaching_mesh_access_is_not_compose_time() {
  let mut mesh = Mesh::new();
  mesh.declare_var("gain", Var::Int(2), true);
  mesh.add_function(
    FunctionDef::new("Gain", Type::none(), Type::int())
      .uses(&["gain"])
      .body(vec![get("gain")]),
  );
  mesh.add_wire(wire(
    "root",
    false,
    vec![evaluated(vec![call("Gain", vec![])])],
  ));
  let d = compile_error(&mut mesh);
  assert_eq!(d.code, "not-compose-time");
  assert!(d.message.contains("mesh variable gain"), "{}", d.message);
  // Located at the read inside the function that declares it.
  assert_eq!(d.shard.as_deref(), Some("Get"), "{}", d.message);
  assert!(
    d.message.contains("(reached through Gain)"),
    "{}",
    d.message
  );
  assert!(
    d.path
      .contains(&otter_core::diagnostic::PathStep::Function("Gain".into())),
    "{:?}",
    d.path
  );
}

#[test]
fn evaluated_floats_keep_their_bits_and_print_back() {
  use otter_core::shards::math::{DIVIDE, MULTIPLY};
  // The divisor is opaque so the NaN comes from the same division the
  // shard runs, with the target's NaN bits.
  let (zero, divisor) = std::hint::black_box((0.0f64, 0.0f64));
  for (flow, expected) in [
    (
      vec![
        konst(Var::Float(0.0)),
        ShardDef::new(&DIVIDE, vec![val(Var::Float(0.0))]),
      ],
      zero / divisor,
    ),
    (
      vec![
        konst(Var::Float(1.0)),
        ShardDef::new(&DIVIDE, vec![val(Var::Float(0.0))]),
      ],
      f64::INFINITY,
    ),
    (
      vec![
        konst(Var::Float(-1.0)),
        ShardDef::new(&DIVIDE, vec![val(Var::Float(0.0))]),
      ],
      f64::NEG_INFINITY,
    ),
    (
      vec![
        konst(Var::Float(0.0)),
        ShardDef::new(&MULTIPLY, vec![val(Var::Float(-1.0))]),
      ],
      -0.0,
    ),
    (
      vec![
        konst(Var::Float(5e-324)),
        ShardDef::new(&MULTIPLY, vec![val(Var::Float(1.0))]),
      ],
      5e-324,
    ),
    (
      vec![
        konst(Var::Float(0.1)),
        ShardDef::new(&MULTIPLY, vec![val(Var::Float(3.0))]),
      ],
      0.1 * 3.0,
    ),
  ] {
    let mut mesh = Mesh::new();
    mesh.add_wire(wire("root", false, vec![evaluated(flow)]));
    let (outcome, _) = run_logging(&mut mesh, 2);
    let Outcome::Completed(Var::Float(value)) = outcome else {
      panic!("{outcome:?}")
    };
    assert_eq!(
      value.to_bits(),
      expected.to_bits(),
      "{value} against {expected}"
    );
    // Finite values print in the shortest form that reads back exactly.
    if value.is_finite() {
      let printed = Var::Float(value).to_string();
      assert_eq!(
        printed.parse::<f64>().unwrap().to_bits(),
        value.to_bits(),
        "{printed}"
      );
    }
  }
}

// M9: flow parameters (docs/metaprogramming.md §3).

fn run_action() -> ShardDef {
  ShardDef::new(&otter_core::shards::RUN, vec![var("action")])
}

/// `Twice`: runs its block twice, the first result feeding the second.
fn twice() -> FunctionDef {
  FunctionDef::new("Twice", Type::int(), Type::int())
    .flow_param(
      "action",
      otter_core::FlowType {
        input: Type::int(),
        output: Some(Type::int()),
      },
    )
    .body(vec![run_action(), run_action()])
}

#[test]
fn a_straight_line_block_is_inlined_with_its_callee_and_any_other_is_framed() {
  for (block, inlined) in [
    (vec![add(val(Var::Int(1)))], true),
    (vec![probe("b"), add(val(Var::Int(1)))], false),
  ] {
    let mut mesh = Mesh::new();
    mesh.add_function(twice());
    mesh.add_wire(wire(
      "root",
      false,
      vec![
        konst(Var::Int(1)),
        call("Twice", vec![named("action", ParamValue::Flow(block))]),
      ],
    ));
    let root = mesh.compile("root", Type::none()).unwrap();
    // Inlined: the callee's code and the block's, in place of the call and
    // its `Run`s, all VM instructions.
    let kinds = root.flow.instruction_kinds();
    assert_eq!(!kinds.contains(&"fallback"), inlined, "{kinds:?}");
    let (outcome, _) = run_logging(&mut mesh, 10);
    assert_eq!(
      outcome,
      Outcome::Completed(Var::Int(3)),
      "inlined: {inlined}"
    );
  }
}

/// An inlined `Run` that passes its input on restores it from a hidden
/// slot and ends that slot's value at once: a collection the input shares
/// is uniquely owned again for the rest of the callee, so pushing to it is
/// not a copy.
#[test]
fn a_run_releases_the_input_it_restored() {
  use std::sync::Arc;
  let first = FunctionDef::new("First", Type::seq(Type::int()), Type::int())
    .uses(&["acc"])
    .mutates(&["acc"])
    .flow_param(
      "action",
      otter_core::FlowType {
        input: Type::seq(Type::int()),
        output: None,
      },
    )
    .body(vec![
      run_action(),
      take(val(Var::Int(0))),
      declare("first"),
      konst(Var::Int(1)),
      ShardDef::new(&otter_core::shards::data::PUSH, vec![var("acc")]),
      get("first"),
    ]);
  // Inlined with a straight-line block; framed with one that logs.
  for (block, inlined) in [
    (vec![take(val(Var::Int(0)))], true),
    (vec![log(), take(val(Var::Int(0)))], false),
  ] {
    let mut mesh = Mesh::new();
    mesh.declare_var("acc", Var::from_seq(Arc::new(vec![Var::Int(7)])), true);
    let allocation = match &mesh.get_var("acc") {
      Some(Var::Seq(items)) => Arc::as_ptr(items),
      _ => unreachable!(),
    };
    mesh.add_function(first.clone());
    mesh.add_wire(wire(
      "root",
      false,
      vec![
        get("acc"),
        call("First", vec![named("action", ParamValue::Flow(block))]),
        pause(),
      ],
    ));
    let root = mesh.compile("root", Type::none()).unwrap();
    // Inlined, the only fallback is the `Pause`.
    let kinds = root.flow.instruction_kinds();
    let fallbacks = kinds.iter().filter(|k| **k == "fallback").count();
    assert_eq!(fallbacks == 1, inlined, "{kinds:?}");
    let id = mesh.spawn(&root, Var::None).unwrap();
    mesh.tick();
    assert_eq!(mesh.outcome(id), None);
    let acc = mesh.get_var("acc");
    let Some(Var::Seq(items)) = &acc else {
      unreachable!()
    };
    assert_eq!(items.as_slice(), &[Var::Int(7), Var::Int(1)]);
    // A framed callee's frame holds its input (`input`, readable to the
    // end of its body) until the call ends, so there the push copies.
    assert_eq!(
      Arc::as_ptr(items) == allocation,
      inlined,
      "inlined: {inlined}"
    );
  }
}

#[test]
fn a_loop_over_a_block_is_inlined_with_a_straight_line_block() {
  // `Each`: runs the block on each of the first `n` elements.
  let each = FunctionDef::new("Each", Type::seq(Type::int()), Type::seq(Type::int()))
    .param("n", Type::int())
    .flow_param(
      "action",
      otter_core::FlowType {
        input: Type::int(),
        output: None,
      },
    )
    .body(vec![
      bind("xs"),
      konst(Var::Int(0)),
      declare("i"),
      while_(
        vec![get("i"), is_less(var("n"))],
        vec![get("xs"), take(var("i")), run_action(), inc("i")],
      ),
      get("xs"),
    ]);
  for (block, inlined) in [
    (vec![add(var("total")), update("total")], true),
    (vec![probe("b"), add(var("total")), update("total")], false),
  ] {
    let mut mesh = Mesh::new();
    mesh.add_function(each.clone());
    mesh.add_wire(wire(
      "root",
      false,
      vec![
        konst(Var::Int(0)),
        declare("total"),
        konst(Var::from_seq(std::sync::Arc::new(vec![
          Var::Int(1),
          Var::Int(2),
          Var::Int(3),
        ]))),
        call(
          "Each",
          vec![
            named("n", val(Var::Int(3))),
            named("action", ParamValue::Flow(block)),
          ],
        ),
        get("total"),
      ],
    ));
    let root = mesh.compile("root", Type::none()).unwrap();
    let kinds = root.flow.instruction_kinds();
    assert_eq!(!kinds.contains(&"fallback"), inlined, "{kinds:?}");
    let (outcome, _) = run_logging(&mut mesh, 10);
    assert_eq!(
      outcome,
      Outcome::Completed(Var::Int(6)),
      "inlined: {inlined}"
    );
  }
}
