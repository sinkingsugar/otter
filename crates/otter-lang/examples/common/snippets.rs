//! Script workloads for the instruction gate (`bench_gate snippet`) and the
//! wall-clock comparison (`bench_snippets`): each repeats a loop body over
//! values its setup declares, and checks the result (docs/iteration-and-
//! access.md). Shared through `#[path]`, so both run the same scripts.

use otter_core::Var;

pub struct Snippet {
  pub name: &'static str,
  /// Declarations before the loop; `acc` (an Int variable) is declared
  /// after them.
  pub setup: &'static str,
  /// One iteration; it adds its work to `acc`.
  pub body: &'static str,
  /// What `acc` holds after `n` iterations.
  pub result: fn(i64) -> Var,
}

const JSON: &str = r#""{\"a\": {\"b\": [1, 2, 3]}}" | FromJson = j"#;

pub static SNIPPETS: &[Snippet] = &[
  // Paths (§1, §2): a static path, the same through Any, and lenient.
  Snippet {
    name: "path-static",
    setup: "{a: {b: [1 2 3]}} = t",
    body: "t.a.b.2 | Add(acc) | Update(acc)",
    result: |n| Var::Int(3 * n),
  },
  Snippet {
    name: "path-dynamic",
    setup: JSON,
    body: "j.a.b.2 | ExpectInt | Add(acc) | Update(acc)",
    result: |n| Var::Int(3 * n),
  },
  Snippet {
    name: "path-lenient",
    setup: JSON,
    body: "j.a.b.2? | ExpectInt | Add(acc) | Update(acc)",
    result: |n| Var::Int(3 * n),
  },
  // Iteration (§4) over 256 Ints, beside the hand-written loops it
  // replaces: `repeat-take` sums as `foreach-sum` does, `repeat-push`
  // maps as `map-256` does.
  Snippet {
    name: "repeat-take",
    setup: SEQ_256,
    body: "0 | Update(i)\nRepeat({ xs | Take(i) | Add(acc) | Update(acc) Inc(i) } times: (xs | Count))",
    result: |n| Var::Int(SUM_256 * n),
  },
  Snippet {
    name: "foreach-sum",
    setup: SEQ_256,
    body: "xs | ForEach({ Add(acc) | Update(acc) })",
    result: |n| Var::Int(SUM_256 * n),
  },
  Snippet {
    name: "foreach-named",
    setup: SEQ_256,
    body: "xs | ForEach(x j { x | Add(acc) | Update(acc) })",
    result: |n| Var::Int(SUM_256 * n),
  },
  Snippet {
    name: "repeat-push",
    setup: SEQ_256,
    body: "[] | ExpectIntSeq | Update(out)\n0 | Update(i)\nRepeat({ xs | Take(i) | Add(1) | Push(out) Inc(i) } times: (xs | Count))\nout | Count | Add(acc) | Update(acc)",
    result: |n| Var::Int(256 * n),
  },
  Snippet {
    name: "map-256",
    setup: SEQ_256,
    body: "xs | Map({ Add(1) }) | Count | Add(acc) | Update(acc)",
    result: |n| Var::Int(256 * n),
  },
  Snippet {
    name: "filter-256",
    setup: SEQ_256,
    body: "xs | Filter({ IsMore(127) }) | Count | Add(acc) | Update(acc)",
    result: |n| Var::Int(128 * n),
  },
  Snippet {
    name: "reduce-256",
    setup: SEQ_256,
    body: "xs | Reduce(a x { x | Add(a) } init: 0) | Add(acc) | Update(acc)",
    result: |n| Var::Int(SUM_256 * n),
  },
  Snippet {
    name: "table-foreach",
    setup: "{a: 1 b: 2 c: 3 d: 4 e: 5 f: 6 g: 7 h: 8 i: 9 j: 10 k: 11 l: 12 m: 13 n: 14 o: 15 p: 16} = t",
    body: "t | ForEach(v k { v | Add(acc) | Update(acc) })",
    result: |n| Var::Int(136 * n),
  },
];

/// `xs`, the Ints 0 to 255, and the variables the hand-written loops use.
const SEQ_256: &str = "#( 0 | Var(k) [] | ExpectIntSeq | Var(s) Repeat({ k | Push(s) Inc(k) } times: 256) s ) = xs\n0 | Var(i)\n[] | ExpectIntSeq | Var(out)";
const SUM_256: i64 = 255 * 256 / 2;

pub fn find(name: &str) -> &'static Snippet {
  SNIPPETS
    .iter()
    .find(|s| s.name == name)
    .unwrap_or_else(|| panic!("unknown snippet {name}"))
}

/// The script running `snippet`'s body `iterations` times; it outputs `acc`.
pub fn script(snippet: &Snippet, iterations: i64) -> String {
  format!(
    "{}\n0 | Var(acc)\nRepeat({{ {} }} times: {iterations})\nacc",
    snippet.setup, snippet.body
  )
}
