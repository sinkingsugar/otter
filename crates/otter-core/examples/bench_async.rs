//! Polling every tick against resuming on notification.
//!
//! N instances each loop on a simulated request with a 50-step latency, so
//! most ticks have nothing new for most instances. Only `mesh.tick()` is timed
//! (the service's clock step is the same for every configuration). After 10
//! warm-up ticks, 1000 ticks are timed; completed requests and total future
//! polls are counted.
//!
//! Usage: cargo run --release --example bench_async

use std::time::Instant;

use otter_core::shards::defs::*;
use otter_core::shards::sim::{self, RequestState};
use otter_core::{Type, Var, WakeMode, WireDef};

const INSTANCES: usize = 1000;
const WARMUP: u32 = 10;
const TICKS: u32 = 1000;
const LATENCY: i64 = 50;

macro_rules! run_async {
  ($mesh:expr, $mode:expr) => {{
    sim::reset();
    let mut mesh = $mesh;
    mesh.set_wake_mode($mode);
    mesh.add_wire(WireDef {
      name: "client".into(),
      looped: true,
      flow: vec![request(LATENCY, false, true)],
    });
    let client = mesh.compile("client", Type::none()).expect("compile");
    for _ in 0..INSTANCES {
      mesh.spawn(&client, Var::None).expect("spawn");
    }
    for _ in 0..WARMUP {
      sim::advance();
      mesh.tick();
    }
    let polls0 = sim::total_polls();
    let done0 = completed();
    let mut tick_time = 0.0;
    for _ in 0..TICKS {
      sim::advance();
      let t = Instant::now();
      mesh.tick();
      tick_time += t.elapsed().as_secs_f64();
    }
    (
      tick_time * 1e9 / f64::from(TICKS),
      sim::total_polls() - polls0,
      completed() - done0,
    )
  }};
}

fn completed() -> usize {
  sim::request_states()
    .iter()
    .filter(|s| **s == RequestState::Completed)
    .count()
}

fn main() {
  for mode in [WakeMode::PollEveryTick, WakeMode::OnNotify] {
    let (tick_ns, polls, completed) = run_async!(otter_core::Mesh::new(), mode);
    println!(
      "scheduler=stackless mode={mode:?} tick_us={:.2} ns_per_instance_tick={:.1} polls={polls} completed_requests={completed}",
      tick_ns / 1000.0,
      tick_ns / INSTANCES as f64,
    );
  }
}
