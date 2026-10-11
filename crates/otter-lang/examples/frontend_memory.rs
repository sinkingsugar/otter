//! Reproducible frontend peak-memory probes for the device acceptance fixtures.
//! Run in release with docs disabled. Counts requested heap bytes, not RSS or
//! allocator overhead. Native results are not device budgets. No timing claims.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use otter_core::Catalog;
use otter_lang::{Program, Source};

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
// SAFETY: allocation and deallocation use System with the original layout and
// pointer. Counters never dereference allocations or alter their lifetimes.
unsafe impl GlobalAlloc for Counting {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    let ptr = unsafe { System.alloc(layout) };
    if !ptr.is_null() {
      let live = LIVE.fetch_add(layout.size(), Relaxed) + layout.size();
      PEAK.fetch_max(live, Relaxed);
    }
    ptr
  }
  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    LIVE.fetch_sub(layout.size(), Relaxed);
    unsafe { System.dealloc(ptr, layout) };
  }
  // Default realloc deliberately counts allocate/copy/free, consistently across
  // compared revisions. It can overstate an allocator's in-place growth peak.
}
#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn measure<T>(fixture: &str, phase: &str, f: impl FnOnce() -> T) -> T {
  let before = LIVE.load(Relaxed);
  PEAK.store(before, Relaxed);
  let result = f();
  let live = LIVE.load(Relaxed);
  let peak = PEAK.load(Relaxed);
  println!("{fixture},{phase},{before},{live},{peak}");
  result
}

fn main() {
  let catalog = Catalog::new(&[otter_core::shards::CATALOG]).unwrap();
  println!("fixture,phase,before_bytes,live_bytes,peak_bytes");
  // Function chains, as `deep_do_chains_are_a_diagnostic_not_a_crash`
  // composes them: 20 deep runs on the device, 80 is `too-deep`.
  let chain = |n: usize| {
    let mut src = String::new();
    for i in 0..n {
      src.push_str(&format!(
        "@fn(W{i} input: None output: Int params: {{}} {{W{}}})\n",
        i + 1
      ));
    }
    src + &format!("@fn(W{n} input: None output: Int params: {{}} {{1}})\nW0")
  };
  let mut nested = String::from("1");
  for _ in 0..30 {
    nested = format!("When({{true}} {{{nested}}})");
  }
  // Each level a function whose body nests the next call in a `When`.
  let mut wrapped = String::new();
  let n = (otter_core::compose::MAX_FLOW_DEPTH - 2) / 2;
  for i in 0..n {
    wrapped.push_str(&format!(
      "@fn(W{i} input: Int output: Int params: {{}} {{When({{true}} {{W{}}})}})\n",
      i + 1
    ));
  }
  wrapped.push_str(&format!(
    "@fn(W{n} input: Int output: Int params: {{}} {{Math.Add(1)}})\n1 | W0"
  ));
  let mut entries = String::new();
  for i in 0..64 {
    entries.push_str(&format!("@wire(e{i} {{{i}}})\n"));
  }
  entries.push_str("@wire(ticker {Pause} looped: true)\n@mesh(m)\n");
  for i in 0..64 {
    entries.push_str(&format!("@schedule(m e{i})\n"));
  }
  entries.push_str("@schedule(m ticker)\n@run(m iterations: 3)");
  for (name, text, valid) in [
    ("entries65", entries, true),
    ("fn20", chain(20), true),
    ("fn80", chain(80), false),
    ("when30", nested, true),
    ("wrapped_when", wrapped, true),
  ] {
    let source = Source::new("memory.shs", text);
    let (tree, problems) = measure(name, "parse", || otter_lang::parse(&source));
    assert!(problems.is_empty());
    drop(tree);
    drop(problems);
    let program = measure(name, "load", || {
      Program::load(source, &catalog, &Default::default()).unwrap_or_else(|(_, d)| panic!("{d:?}"))
    });
    let diagnostics = measure(name, "compose", || program.compose());
    assert_eq!(diagnostics.is_empty(), valid);
    if !valid {
      assert_eq!(diagnostics[0].code, "too-deep");
    }
    drop(diagnostics);
    let report = measure(name, "analyze", || program.analyze());
    assert_eq!(report.ok(), valid);
    drop(report);
    if valid {
      let report = measure(name, "run", || program.run().unwrap());
      assert!(report.succeeded());
    }
  }
}
