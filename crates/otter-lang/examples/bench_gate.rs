//! The instruction gate's driver (docs/performance-gates.md): runs one
//! workload for a given number of iterations and prints the allocations
//! and bytes the whole process requested. `bench/instructions/run.py` runs
//! it at N and 2N iterations, natively for the allocation counts and under
//! Cachegrind for the instruction counts, and divides the difference by N,
//! so process start, parsing, compose and teardown cancel out.
//!
//! Usage: cargo run --release -p otter-lang --example bench_gate -- WORKLOAD...
//! - `script PATH`: runs a script (the VM suite's generated workloads); its
//!   log lines go to stdout for the caller to verify.
//! - `entity INSTANCES TICKS`: the entity workload (`bench_instances`),
//!   `TICKS` steady ticks after every instance has started.
//! - `depth DEPTH INSTANCES TICKS`: suspend and resume through `DEPTH`
//!   nested calls (`bench_depth --progress`).
//! - `table CASE ITERATIONS`: one of `bench_tables`' VM cases.
//! - `reload CYCLES`: a preserving reload of an edited function, then two
//!   ticks, per cycle.
//!
//! Every workload verifies the work it did, and fails otherwise.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use otter_core::{Catalog, Mesh, Type, Var, bench};
use otter_lang::{Program, Session, Source, render};

struct CountingAlloc;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);

// SAFETY: forwards to the system allocator and only adds counting.
unsafe impl GlobalAlloc for CountingAlloc {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    ALLOCS.fetch_add(1, Ordering::Relaxed);
    BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
    // SAFETY: same contract as the caller's.
    unsafe { System.alloc(layout) }
  }

  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    // SAFETY: same contract as the caller's.
    unsafe { System.dealloc(ptr, layout) }
  }

  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    ALLOCS.fetch_add(1, Ordering::Relaxed);
    BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
    // SAFETY: same contract as the caller's.
    unsafe { System.realloc(ptr, layout, new_size) }
  }
}

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

fn catalog() -> Catalog {
  Catalog::new(&[otter_core::shards::CATALOG]).expect("catalog")
}

fn int(args: &[String], i: usize, what: &str) -> i64 {
  args
    .get(i)
    .unwrap_or_else(|| panic!("missing {what}"))
    .parse()
    .unwrap_or_else(|_| panic!("{what} must be an integer"))
}

fn script(path: &str) {
  let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
  let program = match Program::load(Source::new(path, text), &catalog(), &HashMap::new()) {
    Ok(p) => p,
    Err((source, diagnostics)) => {
      for d in &diagnostics {
        eprint!("{}", render(d, &source));
      }
      panic!("{path} does not compose");
    }
  };
  let report = program.run().unwrap_or_else(|d| panic!("{path}: {d:?}"));
  assert!(report.succeeded(), "{path} failed: {report:?}");
}

fn entity(instances: i64, ticks: i64) {
  let mut mesh = Mesh::new();
  for (name, value, mutable) in bench::mesh_vars(instances) {
    mesh.declare_var(name, value, mutable);
  }
  for def in bench::functions() {
    mesh.add_function(def);
  }
  for def in bench::wires() {
    mesh.add_wire(def);
  }
  let spawner = mesh.compile("spawner", Type::none()).expect("compile");
  mesh.spawn(&spawner, Var::None).expect("spawn");
  while mesh.get_var("ready-count") != Some(Var::Int(instances)) {
    mesh.tick();
  }
  let Some(Var::Int(before)) = mesh.get_var("iterations") else {
    unreachable!()
  };
  for _ in 0..ticks {
    mesh.tick();
  }
  // An entity iteration spans two ticks (it suspends once, in `Think`).
  let Some(Var::Int(after)) = mesh.get_var("iterations") else {
    unreachable!()
  };
  let done = after - before;
  assert!(
    (done - instances * ticks / 2).abs() <= instances,
    "{done} entity iterations in {ticks} ticks of {instances} instances"
  );
}

fn depth(depth: i64, instances: i64, ticks: i64) {
  let mut mesh = Mesh::new();
  mesh.declare_var("resumes", Var::Int(0), true);
  for def in bench::depth::functions(depth as usize, bench::depth::progress_leaf()) {
    mesh.add_function(def);
  }
  for def in bench::depth::wires() {
    mesh.add_wire(def);
  }
  let root = mesh.compile("root", Type::none()).expect("compile");
  for _ in 0..instances {
    mesh.spawn(&root, Var::None).expect("spawn");
  }
  for _ in 0..ticks {
    mesh.tick();
  }
  // Every tick counts a resume and pauses again.
  assert_eq!(mesh.get_var("resumes"), Some(Var::Int(instances * ticks)));
}

fn table(case: &str, iterations: i64) {
  let (_, body) = bench::tables::cases()
    .into_iter()
    .find(|(name, _)| *name == case)
    .unwrap_or_else(|| panic!("unknown table case {case}"));
  let mut mesh = Mesh::new();
  mesh.add_wire(bench::tables::wire(body, iterations));
  let compiled = mesh.compile("bench", Type::none()).expect("compile");
  mesh.spawn(&compiled, Var::None).expect("spawn");
  mesh.run(10);
  assert_eq!(mesh.running(), 0, "{case} did not finish");
}

fn reload_source(value: i64) -> Source {
  Source::new(
    "reload.shs",
    format!(
      r#"@fn(Inner input: None output: Int params: {{}} {{ Pause() {value} }})
@fn(Outer input: None output: Int params: {{}} {{ Inner }})
@wire(main {{ Keep(n 0) Inc(n) Outer | Math.Add(n) | Keep(last 0) | Update(last) }} looped: true)
@mesh(m) @schedule(m main) @run(m)"#
    ),
  )
}

fn reload(cycles: i64) {
  let catalog = catalog();
  let defines = HashMap::new();
  let mut session = Session::new();
  let preserve = |session: &mut Session, value| {
    let finished = session
      .reload_preserving(reload_source(value), &catalog, &defines)
      .unwrap_or_else(|(_, d)| panic!("preserving reload failed: {d:?}"));
    assert!(
      finished.is_empty(),
      "the reload restarted work: {finished:?}"
    );
  };
  preserve(&mut session, 0);
  for cycle in 1..=cycles {
    preserve(&mut session, cycle % 2);
    for _ in 0..2 {
      assert!(session.tick().is_empty(), "main finished");
    }
  }
  assert_eq!(session.running(), 1);
}

fn main() {
  let args: Vec<String> = std::env::args().skip(1).collect();
  match args.first().map(String::as_str) {
    Some("script") => script(args.get(1).expect("script PATH")),
    Some("entity") => entity(int(&args, 1, "INSTANCES"), int(&args, 2, "TICKS")),
    Some("depth") => depth(
      int(&args, 1, "DEPTH"),
      int(&args, 2, "INSTANCES"),
      int(&args, 3, "TICKS"),
    ),
    Some("table") => table(
      args.get(1).expect("table CASE ITERATIONS"),
      int(&args, 2, "ITERATIONS"),
    ),
    Some("reload") => reload(int(&args, 1, "CYCLES")),
    _ => {
      eprintln!(
        "usage: bench_gate script PATH | entity INSTANCES TICKS | depth DEPTH INSTANCES TICKS | table CASE ITERATIONS | reload CYCLES"
      );
      std::process::exit(2);
    }
  }
  println!(
    "GATE_ALLOCS: {}\nGATE_BYTES: {}",
    ALLOCS.load(Ordering::Relaxed),
    BYTES.load(Ordering::Relaxed)
  );
}
