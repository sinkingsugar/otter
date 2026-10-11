//! Table storage benchmark (golden path §7.3, docs/values-and-types.md §7):
//! small fixed records built and read through the VM, a retained snapshot
//! followed by a rebuild, same-shape rebuilds within one constructor
//! segment, and growing dynamic tables through the host API. Reports time
//! and allocations per iteration, so the same binary built before and after
//! a storage change gives the comparison.
//!
//! Usage: cargo run --release --example bench_tables -- [iterations]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use otter_core::bench::tables::key;
use otter_core::{Mesh, ShardDef, Table, Type, Var, bench};

struct CountingAlloc;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards to the system allocator and only adds counting.
unsafe impl GlobalAlloc for CountingAlloc {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    ALLOCS.fetch_add(1, Ordering::Relaxed);
    BYTES.fetch_add(layout.size(), Ordering::Relaxed);
    // SAFETY: same contract as the caller's.
    unsafe { System.alloc(layout) }
  }

  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    // SAFETY: same contract as the caller's.
    unsafe { System.dealloc(ptr, layout) }
  }

  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    ALLOCS.fetch_add(1, Ordering::Relaxed);
    BYTES.fetch_add(new_size, Ordering::Relaxed);
    // SAFETY: same contract as the caller's.
    unsafe { System.realloc(ptr, layout, new_size) }
  }
}

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

struct Sample {
  ns: f64,
  allocs: f64,
  bytes: f64,
}

fn measure(iterations: usize, mut run: impl FnMut()) -> Sample {
  run();
  let allocs = ALLOCS.load(Ordering::Relaxed);
  let bytes = BYTES.load(Ordering::Relaxed);
  let t = Instant::now();
  run();
  let ns = t.elapsed().as_secs_f64() * 1e9;
  let n = iterations as f64;
  Sample {
    ns: ns / n,
    allocs: (ALLOCS.load(Ordering::Relaxed) - allocs) as f64 / n,
    bytes: (BYTES.load(Ordering::Relaxed) - bytes) as f64 / n,
  }
}

fn vm(name: &str, iterations: usize, body: Vec<ShardDef>) {
  let def = bench::tables::wire(body, iterations as i64);
  let sample = measure(iterations, || {
    let mut mesh = Mesh::new();
    mesh.add_wire(def.clone());
    let compiled = mesh.compile("bench", Type::none()).expect("compiles");
    mesh.spawn(&compiled, Var::None).expect("spawns");
    mesh.run(10);
    assert_eq!(mesh.running(), 0, "{name} did not finish");
  });
  report(name, &sample);
}

fn host(name: &str, iterations: usize, mut body: impl FnMut()) {
  let sample = measure(iterations, || {
    for _ in 0..iterations {
      body();
    }
  });
  report(name, &sample);
}

fn report(name: &str, s: &Sample) {
  println!(
    "{name:<16} ns_per_iter={:>8.1} allocs_per_iter={:>6.2} bytes_per_iter={:>8.1}",
    s.ns, s.allocs, s.bytes
  );
}

fn main() {
  let iterations: usize = std::env::args()
    .nth(1)
    .map(|s| s.parse().expect("iterations must be an integer"))
    .unwrap_or(200_000);
  println!(
    "iterations={iterations} var_bytes={} var_align={}",
    std::mem::size_of::<Var>(),
    std::mem::align_of::<Var>()
  );
  for (name, body) in bench::tables::cases() {
    vm(name, iterations, body);
  }

  // Host API: a growing dynamic table, and deriving a changed table from a
  // shared one.
  host("build-64", iterations / 10, || {
    let mut b = Table::builder();
    for i in 0..64 {
      b.insert(key(i), Var::Int(i as i64));
    }
    std::hint::black_box(b.build());
  });
  let shared = (0..64)
    .map(|i| (key(i), Var::Int(i as i64)))
    .collect::<Table>();
  let keep = shared.clone();
  host("derive-64", iterations / 10, || {
    let changed = shared
      .clone()
      .into_builder()
      .with("k07", Var::Int(-1))
      .build();
    std::hint::black_box(changed);
  });
  drop(keep);
}
