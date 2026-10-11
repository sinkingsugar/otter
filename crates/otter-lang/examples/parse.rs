//! Parses files and prints their syntax problems as JSON diagnostics.
//! `cargo run -p otter-lang --example parse -- a.shs b.shs`

fn main() {
  let mut failed = 0;
  for path in std::env::args().skip(1) {
    let text = match std::fs::read_to_string(&path) {
      Ok(t) => t,
      Err(e) => {
        eprintln!("{path}: {e}");
        failed += 1;
        continue;
      }
    };
    let source = otter_lang::Source::new(path.clone(), text);
    let (_, problems) = otter_lang::parse(&source);
    if !problems.is_empty() {
      failed += 1;
    }
    for p in problems {
      println!("{}", p.to_diagnostic(&source).to_json());
    }
  }
  std::process::exit(i32::from(failed > 0));
}
