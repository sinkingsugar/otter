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

use otter_core::compose::WireDef;
use otter_core::shards::data::TABLE_MAKE;
use otter_core::shards::defs::*;
use otter_core::{Mesh, ShardDef, Table, Type, Var};

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

fn key(i: usize) -> String {
  format!("k{i:02}")
}

/// `Table.Make` of `n` keys reading the locals `v0..vn`.
fn table_make(n: usize) -> ShardDef {
  let keys = Var::from_seq(std::sync::Arc::new(
    (0..n).map(|i| Var::string(&key(i))).collect(),
  ));
  let mut params = vec![val(keys)];
  params.extend((0..n).map(|i| var(&format!("v{i}"))));
  ShardDef::new(&TABLE_MAKE, params)
}

fn literal_table(n: usize) -> Var {
  Var::table((0..n).map(|i| (key(i), Var::Int(i as i64))))
}

/// A wire that declares `v0..v15`, `acc`, `t` and `key`, then repeats
/// `body` `iterations` times.
fn wire(body: Vec<ShardDef>, iterations: i64) -> WireDef {
  let mut flow: Vec<ShardDef> = (0..16)
    .flat_map(|i| [konst(Var::Int(i)), declare(&format!("v{i}"))])
    .collect();
  flow.extend([
    konst(Var::Int(0)),
    declare("acc"),
    konst(literal_table(16)),
    declare("t"),
    konst(Var::string("k07")),
    declare("key"),
    repeat(body, val(Var::Int(iterations))),
  ]);
  WireDef {
    name: "bench".into(),
    looped: false,
    flow,
  }
}

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
  let def = wire(body, iterations as i64);
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
  let take = |k: &str| take(val(Var::string(k)));
  // Read a retained 16-key record by a literal key.
  vm(
    "const-take-16",
    iterations,
    vec![get("t"), take("k07"), update("acc")],
  );
  // Read it by a key held in a variable.
  vm(
    "var-key-take-16",
    iterations,
    vec![
      get("t"),
      otter_core::shards::defs::take(var("key")),
      ShardDef::new(&otter_core::shards::values::EXPECT_INT, vec![]),
      update("acc"),
    ],
  );
  // Build a record from locals and read one field.
  vm(
    "make-take-4",
    iterations,
    vec![table_make(4), take("k02"), update("acc")],
  );
  vm(
    "make-take-16",
    iterations,
    vec![table_make(16), take("k07"), update("acc")],
  );
  // Rebuild while the previous record is retained by `t`.
  vm("rebuild-16", iterations, vec![table_make(16), update("t")]);
  // Two same-shape constructors in one segment: the second reuses the
  // first's unique storage.
  vm("chain-16", iterations, vec![table_make(16), table_make(16)]);

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
