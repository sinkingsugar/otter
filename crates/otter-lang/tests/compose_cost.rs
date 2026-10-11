//! What compose costs, checked without timing (bench/compose/README.md):
//! allocations counted on the test's own thread, compared between a
//! workload and a twin holding as many values that expand to what they
//! hold, and walks over values that are astronomically large as paths
//! bounded by a watchdog, so a regression fails instead of hanging. Native
//! only (a global allocator and threads); `bench_compose` measures time.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use otter_core::{Catalog, Var};
use otter_lang::{Program, Session, Source};

thread_local! {
  static BYTES: Cell<usize> = const { Cell::new(0) };
}

struct Counting;
// SAFETY: forwards to the system allocator with the caller's layout and
// pointer; the thread-local counter is const-initialized and never
// allocates, and `try_with` skips threads being torn down.
unsafe impl GlobalAlloc for Counting {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    let _ = BYTES.try_with(|b| b.set(b.get() + layout.size()));
    // SAFETY: same contract as the caller's.
    unsafe { System.alloc(layout) }
  }
  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    // SAFETY: same contract as the caller's.
    unsafe { System.dealloc(ptr, layout) }
  }
}
#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Bytes `f` allocates on this thread.
fn allocated(f: impl FnOnce()) -> usize {
  let before = BYTES.with(Cell::get);
  f();
  BYTES.with(Cell::get) - before
}

/// Runs `f` on its own thread, failing the test if it takes longer than a
/// minute (a walk path by path over these values would never end).
fn bounded(what: &str, f: impl FnOnce() + Send + 'static) {
  let (done, finished) = mpsc::channel();
  let worker = std::thread::spawn(move || {
    f();
    let _ = done.send(());
  });
  match finished.recv_timeout(Duration::from_secs(60)) {
    Ok(()) => worker.join().unwrap(),
    Err(mpsc::RecvTimeoutError::Disconnected) => {
      // The worker panicked: report its panic.
      worker.join().unwrap();
    }
    Err(mpsc::RecvTimeoutError::Timeout) => panic!("{what}: still running after a minute"),
  }
}

fn catalog() -> Catalog {
  Catalog::new(&[otter_core::shards::CATALOG]).unwrap()
}

/// Constants each holding the level below 8 times, or once and 7 integers
/// (`flat`: the same number of values, expanding to what it holds).
fn shared_reads(levels: usize, reads: usize, flat: bool) -> String {
  let mut src = String::from("@const(t0 {a: 1})\n");
  for i in 1..=levels {
    let p = format!("@t{}", i - 1);
    let q = if flat { "1".to_string() } else { p.clone() };
    src += &format!("@const(t{i} [{p} {q} {q} {q} {q} {q} {q} {q}])\n");
  }
  for i in 0..reads {
    src += &format!("@t{levels} | Count = c{i}\n");
  }
  src + &format!("c{} | Log\n", reads - 1)
}

fn map_tables_looped(levels: usize, reads: usize, flat: bool) -> String {
  let mut src = String::from("@const(t0 {a: 1})\n");
  for i in 1..=levels {
    let p = format!("@t{}", i - 1);
    let q = if flat { "1".to_string() } else { p.clone() };
    src += &format!("@const(t{i} {{a: {p} b: {q}}})\n");
  }
  src += "@wire(w {\n";
  for _ in 0..reads {
    src += &format!("@t{levels} | Count | Math.Add(1)\n");
  }
  src + "Pause } looped: true)\n@mesh(m) @schedule(m w) @run(m)\n"
}

fn compose(src: &str, catalog: &Catalog) {
  let program = Program::load(Source::new("cost.shs", src), catalog, &HashMap::new())
    .map_err(|(_, d)| d)
    .expect("loads");
  let diagnostics = program.compose();
  assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn composing_a_shared_constant_allocates_what_it_holds() {
  // 5 levels: 32,768 tables as paths, 6 values as held. Before compose
  // kept constants shared, each read allocated about 3.5 MB here.
  let catalog = catalog();
  compose(&shared_reads(5, 2, false), &catalog); // interning, once
  let shared = allocated(|| compose(&shared_reads(5, 200, false), &catalog));
  let flat = allocated(|| compose(&shared_reads(5, 200, true), &catalog));
  assert!(
    shared * 4 <= flat * 5,
    "{shared} bytes for shared copies, {flat} for the flat twin"
  );
}

#[test]
fn a_preserving_reload_of_shared_map_tables_allocates_what_they_hold() {
  let catalog = catalog();
  let reload = |flat: bool| {
    let src = map_tables_looped(14, 100, flat);
    let mut session = Session::new();
    session
      .reload(Source::new("cost.shs", &src), &catalog, &HashMap::new())
      .map_err(|(_, d)| d)
      .expect("loads");
    allocated(|| {
      session
        .reload_preserving(Source::new("cost.shs", &src), &catalog, &HashMap::new())
        .map_err(|(_, d)| d)
        .expect("reloads");
    })
  };
  reload(false); // interning, once
  let (shared, flat) = (reload(false), reload(true));
  assert!(
    shared <= flat * 3,
    "{shared} bytes for shared copies, {flat} for the flat twin"
  );
  // The preserving reload's equality walks each held pair once: bounded.
  bounded("preserving reload", || {
    let catalog = crate::catalog();
    let src = map_tables_looped(14, 1000, false);
    let mut session = Session::new();
    for _ in 0..2 {
      session
        .reload_preserving(Source::new("cost.shs", &src), &catalog, &HashMap::new())
        .map_err(|(_, d)| d)
        .expect("reloads");
    }
  });
}

#[test]
fn walks_over_values_built_from_shared_copies_end() {
  // 64 levels each holding the level below twice: 2^64 paths. Conversion,
  // typing, admits, equality (sequences, map, struct and mixed tables)
  // and the cache-key hash each visit what is held.
  bounded("walks over shared values", || {
    let seqs = |leaf: i64| {
      let mut v = Var::table([("a", Var::Int(leaf))]);
      for _ in 0..64 {
        v = Var::from_seq(Arc::new(vec![v.clone(), v]));
      }
      v
    };
    let tables = |leaf: i64| {
      let mut v = Var::table([("a", Var::Int(leaf))]);
      for _ in 0..64 {
        v = Var::table([("a", v.clone()), ("b", v)]);
      }
      v
    };
    for build in [&seqs as &dyn Fn(i64) -> Var, &tables] {
      let (v, w) = (build(1), build(1));
      let converted = v.clone().into_struct_tables();
      assert_eq!(converted.type_of(), w.type_of());
      assert!(v.type_of().admits(&converted));
      assert_eq!(v, w);
      assert_eq!(converted, w);
      assert_ne!(converted, build(2));
      let hash = |v: &Var| {
        use std::hash::{BuildHasher, RandomState};
        RandomState::new().hash_one(otter_core::ParamValue::Value(v.clone()))
      };
      let _ = hash(&v);
    }
  });
}
