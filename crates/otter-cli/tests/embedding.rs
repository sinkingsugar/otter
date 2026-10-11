//! The example from docs/embedding.md, as a test: a host crate defines its
//! own shards with the public API only (as an external crate would), adds
//! them to the catalog, and checks and runs a script.

use std::collections::HashMap;

use otter_core::args::Args;
use otter_core::compose::ComposeCtx;
use otter_core::describe::{
  DefaultValue, Forms, InputDesc, OutputDesc, ParamDecl, Params, Requirement, ShardDesc, Targets,
  TypeName,
};
use otter_core::instance::{InstanceCtx, LeafCtx};
use otter_core::shards::Operand;
use otter_core::shards::async_shard::{AsyncShard, async_type};
use otter_core::shards::leaf::{LeafShard, leaf_type};
use otter_core::{Catalog, Composed, Error, Flow, Outcome, Result, ShardType, Type, Var};
use otter_io::IoTask;
use otter_lang::{Program, Source};

// --- A leaf shard: a fixed record from a literal-or-variable parameter ---

static READING_PARAMS: &[ParamDecl] = &[
  ParamDecl {
    name: "sensor",
    help: "Which sensor to read: a literal, or an Int variable read at activation.",
    forms: Forms::LITERAL.or(Forms::VARIABLE),
    types: &[TypeName::Int],
    requirement: Requirement::Default(DefaultValue::Int(0)),
    ty: None,
  },
  ParamDecl {
    name: "unit",
    help: "Override the unit (optional): a literal or a String variable.",
    forms: Forms::LITERAL.or(Forms::VARIABLE),
    types: &[TypeName::String],
    requirement: Requirement::Optional,
    ty: None,
  },
];

/// Compiled Host.Reading: the sensor, and the unit override if given.
struct ReadingCompiled {
  sensor: Operand,
  unit: Option<Operand>,
}

const READING_DESC: ShardDesc = ShardDesc {
  name: "Host.Reading",
  version: 1,
  summary: "Reads a sensor: {id value unit}.",
  help: "Ignores its input. value is none when the sensor cannot be read.",
  params: Params::Declared(READING_PARAMS),
  input: InputDesc::Ignored,
  output: OutputDesc::Dynamic("{id: Int value: Float | None unit: String}"),
  targets: Targets::All,
  aliases: &[],
  effects: otter_core::signature::Effects::UNKNOWN,
  lifetime: otter_core::signature::Lifetime::Unknown,
};

struct Reading;

impl LeafShard for Reading {
  type Compiled = ReadingCompiled;
  type State = ();
  const DESC: ShardDesc = READING_DESC;

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<ReadingCompiled>> {
    // Compose reads only its arguments and declared context: never the host.
    // The declared types (Int) are checked by core, for a literal (by the
    // decoder) and for a variable (by compose_arg).
    let (sensor, _) = Operand::compose_arg(args, "sensor", READING_DESC.name, ctx)?;
    // An optional parameter: None when the script did not give it.
    let unit =
      Operand::compose_optional_arg(args, "unit", READING_DESC.name, ctx)?.map(|(op, _)| op);
    Ok(Composed {
      compiled: ReadingCompiled { sensor, unit },
      // A fixed record: `r.value` composes to `Float | None`, `r.typo` fails.
      output: Type::fixed_table([
        ("id", Type::int()),
        ("value", Type::union([Type::float(), Type::none()])),
        ("unit", Type::string()),
      ]),
    })
  }

  fn instantiate(_: &ReadingCompiled, _: &mut InstanceCtx) -> Result<()> {
    Ok(())
  }

  fn activate(c: &ReadingCompiled, _: &mut (), ctx: &mut impl LeafCtx, _: &Var) -> Result<Flow> {
    let Var::Int(id) = c.sensor.get(ctx) else {
      return Err(Error::Activation(
        "Host.Reading: sensor is not an Int".into(),
      ));
    };
    // The host is read at activation; an unknown value is an explicit none.
    let value = if id >= 0 {
      Var::Float(id as f64 * 1.5)
    } else {
      Var::None
    };
    Ok(Flow::Next(Var::table([
      ("id", Var::Int(id)),
      ("value", value),
      (
        "unit",
        c.unit
          .as_ref()
          .map_or_else(|| Var::string("C"), |u| u.get(ctx)),
      ),
    ])))
  }
}

// --- A blocking shard: slow host work off the mesh thread ---

const SCAN_DESC: ShardDesc = ShardDesc {
  name: "Host.Scan",
  version: 1,
  summary: "Counts to the input on a worker thread.",
  help: "Suspends only its own wire; cancelling the instance stops the scan.",
  params: Params::Declared(&[]),
  input: InputDesc::Types(&[TypeName::Int]),
  output: OutputDesc::Fixed(TypeName::Int),
  targets: Targets::NativeOnly,
  aliases: &[],
  effects: otter_core::signature::Effects::UNKNOWN,
  lifetime: otter_core::signature::Lifetime::Unknown,
};

struct Scan;

impl AsyncShard for Scan {
  type Compiled = ();
  type Op = IoTask;
  const DESC: ShardDesc = SCAN_DESC;

  fn compose(_: &Args, _: &mut ComposeCtx<'_>) -> Result<Composed<()>> {
    // The declared input (Int) is enforced by compose before this runs.
    Ok(Composed {
      compiled: (),
      output: Type::int(),
    })
  }

  fn start(_: &(), _: &mut impl LeafCtx, input: &Var) -> Result<IoTask> {
    // Own the inputs: the work runs on another thread.
    let Var::Int(n) = *input else {
      return Err(Error::Activation("Host.Scan: input is not an Int".into()));
    };
    Ok(otter_io::runtime::spawn_blocking(move |cancel| {
      let mut count = 0;
      for _ in 0..n {
        if cancel.is_cancelled() {
          return Err("cancelled".into());
        }
        count += 1;
      }
      Ok(Var::Int(count))
    }))
  }
}

// --- The host's catalog: its shards next to the core's ---

static READING: ShardType = leaf_type::<Reading>();
static SCAN: ShardType = async_type::<Scan>();
static HOST_CATALOG: &[&ShardType] = &[&READING, &SCAN];

fn catalog() -> Catalog {
  Catalog::new(&[otter_core::shards::CATALOG, HOST_CATALOG]).expect("no duplicate names")
}

const SCRIPT: &str = r#"
@sensor | ParseInt = id
Host.Reading(sensor: id) = r
If(r.value | IsNone {"unreadable"} {f"{r.id}: {r.value}{r.unit}"}) | Log
1000 | Host.Scan
"#;

#[test]
fn a_host_checks_and_runs_a_script_with_its_own_shards() {
  let mut defines = HashMap::new();
  defines.insert("sensor".to_string(), "4".to_string());
  let report = otter_lang::check(Source::new("host.shs", SCRIPT), &catalog(), &defines);
  assert!(report.ok(), "{}", report.to_json());

  let program = Program::load(Source::new("host.shs", SCRIPT), &catalog(), &defines)
    .unwrap_or_else(|(_, d)| panic!("{d:?}"));
  let (report, lines) = otter_core::log::capture(|| program.run());
  let report = report.unwrap_or_else(|d| panic!("{d:?}"));
  assert_eq!(lines, ["4: 6C"]);
  assert!(matches!(
    &report.outcomes[0].1,
    Some(Outcome::Completed(Var::Int(1000)))
  ));

  // A typo on the host's fixed record is a located compose error.
  let report = otter_lang::check(
    Source::new("host.shs", "Host.Reading = r\nr.valeu"),
    &catalog(),
    &HashMap::new(),
  );
  let d = &report.diagnostics[0];
  assert_eq!(
    (d.code, d.line, d.did_you_mean.clone()),
    ("unknown-key", Some(2), vec!["value".to_string()])
  );

  // Declared input types are enforced for host shards too: Host.Scan does
  // not check its input itself.
  let report = otter_lang::check(
    Source::new("host.shs", "\"x\" | Host.Scan"),
    &catalog(),
    &HashMap::new(),
  );
  let d = &report.diagnostics[0];
  assert_eq!(
    (d.code, d.shard.as_deref()),
    ("input-type-mismatch", Some("Host.Scan"))
  );
  assert!(
    d.message.contains("Host.Scan needs Int input, got String"),
    "{}",
    d.message
  );

  // A variable of the wrong type for a declared parameter is reported by
  // core, located at the shard.
  let report = otter_lang::check(
    Source::new("host.shs", "\"two\" = s\nHost.Reading(sensor: s)"),
    &catalog(),
    &HashMap::new(),
  );
  let d = &report.diagnostics[0];
  assert_eq!(
    (d.code, d.param.as_deref(), d.line),
    ("wrong-variable-type", Some("sensor"), Some(2))
  );
  assert!(
    d.message.contains("sensor must be Int, but s is String"),
    "{}",
    d.message
  );

  // The optional parameter: absent (the default unit), and given as a
  // variable.
  for (src, unit) in [
    ("Host.Reading(sensor: 2) = r\nr.unit", "C"),
    (
      "\"F\" = u\nHost.Reading(sensor: 2 unit: u) = r\nr.unit",
      "F",
    ),
  ] {
    let program = Program::load(Source::new("host.shs", src), &catalog(), &HashMap::new())
      .unwrap_or_else(|(_, d)| panic!("{d:?}"));
    let report = program.run().unwrap_or_else(|d| panic!("{d:?}"));
    assert!(
      matches!(&report.outcomes[0].1, Some(Outcome::Completed(Var::String(u))) if &***u == unit),
      "{src}: {:?}",
      report.outcomes
    );
  }
}

// A service owned by the host, with a recorded/fake operation. This models
// keeping an attached resource warm across revisions without live I/O.
#[derive(Default)]
struct WarmService {
  ready: std::cell::Cell<bool>,
  starts: std::cell::Cell<i64>,
  dropped: std::cell::Cell<i64>,
}

thread_local! {
  static WARM_SERVICE: std::cell::RefCell<Option<std::rc::Rc<WarmService>>> = const { std::cell::RefCell::new(None) };
}

struct WarmOperation {
  service: std::rc::Rc<WarmService>,
  pending: bool,
}

impl std::future::Future for WarmOperation {
  type Output = Result<Var>;

  fn poll(
    self: std::pin::Pin<&mut Self>,
    _: &mut std::task::Context<'_>,
  ) -> std::task::Poll<Self::Output> {
    if self.pending && !self.service.ready.get() {
      std::task::Poll::Pending
    } else {
      std::task::Poll::Ready(Ok(Var::Int(self.service.starts.get())))
    }
  }
}

impl Drop for WarmOperation {
  fn drop(&mut self) {
    self.service.dropped.set(self.service.dropped.get() + 1);
  }
}

struct Warm;

impl AsyncShard for Warm {
  type Compiled = ();
  type Op = WarmOperation;
  const DESC: ShardDesc = ShardDesc {
    name: "Host.Warm",
    summary: otter_core::shard_doc!("Uses a service already owned by the host."),
    help: otter_core::shard_doc!(
      "Zero input waits; other inputs return the service's operation count."
    ),
    ..SCAN_DESC
  };

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<()>> {
    Scan::compose(args, ctx)
  }

  fn start(_: &(), _: &mut impl LeafCtx, input: &Var) -> Result<WarmOperation> {
    let service = WARM_SERVICE.with(|s| s.borrow().as_ref().unwrap().clone());
    // Before each new revision starts, the old operation has been dropped.
    assert_eq!(service.starts.get(), service.dropped.get());
    service.starts.set(service.starts.get() + 1);
    Ok(WarmOperation {
      service,
      pending: matches!(input, Var::Int(0)),
    })
  }
}

#[test]
fn reload_keeps_host_service_warm_and_cancels_pending_operations() {
  fn exercise() {
    static WARM: ShardType = async_type::<Warm>();
    let catalog = Catalog::new(&[otter_core::shards::CATALOG, &[&WARM]]).unwrap();
    let service = std::rc::Rc::new(WarmService::default());
    WARM_SERVICE.with(|s| *s.borrow_mut() = Some(service.clone()));
    let mut session = otter_lang::Session::new();
    let defines = HashMap::new();
    let load = |session: &mut otter_lang::Session, text| {
      session
        .reload(Source::new("warm.shs", text), &catalog, &defines)
        .unwrap_or_else(|(_, d)| panic!("{d:?}"))
    };
    load(&mut session, "0 | Host.Warm");
    assert_eq!(service.starts.get(), 0); // Preparation is effect-free.
    assert!(session.tick().is_empty());
    assert_eq!(service.starts.get(), 1);
    assert_eq!(service.dropped.get(), 0);
    assert!(
      session
        .reload(Source::new("bad.shs", "unknown"), &catalog, &defines)
        .is_err()
    );
    assert_eq!(service.dropped.get(), 0);
    assert!(session.tick().is_empty());
    let cancelled = load(&mut session, "1 | Host.Warm");
    assert!(matches!(cancelled[0].outcome, Outcome::Cancelled));
    assert_eq!(service.dropped.get(), 1);
    assert_eq!(service.starts.get(), 1); // New revision has not started.
    assert!(matches!(
      session.tick()[0].outcome,
      Outcome::Completed(Var::Int(2))
    ));
    assert_eq!(service.dropped.get(), 2);
    load(&mut session, "0 | Host.Warm");
    session.tick();
    drop(session); // Drop also cancels pending work.
    assert_eq!(service.dropped.get(), 3);
    assert_eq!(std::rc::Rc::strong_count(&service), 2); // Host + service registry only.
    WARM_SERVICE.with(|s| *s.borrow_mut() = None);
  }
  exercise();
}

#[test]
fn preserving_reload_keeps_pending_host_operation_until_its_call_returns() {
  fn exercise() {
    static WARM: ShardType = async_type::<Warm>();
    let catalog = Catalog::new(&[otter_core::shards::CATALOG, &[&WARM]]).unwrap();
    let service = std::rc::Rc::new(WarmService::default());
    WARM_SERVICE.with(|s| *s.borrow_mut() = Some(service.clone()));
    let mut session = otter_lang::Session::new();
    let defines = HashMap::new();
    let load = |session: &mut otter_lang::Session, input| {
      let source = format!(
        r#"@fn(Inner input: None output: Int params: {{}} {{{input} Host.Warm}})
@wire(main {{Keep(n 0) Inc(n) Log Inner}} looped: true)
@mesh(m) @schedule(m main) @run(m)"#
      );
      session
        .reload_preserving(Source::new("warm.shs", source), &catalog, &defines)
        .unwrap_or_else(|(_, d)| panic!("{d:?}"))
    };
    load(&mut session, 0);
    let (_, lines) = otter_core::log::capture(|| {
      session.tick();
      assert_eq!(service.starts.get(), 1);
      assert!(load(&mut session, 1).is_empty());
      session.tick();
      assert_eq!(service.dropped.get(), 0); // Accepted edit does not cancel the active call.
      assert_eq!(service.starts.get(), 1);
      service.ready.set(true);
      session.tick(); // Existing operation finishes before replacement starts.
      assert_eq!(service.dropped.get(), 1);
      session.tick();
    });
    assert_eq!(lines, ["1", "2"]);
    assert_eq!(service.starts.get(), 2);
    assert_eq!(service.dropped.get(), 2);
    drop(session);
    assert_eq!(std::rc::Rc::strong_count(&service), 2);
    WARM_SERVICE.with(|s| *s.borrow_mut() = None);
  }
  exercise();
}
