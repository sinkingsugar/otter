//! Real async I/O: `Http.Get` against a local server.

mod support;

use std::thread;
use std::time::{Duration, Instant};

use otter_core::shards::defs::*;
use otter_core::shards::{ProbeEvent, ProbeEventKind, take_probe_events};
use otter_core::{InstanceId, Outcome, ShardDef, Type, Var, WakeMode, WireDef};
use otter_io::http;
use otter_io::runtime::polls_on_this_thread;

use support::{TestServer, closed_port_url};

use otter_core::Mesh;

fn wire(name: &str, looped: bool, flow: Vec<ShardDef>) -> WireDef {
  WireDef {
    name: name.into(),
    looped,
    flow,
  }
}

fn cleanups(events: &[ProbeEvent], tag: &str) -> usize {
  events
    .iter()
    .filter(|e| e.tag == tag && e.kind == ProbeEventKind::Cleanup)
    .count()
}

/// Ticks (without blocking) until the instance finishes, up to 5 s.
fn run_until_done(mesh: &mut Mesh, id: InstanceId) {
  let start = Instant::now();
  while mesh.outcome(id).is_none() {
    assert!(start.elapsed() < Duration::from_secs(5), "timed out");
    mesh.tick();
    thread::sleep(Duration::from_millis(1));
  }
}

/// Ticks until `cond` holds, up to 5 s.
fn tick_until(mesh: &mut Mesh, mut cond: impl FnMut() -> bool) {
  let start = Instant::now();
  while !cond() {
    assert!(start.elapsed() < Duration::from_secs(5), "timed out");
    mesh.tick();
    thread::sleep(Duration::from_millis(1));
  }
}

fn get_wire(mesh: &mut Mesh, url: &str) -> std::sync::Arc<otter_core::CompiledWire> {
  mesh.add_wire(wire("get", false, vec![probe("client"), http::get(url)]));
  mesh.compile("get", Type::none()).unwrap()
}

#[test]
fn successful_response() {
  for mode in [WakeMode::PollEveryTick, WakeMode::OnNotify] {
    take_probe_events();
    let server = TestServer::start();
    let mut mesh = Mesh::new();
    mesh.set_wake_mode(mode);
    let get = get_wire(&mut mesh, &server.url("/ok"));
    let id = mesh.spawn(&get, Var::None).unwrap();
    run_until_done(&mut mesh, id);
    assert_eq!(
      mesh.outcome(id),
      Some(&Outcome::Completed(Var::string("hello"))),
      "{mode:?}"
    );
    assert_eq!(cleanups(&take_probe_events(), "client"), 1);
  }
}

#[test]
fn named_arguments_and_compose_diagnostics() {
  let server = TestServer::start();
  let mut mesh = Mesh::new();
  mesh.add_wire(wire(
    "named",
    false,
    vec![http::get_args(vec![
      otter_core::Arg::named("timeout", otter_core::ParamValue::Value(Var::Int(5))),
      otter_core::Arg::named(
        "url",
        otter_core::ParamValue::Value(Var::string(&server.url("/ok"))),
      ),
    ])],
  ));
  let named = mesh.compile("named", Type::none()).unwrap();
  let id = mesh.spawn(&named, Var::None).unwrap();
  run_until_done(&mut mesh, id);
  assert_eq!(
    mesh.outcome(id),
    Some(&Outcome::Completed(Var::string("hello")))
  );

  // Compose-time checks report structured diagnostics.
  mesh.add_wire(wire(
    "bad-timeout",
    false,
    vec![http::get_with_timeout(&server.url("/ok"), 0)],
  ));
  let err = mesh.compile("bad-timeout", Type::none()).err().unwrap();
  let d = err.diagnostic().unwrap();
  assert_eq!(
    (d.phase.name(), d.code, d.param.as_deref()),
    ("compose", "invalid-argument-value", Some("timeout"))
  );

  mesh.declare_var("n", Var::Int(1), false);
  mesh.add_wire(wire(
    "bad-url-var",
    false,
    vec![http::get_args(vec![otter_core::Arg::pos(
      otter_core::ParamValue::Var("n".into()),
    )])],
  ));
  let err = mesh.compile("bad-url-var", Type::none()).err().unwrap();
  assert_eq!(err.diagnostic().unwrap().code, "wrong-variable-type");

  // A missing URL variable is structured too, naming the parameter.
  mesh.add_wire(wire(
    "missing-url-var",
    false,
    vec![http::get_args(vec![otter_core::Arg::named(
      "url",
      otter_core::ParamValue::Var("nowhere".into()),
    )])],
  ));
  let err = mesh.compile("missing-url-var", Type::none()).err().unwrap();
  let d = err.diagnostic().unwrap();
  assert_eq!(
    (d.code, d.param.as_deref(), d.param_index),
    ("unknown-variable", Some("url"), Some(0))
  );
}

#[test]
fn headers_from_a_literal_or_a_variable_reach_the_server() {
  use otter_core::{Arg, ParamValue};
  let server = TestServer::start();
  let url = Var::string(&server.url("/echo-headers"));
  let headers = |pairs: &[(&str, Var)]| Var::table(pairs.to_vec());
  let get = |headers: ParamValue| {
    http::get_args(vec![
      Arg::pos(ParamValue::Value(url.clone())),
      Arg::named("headers", headers),
    ])
  };
  let mut mesh = Mesh::new();
  mesh.add_wire(wire(
    "literal",
    false,
    vec![get(ParamValue::Value(headers(&[
      ("user-agent", Var::string("otter-test")),
      ("x-b", Var::string("2")),
      ("X-A", Var::string("1")),
    ])))],
  ));
  let literal = mesh.compile("literal", Type::none()).unwrap();
  let id = mesh.spawn(&literal, Var::None).unwrap();
  run_until_done(&mut mesh, id);
  assert_eq!(
    mesh.outcome(id),
    Some(&Outcome::Completed(Var::string(
      "user-agent: otter-test\nx-a: 1\nx-b: 2"
    )))
  );

  // A variable is read at activation, so each run sends its current value.
  mesh.declare_var("h", headers(&[("x-run", Var::string("one"))]), true);
  mesh.add_wire(wire("bound", false, vec![get(ParamValue::Var("h".into()))]));
  let bound = mesh.compile("bound", Type::none()).unwrap();
  let id = mesh.spawn(&bound, Var::None).unwrap();
  run_until_done(&mut mesh, id);
  let body = |mesh: &Mesh, id| match mesh.outcome(id) {
    Some(Outcome::Completed(v)) => v.as_str().unwrap().to_string(),
    other => panic!("{other:?}"),
  };
  assert!(
    body(&mesh, id).ends_with("x-run: one"),
    "{}",
    body(&mesh, id)
  );

  // A bound value that is not a valid header fails the request at activation.
  mesh.set_var("h", headers(&[("x-run", Var::string("a\nb"))]));
  let id = mesh.spawn(&bound, Var::None).unwrap();
  run_until_done(&mut mesh, id);
  assert!(
    matches!(mesh.outcome(id), Some(Outcome::Failed(otter_core::Error::Activation(m))) if m == "Http.Get: invalid value for header x-run"),
    "{:?}",
    mesh.outcome(id)
  );

  // Constant headers are checked at compose.
  for (name, value, message) in [
    ("x-n", Var::Int(1), "header x-n must be a String, got Int"),
    (
      "bad name",
      Var::string("v"),
      "invalid header name \"bad name\"",
    ),
    ("x-v", Var::string("a\nb"), "invalid value for header x-v"),
  ] {
    let wire_name = format!("bad-{name}");
    mesh.add_wire(wire(
      &wire_name,
      false,
      vec![get(ParamValue::Value(headers(&[(name, value)])))],
    ));
    let err = mesh.compile(&wire_name, Type::none()).err().unwrap();
    let d = err.diagnostic().unwrap();
    assert_eq!(
      (d.code, d.param.as_deref(), d.message.as_str()),
      ("invalid-argument-value", Some("headers"), message)
    );
  }
  // A variable must be a table whose values can be Strings.
  mesh.declare_var("not-a-table", Var::Int(1), false);
  mesh.declare_var("ints", headers(&[("x-n", Var::Int(1))]), false);
  for var in ["not-a-table", "ints"] {
    mesh.add_wire(wire(var, false, vec![get(ParamValue::Var(var.into()))]));
    let err = mesh.compile(var, Type::none()).err().unwrap();
    assert_eq!(
      err.diagnostic().unwrap().code,
      "wrong-variable-type",
      "{var}"
    );
  }
}

#[test]
fn connection_failure_and_error_status() {
  take_probe_events();
  let mut mesh = Mesh::new();
  let refused = get_wire(&mut mesh, &closed_port_url());
  let id = mesh.spawn(&refused, Var::None).unwrap();
  run_until_done(&mut mesh, id);
  assert!(
    matches!(mesh.outcome(id), Some(Outcome::Failed(otter_core::Error::Activation(m))) if m.starts_with("Request failed")),
    "{:?}",
    mesh.outcome(id)
  );
  assert_eq!(cleanups(&take_probe_events(), "client"), 1);

  let server = TestServer::start();
  let mut mesh = Mesh::new();
  let missing = get_wire(&mut mesh, &server.url("/missing"));
  let id = mesh.spawn(&missing, Var::None).unwrap();
  run_until_done(&mut mesh, id);
  assert!(
    matches!(mesh.outcome(id), Some(Outcome::Failed(otter_core::Error::Activation(m))) if m.contains("status 404") && m.contains("nope")),
    "{:?}",
    mesh.outcome(id)
  );
}

/// Cancels while the request waits on `path`; checks that the client
/// really closes the connection, that cleanup runs once, and that late
/// wakeups (the runtime finishing the cancelled task) are harmless.
fn cancel_while_waiting(path: &str, waiting_event: &str) {
  for mode in [WakeMode::PollEveryTick, WakeMode::OnNotify] {
    take_probe_events();
    let server = TestServer::start();
    let mut mesh = Mesh::new();
    mesh.set_wake_mode(mode);
    let get = get_wire(&mut mesh, &server.url(path));
    let id = mesh.spawn(&get, Var::None).unwrap();
    tick_until(&mut mesh, || {
      server.events().iter().any(|e| e == waiting_event)
    });

    mesh.cancel(id);
    assert_eq!(mesh.outcome(id), Some(&Outcome::Cancelled));
    assert_eq!(cleanups(&take_probe_events(), "client"), 1);
    // Dropping the pending future closes the connection: the server sees
    // the client go away while it is still waiting to respond. What it
    // already received stays received.
    assert!(
      server.wait_for(&format!("client-closed {path}"), Duration::from_secs(2)),
      "{mode:?}: connection not closed after cancel: {:?}",
      server.events()
    );
    assert!(server.events().contains(&format!("received {path}")));

    // Late wakeups: let everything settle and keep ticking; the
    // cancelled instance never resumes.
    server.release();
    for _ in 0..20 {
      mesh.tick();
      thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(mesh.outcome(id), Some(&Outcome::Cancelled));
    assert!(take_probe_events().is_empty());
    assert!(!server.events().contains(&format!("completed {path}")));
  }
}

#[test]
fn cancel_while_waiting_for_headers() {
  cancel_while_waiting("/slow-headers", "received /slow-headers");
}

#[test]
fn cancel_midway_through_body() {
  cancel_while_waiting("/slow-body", "partial-sent /slow-body");
}

#[test]
fn other_instances_progress_while_a_request_waits() {
  let server = TestServer::start();
  let mut mesh = Mesh::new();
  mesh.set_wake_mode(WakeMode::OnNotify);
  mesh.declare_var("count", Var::Int(0), true);
  mesh.add_wire(wire("counter", true, vec![inc("count")]));
  let get = get_wire(&mut mesh, &server.url("/slow-body"));
  let counter = mesh.compile("counter", Type::none()).unwrap();
  let waiter = mesh.spawn(&get, Var::None).unwrap();
  mesh.spawn(&counter, Var::None).unwrap();

  tick_until(&mut mesh, || {
    server
      .events()
      .iter()
      .any(|e| e == "partial-sent /slow-body")
  });
  let before = mesh.get_var("count");
  for _ in 0..100 {
    mesh.tick();
  }
  // The counter advanced on every tick while the request was pending:
  // waiting never blocks the scheduler.
  let (Some(Var::Int(a)), Some(Var::Int(b))) = (before, mesh.get_var("count")) else {
    unreachable!()
  };
  assert_eq!(b - a, 100);
  assert!(mesh.outcome(waiter).is_none());

  server.release();
  run_until_done(&mut mesh, waiter);
  assert_eq!(
    mesh.outcome(waiter),
    Some(&Outcome::Completed(Var::string("helloworld")))
  );
}

#[test]
fn notified_waiting_polls_only_on_progress() {
  let mut polls = Vec::new();
  for mode in [WakeMode::PollEveryTick, WakeMode::OnNotify] {
    let server = TestServer::start();
    let mut mesh = Mesh::new();
    mesh.set_wake_mode(mode);
    let get = get_wire(&mut mesh, &server.url("/slow-headers"));
    let id = mesh.spawn(&get, Var::None).unwrap();
    let before = polls_on_this_thread();
    tick_until(&mut mesh, || {
      server
        .events()
        .iter()
        .any(|e| e == "received /slow-headers")
    });
    for _ in 0..200 {
      mesh.tick();
    }
    server.release();
    run_until_done(&mut mesh, id);
    assert_eq!(
      mesh.outcome(id),
      Some(&Outcome::Completed(Var::string("late")))
    );
    polls.push(polls_on_this_thread() - before);
  }
  // Polling every tick re-checks the pending request ~200+ times; notified
  // waiting polls only when the runtime made progress (a few times).
  assert!(polls[0] >= 200, "poll mode: {} polls", polls[0]);
  assert!(polls[1] <= 5, "notify mode: {} polls", polls[1]);
}

/// Runs one `Http.Get` with `args` to completion: its composed output type
/// and outcome.
fn run_get(args: Vec<otter_core::Arg>) -> (Type, Outcome) {
  let mut mesh = Mesh::new();
  mesh.add_wire(wire("get", false, vec![http::get_args(args)]));
  let get = mesh.compile("get", Type::none()).unwrap();
  let otter_core::signature::SignatureOutput::Type(output) = get.signature().output else {
    unreachable!("a compiled wire has an exact output type")
  };
  let id = mesh.spawn(&get, Var::None).unwrap();
  run_until_done(&mut mesh, id);
  (output, mesh.outcome(id).cloned().unwrap())
}

/// `url` plus named literal options.
fn get_args(url: &str, options: &[(&str, Var)]) -> Vec<otter_core::Arg> {
  use otter_core::{Arg, ParamValue};
  let mut args = vec![Arg::pos(ParamValue::Value(Var::string(url)))];
  for (name, value) in options {
    args.push(Arg::named(name, ParamValue::Value(value.clone())));
  }
  args
}

fn field<'a>(value: &'a Var, key: &str) -> &'a Var {
  value.as_table().and_then(|t| t.get(key)).unwrap()
}

fn field_type(ty: Type, key: &str) -> Type {
  ty.as_table()
    .and_then(|t| t.keys.iter().find(|(k, _)| &**k == key))
    .map(|(_, ty)| *ty)
    .unwrap()
}

#[test]
fn bytes_and_full_response_have_exact_types() {
  let server = TestServer::start();
  let (ty, outcome) = run_get(get_args(
    &server.url("/binary"),
    &[("bytes", Var::Bool(true))],
  ));
  assert_eq!(ty, Type::bytes());
  assert_eq!(outcome, Outcome::Completed(Var::bytes(&[0xff, 0x00, 0x01])));

  // A non-success status is a response, not an error.
  let (ty, outcome) = run_get(get_args(
    &server.url("/missing"),
    &[("full-response", Var::Bool(true))],
  ));
  let headers = Type::table(Vec::<(&str, Type)>::new(), Some(Type::string()));
  assert_eq!(
    ty,
    Type::fixed_table([
      ("status", Type::int()),
      ("headers", headers),
      ("body", Type::string()),
    ])
  );
  let Outcome::Completed(response) = outcome else {
    panic!("{outcome:?}")
  };
  assert_eq!(field(&response, "status"), &Var::Int(404));
  assert_eq!(field(&response, "body"), &Var::string("nope"));
  let response_headers = field(&response, "headers");
  assert_eq!(field(response_headers, "content-length"), &Var::string("4"));
  assert_eq!(field(response_headers, "connection"), &Var::string("close"));

  // Both: the body as Bytes; a repeated header joined.
  let (ty, outcome) = run_get(get_args(
    &server.url("/twice"),
    &[
      ("bytes", Var::Bool(true)),
      ("full-response", Var::Bool(true)),
    ],
  ));
  assert_eq!(field_type(ty, "body"), Type::bytes());
  let Outcome::Completed(response) = outcome else {
    panic!("{outcome:?}")
  };
  assert_eq!(field(&response, "status"), &Var::Int(200));
  assert_eq!(field(&response, "body"), &Var::bytes(b"hello"));
  assert_eq!(
    field(field(&response, "headers"), "x-twice"),
    &Var::string("a, b")
  );

  // Option values are checked at compose.
  let mut mesh = Mesh::new();
  for (name, option, value) in [
    ("negative-retry", "retry", Var::Int(-1)),
    ("negative-backoff", "backoff", Var::Float(-0.5)),
    ("infinite-backoff", "backoff", Var::Float(f64::INFINITY)),
  ] {
    mesh.add_wire(wire(
      name,
      false,
      vec![http::get_args(get_args(
        &server.url("/ok"),
        &[(option, value)],
      ))],
    ));
    let err = mesh.compile(name, Type::none()).err().unwrap();
    let d = err.diagnostic().unwrap();
    assert_eq!(
      (d.code, d.param.as_deref()),
      ("invalid-argument-value", Some(option)),
      "{name}"
    );
  }
}

#[test]
fn retry_repeats_transient_failures_only() {
  let quick = |retry: i64| vec![("retry", Var::Int(retry)), ("backoff", Var::Float(0.01))];
  let failed = |outcome: &Outcome| match outcome {
    Outcome::Failed(otter_core::Error::Activation(m)) => m.clone(),
    other => panic!("{other:?}"),
  };

  // Transport errors: two dropped connections, then a response.
  let server = TestServer::start();
  let (_, outcome) = run_get(get_args(&server.url("/flaky"), &quick(2)));
  assert_eq!(outcome, Outcome::Completed(Var::string("recovered")));
  assert_eq!(server.received("/flaky"), 3);

  // Too few attempts: the last error, with the count.
  let server = TestServer::start();
  let (_, outcome) = run_get(get_args(&server.url("/flaky"), &quick(1)));
  let message = failed(&outcome);
  assert!(
    message.starts_with("Request failed") && message.ends_with("(after 2 attempts)"),
    "{message}"
  );
  assert_eq!(server.received("/flaky"), 2);

  // A transient status is retried; a 404 is not.
  let server = TestServer::start();
  let (_, outcome) = run_get(get_args(&server.url("/busy"), &quick(1)));
  assert_eq!(outcome, Outcome::Completed(Var::string("recovered")));
  assert_eq!(server.received("/busy"), 2);
  let (_, outcome) = run_get(get_args(&server.url("/missing"), &quick(3)));
  assert!(failed(&outcome).contains("status 404"));
  assert_eq!(server.received("/missing"), 1);

  // With full-response, a status is the answer, never retried.
  let server = TestServer::start();
  let mut options = quick(2);
  options.push(("full-response", Var::Bool(true)));
  let (_, outcome) = run_get(get_args(&server.url("/busy"), &options));
  let Outcome::Completed(response) = outcome else {
    panic!("{outcome:?}")
  };
  assert_eq!(field(&response, "status"), &Var::Int(503));
  assert_eq!(server.received("/busy"), 1);
}
