//! Instance benchmark, the 2.0 counterpart of the 1.x baseline
//! (`shards/tests/bench-instances.sh`, design doc §5).
//!
//! Spawns N instances of the entity wire through a spawner wire, ticks until
//! every instance has run its first activation, and reports times, compose
//! counts and memory. Memory is reported three ways, per instance:
//! - `rss`: growth of the process's *current* resident set, sampled with
//!   `ps` (the same method as the 1.x harness);
//! - `heap`: growth of live heap bytes, counted by this binary's allocator.
//!   Coroutine stacks are mapped directly from the OS, so they are not in it;
//! - `state`: shard states plus the local frame, as the runtime accounts them.
//!
//! It also reports steady-state cost: after 10 warm-up ticks, 1000 ticks are
//! timed as a batch (total mesh time per tick, and per instance), and the
//! completed entity iterations are counted (expected: N * 500).
//!
//! `rss - heap` is then the non-heap part: touched stack pages, plus
//! allocator overhead.
//!
//! Usage: cargo run --release --example bench_instances -- [N] [--stackless]

use std::alloc::{GlobalAlloc, Layout, System};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use otter_core::{Type, Var, bench};

struct CountingAlloc;

static LIVE_HEAP: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards to the system allocator and only adds counting.
unsafe impl GlobalAlloc for CountingAlloc {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    // SAFETY: same contract as the caller's.
    let ptr = unsafe { System.alloc(layout) };
    if !ptr.is_null() {
      LIVE_HEAP.fetch_add(layout.size(), Ordering::Relaxed);
    }
    ptr
  }

  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    // SAFETY: same contract as the caller's.
    unsafe { System.dealloc(ptr, layout) };
    LIVE_HEAP.fetch_sub(layout.size(), Ordering::Relaxed);
  }

  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    // SAFETY: same contract as the caller's.
    let new = unsafe { System.realloc(ptr, layout, new_size) };
    if !new.is_null() {
      LIVE_HEAP.fetch_sub(layout.size(), Ordering::Relaxed);
      LIVE_HEAP.fetch_add(new_size, Ordering::Relaxed);
    }
    new
  }
}

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

/// Current resident set size in KB, via `ps` (works on macOS and Linux).
fn current_rss_kb() -> u64 {
  let out = Command::new("ps")
    .args(["-o", "rss=", "-p", &std::process::id().to_string()])
    .output()
    .expect("failed to run ps");
  String::from_utf8_lossy(&out.stdout)
    .trim()
    .parse()
    .expect("unexpected ps output")
}

/// The benchmark body, for either scheduler's mesh.
macro_rules! run_bench {
  ($mesh:expr, $n:expr, $scheduler:expr) => {{
    let n: i64 = $n;
    let mut mesh = $mesh;
    for (name, value, mutable) in bench::mesh_vars(n) {
      mesh.declare_var(name, value, mutable);
    }
    for def in bench::functions() {
      mesh.add_function(def);
    }
    for def in bench::wires() {
      mesh.add_wire(def);
    }

  let t = Instant::now();
  let spawner = mesh.compile("spawner", Type::none()).expect("compile");
  let compile_ms = t.elapsed().as_secs_f64() * 1000.0;
  let stats_compiled = mesh.cache_stats();

  let rss_before = current_rss_kb();
  let heap_before = LIVE_HEAP.load(Ordering::Relaxed);
  mesh.spawn(&spawner, Var::None).expect("spawn");

  // Tick 1: the spawner runs Spawn N times; the N instances are created at
  // the end of the tick.
  let t = Instant::now();
  mesh.tick();
  let spawn_ms = t.elapsed().as_secs_f64() * 1000.0;
  // Then tick until every instance has run its first activation.
  while mesh.get_var("ready-count") != Some(Var::Int(n)) {
    mesh.tick();
  }
  let ready_ms = t.elapsed().as_secs_f64() * 1000.0;
  let heap_after = LIVE_HEAP.load(Ordering::Relaxed);
  let rss_after = current_rss_kb();

  // Steady state: every instance runs its loop. One entity iteration spans
  // two ticks (it suspends once, in its nested Pause). Warm up, then time a
  // batch of ticks, and count completed iterations to verify the work.
  const WARMUP: u32 = 10;
  const TICKS: u32 = 1000;
  for _ in 0..WARMUP {
    mesh.tick();
  }
  let Some(Var::Int(it0)) = mesh.get_var("iterations") else { unreachable!() };
  let t = Instant::now();
  for _ in 0..TICKS {
    mesh.tick();
  }
  let tick_ns = t.elapsed().as_secs_f64() * 1e9 / f64::from(TICKS);
  let Some(Var::Int(it1)) = mesh.get_var("iterations") else { unreachable!() };
  let iterations = it1 - it0;

  let stats = mesh.cache_stats();
  assert_eq!(
    stats.wire_composes, stats_compiled.wire_composes,
    "spawning must not compose"
  );

  // Instance 0 is the spawner; entities follow.
  let memory = mesh
    .instance_ids()
    .get(1)
    .and_then(|id| mesh.memory(*id))
    .unwrap_or_default();
  let per_instance = |v: f64| if n > 0 { v / n as f64 } else { 0.0 };
  let rss_kb = rss_after.saturating_sub(rss_before) as f64;
  let heap_kb = heap_after.saturating_sub(heap_before) as f64 / 1024.0;

  println!(
    "scheduler={} instances={n} compile_ms={compile_ms:.3} spawn_ms={spawn_ms:.3} ready_ms={ready_ms:.3} \
     us_per_instance={:.2} tick_us={:.2} ns_per_instance_tick={:.1} iterations={} rss_kb_per_instance={:.2} heap_kb_per_instance={:.2} \
     non_heap_kb_per_instance={:.2} state_bytes_per_instance={} \
     wire_composes={} shard_composes={} var_bytes={}",
    $scheduler,
    per_instance(ready_ms * 1000.0),
    tick_ns / 1000.0,
    per_instance(tick_ns),
    iterations,
    per_instance(rss_kb),
    per_instance(heap_kb),
    per_instance(rss_kb - heap_kb),
    memory.state_bytes,
    stats.wire_composes,
    stats.shard_composes,
    std::mem::size_of::<Var>(),
  );
  }};
}

fn main() {
  let args: Vec<String> = std::env::args().skip(1).collect();
  let n: i64 = args
    .iter()
    .find(|a| !a.starts_with("--"))
    .map(|s| s.parse().expect("N must be an integer"))
    .unwrap_or(100);
  run_bench!(otter_core::Mesh::new(), n, "stackless");
}
