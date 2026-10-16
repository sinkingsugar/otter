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
//! - `object CASE ITERATIONS`: host objects (docs/host-requirements.md H1)
//!   copied between locals (`assign`), into sequences (`seq`) and tables
//!   (`table`), and a service looked up at activation (`lookup`, H3).
//!
//! Every workload verifies the work it did, and fails otherwise.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use otter_core::{Catalog, Mesh, Outcome, Type, Var, bench};
use otter_lang::{Program, Session, Source, render};

/// Counts allocations, and serves them from size classes so that their
/// cost does not depend on the heap's history.
///
/// glibc's cost per call depends on the free chunks around a block, so
/// any change to what a process allocates before a workload (a larger
/// catalog, say) moved a workload's count with no change on its path:
/// `vm/push-shared-seq` rose 2 to 11 percent, every instruction of it in
/// `malloc.c`. Here a block goes back to its class's free list and is
/// reused from it, so after warm-up an allocation is the same few
/// instructions whatever ran before. Allocation counts and bytes keep
/// measuring what the runtime asks for.
struct CountingAlloc {
  lock: AtomicBool,
  /// Free blocks per class, linked through their first word.
  free: UnsafeCell<[*mut u8; CLASSES]>,
}

// SAFETY: `free` is only touched while `lock` is held.
unsafe impl Sync for CountingAlloc {}

/// Powers of two from 16 bytes to 1 MiB; larger blocks go to the system.
const CLASSES: usize = 17;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);

fn class(layout: Layout) -> Option<usize> {
  let size = layout.size().max(layout.align()).max(16);
  let class = size.next_power_of_two().trailing_zeros() as usize - 4;
  (class < CLASSES).then_some(class)
}

impl CountingAlloc {
  fn locked<R>(&self, f: impl FnOnce(&mut [*mut u8; CLASSES]) -> R) -> R {
    while self
      .lock
      .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
      .is_err()
    {
      std::hint::spin_loop();
    }
    // SAFETY: the lock is held.
    let r = f(unsafe { &mut *self.free.get() });
    self.lock.store(false, Ordering::Release);
    r
  }

  /// SAFETY: as `GlobalAlloc::alloc`.
  unsafe fn take(&self, layout: Layout) -> *mut u8 {
    let Some(c) = class(layout) else {
      // SAFETY: same contract as the caller's.
      return unsafe { System.alloc(layout) };
    };
    let block = self.locked(|free| {
      let block = free[c];
      if !block.is_null() {
        // SAFETY: a free block holds the next one in its first word.
        free[c] = unsafe { *(block as *mut *mut u8) };
      }
      block
    });
    if !block.is_null() {
      return block;
    }
    // A class's block is aligned to its size, so it serves any alignment
    // its class covers.
    // SAFETY: a nonzero power-of-two size and alignment.
    unsafe { System.alloc(Layout::from_size_align_unchecked(16 << c, 16 << c)) }
  }

  /// SAFETY: as `GlobalAlloc::dealloc`.
  unsafe fn give(&self, ptr: *mut u8, layout: Layout) {
    let Some(c) = class(layout) else {
      // SAFETY: same contract as the caller's.
      return unsafe { System.dealloc(ptr, layout) };
    };
    self.locked(|free| {
      // SAFETY: the block is at least 16 bytes and no longer in use.
      unsafe { *(ptr as *mut *mut u8) = free[c] };
      free[c] = ptr;
    });
  }
}

// SAFETY: blocks come from the system allocator, each used by one owner
// until freed; a class's block is aligned to and as large as its class.
unsafe impl GlobalAlloc for CountingAlloc {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    ALLOCS.fetch_add(1, Ordering::Relaxed);
    BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
    // SAFETY: same contract as the caller's.
    unsafe { self.take(layout) }
  }

  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    // SAFETY: same contract as the caller's.
    unsafe { self.give(ptr, layout) }
  }

  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    ALLOCS.fetch_add(1, Ordering::Relaxed);
    BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
    // SAFETY: the caller guarantees a valid size for this alignment.
    let new = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
    if class(layout).is_some() && class(layout) == class(new) {
      return ptr;
    }
    // SAFETY: same contract as the caller's; the copy covers what both hold.
    unsafe {
      let moved = self.take(new);
      if !moved.is_null() {
        std::ptr::copy_nonoverlapping(ptr, moved, layout.size().min(new_size));
        self.give(ptr, layout);
      }
      moved
    }
  }
}

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc {
  lock: AtomicBool::new(false),
  free: UnsafeCell::new([std::ptr::null_mut(); CLASSES]),
};

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

/// Host shards for the object workloads: an object, and a service lookup.
mod host {
  use otter_core::describe::{InputDesc, OutputDesc, Params, ShardDesc, Targets, TypeName};
  use otter_core::instance::{InstanceCtx, LeafCtx};
  use otter_core::object::{Object, ObjectType};
  use otter_core::shard::Flow;
  use otter_core::shards::leaf::{LeafShard, leaf_type};
  use otter_core::{Args, ComposeCtx, Composed, Error, Result, ShardType, Var};

  pub static HANDLE: ObjectType = ObjectType::new("Gate.Handle");

  pub struct Handle;

  impl Object for Handle {
    fn object_type(&self) -> &'static ObjectType {
      &HANDLE
    }
  }

  /// The service `Gate.Lookup` reads.
  pub struct Registry;

  const fn desc(name: &'static str, input: InputDesc, output: OutputDesc) -> ShardDesc {
    ShardDesc {
      params: Params::Declared(&[]),
      input,
      output,
      targets: Targets::All,
      effects: otter_core::signature::Effects {
        io: true,
        ..otter_core::signature::Effects::NONE
      },
      lifetime: otter_core::signature::Lifetime::Stateless,
      ..ShardDesc::undocumented(name, 1)
    }
  }

  pub struct Open;

  impl LeafShard for Open {
    type Compiled = ();
    type State = ();
    const DESC: ShardDesc = desc(
      "Gate.Open",
      InputDesc::Ignored,
      OutputDesc::Fixed(TypeName::Object(&HANDLE)),
    );

    fn compose(_: &Args, _: &mut ComposeCtx<'_>) -> Result<Composed<()>> {
      Ok(Composed {
        compiled: (),
        output: HANDLE.to_type(),
      })
    }

    fn instantiate(_: &(), _: &mut InstanceCtx) -> Result<()> {
      Ok(())
    }

    fn activate(_: &(), _: &mut (), _: &mut impl LeafCtx, _: &Var) -> Result<Flow> {
      Ok(Flow::Next(Var::object(Handle)))
    }
  }

  pub struct Lookup;

  impl LeafShard for Lookup {
    type Compiled = ();
    type State = ();
    const DESC: ShardDesc = desc("Gate.Lookup", InputDesc::Any, OutputDesc::Passthrough);

    fn compose(_: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<()>> {
      Ok(Composed {
        compiled: (),
        output: ctx.input(),
      })
    }

    fn instantiate(_: &(), _: &mut InstanceCtx) -> Result<()> {
      Ok(())
    }

    fn activate(_: &(), _: &mut (), ctx: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
      match ctx.services().get::<Registry>() {
        Some(_) => Ok(Flow::Next(input.clone())),
        None => Err(Error::Activation("Gate.Lookup: no registry".into())),
      }
    }
  }

  pub static OPEN: ShardType = leaf_type::<Open>();
  pub static LOOKUP: ShardType = leaf_type::<Lookup>();
}

/// The object workloads' loop bodies: 8 operations, or one constructor of
/// 4 objects.
fn object_body(case: &str) -> &'static str {
  match case {
    "assign" => {
      "o | Update(slot) | Update(slot) | Update(slot) | Update(slot) | Update(slot) | Update(slot) | Update(slot) | Update(slot)"
    }
    "seq" => "[o o o o] | Update(items)",
    "table" => "{a: o b: o c: o d: o} | Update(record)",
    "lookup" => {
      "none | Gate.Lookup | Gate.Lookup | Gate.Lookup | Gate.Lookup | Gate.Lookup | Gate.Lookup | Gate.Lookup | Gate.Lookup"
    }
    _ => panic!("unknown object case {case}"),
  }
}

fn object(case: &str, iterations: i64) {
  let catalog = Catalog::new(&[otter_core::shards::CATALOG, &[&host::OPEN, &host::LOOKUP]])
    .and_then(|c| c.with_objects(&[&[&host::HANDLE]]))
    .expect("catalog");
  let text = format!(
    "Gate.Open | Var(o)\nGate.Open | Var(slot)\n[o] | Var(items)\n{{a: o b: o c: o d: o}} | Var(record)\n0 | Var(done)\nRepeat({{ {} Inc(done) }} times: {iterations})\ndone",
    object_body(case)
  );
  let mut services = otter_core::service::Services::default();
  services.insert(host::Registry);
  let mut session = Session::new();
  session.set_services(services);
  session
    .reload(Source::new("object.shs", text), &catalog, &HashMap::new())
    .unwrap_or_else(|(_, d)| panic!("{case}: {d:?}"));
  let finished = session.tick();
  match &finished[..] {
    [f] if f.outcome == Outcome::Completed(Var::Int(iterations)) => {}
    other => panic!("{case}: {other:?}"),
  }
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
    Some("object") => object(
      args.get(1).expect("object CASE ITERATIONS"),
      int(&args, 2, "ITERATIONS"),
    ),
    _ => {
      eprintln!(
        "usage: bench_gate script PATH | entity INSTANCES TICKS | depth DEPTH INSTANCES TICKS | table CASE ITERATIONS | reload CYCLES | object CASE ITERATIONS"
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
