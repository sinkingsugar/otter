//! The 2.0 client of the HTTP concurrency benchmark (`bench/http-concurrency/`).
//!
//! Compiles one wire doing a single `Http.Get` (compose is excluded from the
//! timings), spawns N instances of it (timed as creation), then ticks without
//! sleeping until every request has finished (timed as execution). Prints
//! one `BENCH` line. Waiting instances use notification-driven wakeups.
//!
//! Usage: bench_http <url> <instances>

use std::time::Instant;

use otter_core::{Outcome, Type, Var, WakeMode, WireDef};

macro_rules! run {
  ($mesh:expr, $url:expr, $n:expr) => {{
    let mut mesh = $mesh;
    mesh.set_wake_mode(WakeMode::OnNotify);
    mesh.add_wire(WireDef {
      name: "req".into(),
      looped: false,
      flow: vec![otter_io::http::get_with_timeout($url, 30)],
    });
    let req = mesh.compile("req", Type::none()).expect("compile");

    let t = Instant::now();
    let ids: Vec<_> = (0..$n)
      .map(|_| mesh.spawn(&req, Var::None).expect("spawn"))
      .collect();
    let create_ms = t.elapsed().as_secs_f64() * 1000.0;

    let t = Instant::now();
    let mut ticks = 0u64;
    while mesh.running() > 0 {
      mesh.tick();
      ticks += 1;
    }
    let exec_ms = t.elapsed().as_secs_f64() * 1000.0;

    let ok = ids
      .iter()
      .filter(|id| matches!(mesh.outcome(**id), Some(Outcome::Completed(_))))
      .count();
    (create_ms, exec_ms, ticks, ok)
  }};
}

fn main() {
  let args: Vec<String> = std::env::args().skip(1).collect();
  let url = args.first().expect("url").clone();
  let n: usize = args.get(1).expect("instances").parse().expect("instances");
  let (create_ms, exec_ms, ticks, ok) = run!(otter_core::Mesh::new(), &url, n);
  println!(
    "BENCH runtime=2.0 instances={n} create_ms={create_ms:.3} exec_ms={exec_ms:.3} ticks={ticks} ok={ok}"
  );
}
