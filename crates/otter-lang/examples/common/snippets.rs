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
];

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
