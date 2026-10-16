//! Cancelling `Http.Get` while it waits between attempts ends its task at
//! once, not when the wait runs out. Its own test binary: it counts the
//! shared runtime's live tasks, which no other test may add to.

mod support;

use std::thread;
use std::time::{Duration, Instant};

use otter_core::{Arg, Mesh, Outcome, ParamValue, Type, Var, WireDef};
use otter_io::http;
use otter_io::runtime::runtime;

use support::TestServer;

/// Ticks until `cond` holds, up to 5 s.
fn tick_until(mesh: &mut Mesh, mut cond: impl FnMut() -> bool) {
  let start = Instant::now();
  while !cond() {
    assert!(start.elapsed() < Duration::from_secs(5), "timed out");
    mesh.tick();
    thread::sleep(Duration::from_millis(1));
  }
}

#[test]
fn cancelling_during_the_backoff_ends_the_request_at_once() {
  let server = TestServer::start();
  let mut mesh = Mesh::new();
  let named = |name, value| Arg::named(name, ParamValue::Value(value));
  mesh.add_wire(WireDef {
    name: "get".into(),
    looped: false,
    flow: vec![http::get_args(vec![
      Arg::pos(ParamValue::Value(Var::string(&server.url("/drop")))),
      named("retry", Var::Int(5)),
      named("backoff", Var::Float(2.0)),
    ])],
  });
  let get = mesh.compile("get", Type::none()).unwrap();
  let id = mesh.spawn(&get, Var::None).unwrap();
  tick_until(&mut mesh, || server.received("/drop") == 1);
  // Let the first attempt fail: the request then waits out the backoff,
  // and its task (with no connection left) is the only one alive.
  let alive = || runtime().metrics().num_alive_tasks();
  tick_until(&mut mesh, || alive() == 1);
  thread::sleep(Duration::from_millis(50));
  assert_eq!(alive(), 1);

  mesh.cancel(id);
  assert_eq!(mesh.outcome(id), Some(&Outcome::Cancelled));
  // Far sooner than the 2 s wait.
  let start = Instant::now();
  while alive() > 0 {
    assert!(
      start.elapsed() < Duration::from_millis(500),
      "the request's task outlived its cancellation"
    );
    thread::sleep(Duration::from_millis(1));
  }
  assert_eq!(server.received("/drop"), 1, "{:?}", server.events());
}
