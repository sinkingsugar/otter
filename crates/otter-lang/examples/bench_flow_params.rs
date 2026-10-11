//! Flow parameters against the native loop (docs/metaprogramming.md §3.4):
//! summing a sequence's elements into a caller variable with a `While`
//! written in place, with a function running a block per element whose
//! call is inlined with its block (`Each`, which takes the length as a
//! parameter), with the same loop as a function that counts the sequence
//! itself (`ForEach`, inlined too since `Count` has a VM form), and with a
//! function whose body has a shard without one (`Framed`: `ExpectSeq`), so
//! the call keeps its frame and each element runs the block in a frame of
//! its own.
//!
//! Usage: cargo run --release -p otter-lang --example bench_flow_params
//!
//! Prints nanoseconds per element, timed by the script around its own
//! loop (so loading and composing are not in it), the median and the
//! fastest of `RUNS` samples. Samples of the three variants interleave, in
//! a rotating order, and every sample checks the sum the loop produced.

use std::collections::HashMap;

use otter_core::Catalog;
use otter_lang::{Program, Source};

const RUNS: usize = 9;
const WIDTHS: [usize; 3] = [8, 64, 256];
/// Elements per sample.
const ELEMENTS: usize = 400_000;

const FUNCTIONS: &str = r#"@fn(Each input: [Int] output: [Int] params: {n: Int action: Flow(input: Int)} {
  = xs
  0 | Var(i)
  While({i | IsLess(n)} { xs | Take(i) | Run(action) Inc(i) })
  xs
})
@fn(ForEach input: [Int] output: [Int] params: {action: Flow(input: Int)} {
  = xs
  0 | Var(i)
  xs | Count = n
  While({i | IsLess(n)} { xs | Take(i) | Run(action) Inc(i) })
  xs
})
@fn(Framed input: [Int] output: [Int] params: {action: Flow(input: Int)} {
  ExpectSeq = xs
  0 | Var(i)
  xs | Count = n
  While({i | IsLess(n)} { xs | Take(i) | Run(action) Inc(i) })
  xs
})
"#;

fn program(variant: &str, width: usize, repeat: usize) -> String {
  let block = "Math.Add(total) | Update(total)";
  let body = match variant {
    "native" => {
      format!("0 | Var(i) While({{i | IsLess({width})}} {{ xs | Take(i) | {block} Inc(i) }})")
    }
    "inlined" => format!("xs | Each(n: {width} action: {{{block}}})"),
    "counted" => format!("xs | ForEach(action: {{{block}}})"),
    "framed" => format!("xs | Framed(action: {{{block}}})"),
    _ => unreachable!(),
  };
  let items: Vec<String> = (0..width).map(|i| i.to_string()).collect();
  format!(
    "{FUNCTIONS}[{}] = xs\n0 | Var(total)\nTime.Now = t0\nRepeat({{ {body} }} times: {repeat})\nTime.Now | Math.Subtract(t0) | Log(\"SECONDS\")\ntotal | Log(\"TOTAL\")",
    items.join(" ")
  )
}

/// One run: the loop's seconds, after checking its sum.
fn sample(program: &Program, expected: i64) -> f64 {
  let (report, lines) = otter_core::log::capture(|| {
    program
      .run()
      .unwrap_or_else(|d| panic!("run failed: {d:?}"))
  });
  assert!(report.succeeded());
  let value = |label: &str| {
    lines
      .iter()
      .find_map(|line| line.strip_prefix(&format!("{label}: ")))
      .unwrap_or_else(|| panic!("no {label} in {lines:?}"))
      .to_string()
  };
  assert_eq!(value("TOTAL"), expected.to_string(), "wrong sum");
  value("SECONDS").parse().expect("seconds")
}

fn main() {
  let catalog = Catalog::new(&[otter_core::shards::CATALOG]).unwrap();
  let variants = ["native", "inlined", "counted", "framed"];
  println!(
    "{:>6} {:>10} {:>10} {:>10} {:>10} {:>9} {:>9} {:>9}   (medians; min per variant)",
    "width", "native ns", "inlined", "counted", "framed", "inlined/", "counted/", "framed/"
  );
  for width in WIDTHS {
    let repeat = ELEMENTS / width;
    let expected = (repeat * width * (width - 1) / 2) as i64;
    let programs: Vec<Program> = variants
      .iter()
      .map(|v| {
        Program::load(
          Source::new("bench.shs", program(v, width, repeat)),
          &catalog,
          &HashMap::new(),
        )
        .unwrap_or_else(|(_, d)| panic!("load failed: {d:?}"))
      })
      .collect();
    let mut samples = vec![Vec::new(); variants.len()];
    for run in 0..RUNS {
      for k in 0..variants.len() {
        let v = (run + k) % variants.len();
        let seconds = sample(&programs[v], expected);
        samples[v].push(seconds * 1e9 / (repeat * width) as f64);
      }
    }
    let stat = |v: usize| {
      let mut s = samples[v].clone();
      s.sort_by(f64::total_cmp);
      (s[s.len() / 2], s[0])
    };
    let stats: Vec<(f64, f64)> = (0..variants.len()).map(stat).collect();
    let median = |v: usize| stats[v].0;
    println!(
      "{width:>6} {:>10.2} {:>10.2} {:>10.2} {:>10.2} {:>9.2} {:>9.2} {:>9.2}   min {:.2} {:.2} {:.2} {:.2}",
      median(0),
      median(1),
      median(2),
      median(3),
      median(1) / median(0),
      median(2) / median(0),
      median(3) / median(0),
      stats[0].1,
      stats[1].1,
      stats[2].1,
      stats[3].1
    );
  }
}
