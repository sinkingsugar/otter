//! Compose benchmark (bench/compose/README.md): loading and composing
//! scripts, and preserving reloads, timed and with allocations counted.
//! Each shared-constant workload has a flat twin holding as many values,
//! with one copy of the level below per level instead of several, so it
//! expands to what it holds: their ratio says whether compose costs what a
//! value holds or what it expands to, on any machine.
//!
//! Usage: cargo run --release -p otter-lang --example bench_compose --
//!   [--check bench/compose/baseline.txt | --write bench/compose/baseline.txt]
//!
//! `--check` fails when a workload allocates more than 25% over its baseline
//! (allocation counts and bytes do not depend on the machine), or when a
//! workload takes more than `RATIO` times its flat twin.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

use otter_core::Catalog;
use otter_lang::{Program, Session, Source};

struct Counting;
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
// SAFETY: forwards to the system allocator with the caller's layout and
// pointer, and only adds counting.
unsafe impl GlobalAlloc for Counting {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    ALLOCS.fetch_add(1, Relaxed);
    BYTES.fetch_add(layout.size(), Relaxed);
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

/// How much slower than its flat twin a workload may be.
const RATIO: f64 = 4.0;
/// How much more than its baseline a workload may allocate.
const ALLOC_SLACK: f64 = 1.25;
/// Timed repetitions per workload; the fastest is reported.
const RUNS: usize = 5;

/// Constants of `levels` levels, each holding the level below 8 times (in
/// a sequence) or twice (in a table), read `reads` times; flat, each holds
/// it once and as many integers in place of the other copies.
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

/// Distinct literals: what most scripts hold.
fn literals(statements: usize) -> String {
  let mut src = String::new();
  for i in 0..statements {
    src += &format!("{{a: {i} b: [{i} 2 3] c: \"s{i}\"}} | Take(\"b\") | Count = v{i}\n");
  }
  src + &format!("v{} | Log\n", statements - 1)
}

fn functions(count: usize) -> String {
  let mut src = String::new();
  for i in 0..count {
    src += &format!(
      "@fn(F{i} input: Int output: Int params: {{k: Int}} {{ Math.Add(k) | Math.Multiply(2) }})\n"
    );
  }
  src += "0";
  for i in 0..count {
    src += &format!(" | F{i}(k: {i})");
  }
  src + " | Log\n"
}

struct Measured {
  name: &'static str,
  ns: f64,
  allocs: usize,
  bytes: usize,
}

fn measure(name: &'static str, mut op: impl FnMut()) -> Measured {
  op(); // warm-up: interned types and shapes, lazily built catalogs
  let mut best = Duration::MAX;
  let (mut allocs, mut bytes) = (usize::MAX, usize::MAX);
  for _ in 0..RUNS {
    let (a, b) = (ALLOCS.load(Relaxed), BYTES.load(Relaxed));
    let t = Instant::now();
    op();
    best = best.min(t.elapsed());
    allocs = allocs.min(ALLOCS.load(Relaxed) - a);
    bytes = bytes.min(BYTES.load(Relaxed) - b);
  }
  Measured {
    name,
    ns: best.as_nanos() as f64,
    allocs,
    bytes,
  }
}

fn main() {
  let args: Vec<String> = std::env::args().skip(1).collect();
  let catalog = Catalog::new(&[otter_core::shards::CATALOG]).unwrap();
  let defines = HashMap::new();
  let compose = |src: String| {
    let catalog = &catalog;
    let defines = &defines;
    move || {
      let program = Program::load(Source::new("bench.shs", &src), catalog, defines)
        .map_err(|(_, d)| d)
        .expect("loads");
      let diagnostics = program.compose();
      assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }
  };
  let reload = |src: String| {
    let catalog = &catalog;
    let defines = &defines;
    let mut session = Session::new();
    session
      .reload(Source::new("bench.shs", &src), catalog, defines)
      .map_err(|(_, d)| d)
      .expect("loads");
    move || {
      session
        .reload_preserving(Source::new("bench.shs", &src), catalog, defines)
        .map_err(|(_, d)| d)
        .expect("reloads");
    }
  };
  let results = [
    measure("shared-table-reads", compose(shared_reads(5, 1000, false))),
    measure(
      "shared-table-reads-flat",
      compose(shared_reads(5, 1000, true)),
    ),
    measure(
      "map-table-reload",
      reload(map_tables_looped(14, 200, false)),
    ),
    measure(
      "map-table-reload-flat",
      reload(map_tables_looped(14, 200, true)),
    ),
    measure("literals", compose(literals(1000))),
    measure("functions", compose(functions(200))),
  ];
  for m in &results {
    println!(
      "{:<26} ms={:>9.3} allocs={:>9} bytes={:>11}",
      m.name,
      m.ns / 1e6,
      m.allocs,
      m.bytes
    );
  }
  match args.first().map(String::as_str) {
    Some("--write") => {
      let path = args.get(1).expect("--write <file>");
      let mut out = String::from("# workload allocs bytes (bench_compose --write)\n");
      for m in &results {
        out += &format!("{} {} {}\n", m.name, m.allocs, m.bytes);
      }
      std::fs::write(path, out).expect("writes the baseline");
    }
    Some("--check") => {
      let path = args.get(1).expect("--check <file>");
      let baseline = std::fs::read_to_string(path).expect("reads the baseline");
      let mut failures = Vec::new();
      for line in baseline.lines().filter(|l| !l.starts_with('#')) {
        let f: Vec<&str> = line.split_whitespace().collect();
        let [name, allocs, bytes] = f[..] else {
          continue;
        };
        let Some(m) = results.iter().find(|m| m.name == name) else {
          failures.push(format!("{name}: in the baseline, not measured"));
          continue;
        };
        for (what, now, base) in [("allocs", m.allocs, allocs), ("bytes", m.bytes, bytes)] {
          let base: usize = base.parse().expect("a count");
          if now as f64 > base as f64 * ALLOC_SLACK {
            failures.push(format!("{name}: {what} {now} > {base} + 25%"));
          }
        }
      }
      for m in &results {
        let Some(flat) = results
          .iter()
          .find(|f| f.name == format!("{}-flat", m.name))
        else {
          continue;
        };
        let ratio = m.ns / flat.ns;
        println!("{:<26} {ratio:.2}x its flat twin", m.name);
        if ratio > RATIO {
          failures.push(format!(
            "{}: {ratio:.1}x its flat twin (limit {RATIO}x)",
            m.name
          ));
        }
      }
      if !failures.is_empty() {
        for f in &failures {
          eprintln!("FAIL {f}");
        }
        std::process::exit(1);
      }
      println!("compose costs within the baseline");
    }
    _ => {}
  }
}
