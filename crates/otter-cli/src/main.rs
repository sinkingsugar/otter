//! `otter`: check, run and describe Otter programs.
//!
//! ```text
//! otter check [--json] [-I dir ...] <file> [key:value ...]
//! otter run [--json] [-I dir ...] <file> [key:value ...]
//! otter watch [-I dir ...] <file> [key:value ...]
//! otter describe <shard>
//! otter search <text>
//! otter catalog
//! otter tool <file> [arguments-json]
//! ```
//!
//! `check --json` prints the 1.x `shards check --json` envelope
//! (`{ok, file, diagnostics}`). `run --json` prints one object after the
//! run instead of the log and result lines: `{ok, file, diagnostics, log,
//! outcomes, spawned_failures}`, where `log` holds the logged lines and
//! each outcome is `{wire, outcome, value?, error?}`. `key:value` arguments are the script's
//! `@key` values. Exit codes: 0 success, 1 problems or a failed run, 2
//! usage. `-I dir` adds a directory `@include` and `@read` search after
//! the including file's own. `tool` loads a tool script
//! (`otter_lang::Tool`): without arguments it prints the definition as
//! JSON, with a JSON object of arguments it calls the tool and prints its
//! output.

use std::collections::HashMap;
use std::process::ExitCode;

use otter_core::diagnostic::{Diagnostic, json_str};
use otter_core::{Catalog, Outcome};
use otter_lang::{CheckReport, FsFiles, Program, RunReport, Source, render};

mod watch;

const USAGE: &str = "usage:
  otter check [--json] [-I dir ...] <file> [key:value ...]
  otter run [--json] [-I dir ...] <file> [key:value ...]
  otter watch [-I dir ...] <file> [key:value ...]
  otter describe <shard>
  otter search <text>
  otter catalog
  otter tool <file> [arguments-json]";

fn catalog() -> Catalog {
  Catalog::new(&[
    otter_core::shards::CATALOG,
    otter_io::CATALOG,
    otter_text::CATALOG,
  ])
  .expect("catalog")
}

struct Options {
  json: bool,
  file: String,
  defines: HashMap<String, String>,
  include_paths: Vec<std::path::PathBuf>,
}

impl Options {
  /// Where `@include` and `@read` look: the filesystem, with `-I`.
  fn files(&self) -> FsFiles {
    FsFiles {
      include_paths: self.include_paths.clone(),
    }
  }
}

fn options(args: &[String]) -> Result<Options, String> {
  let mut json = false;
  let mut file = None;
  let mut defines = HashMap::new();
  let mut include_paths = Vec::new();
  let mut args = args.iter();
  while let Some(arg) = args.next() {
    match arg.as_str() {
      "--json" => json = true,
      "-I" => match args.next() {
        Some(dir) => include_paths.push(dir.into()),
        None => return Err("-I needs a directory".into()),
      },
      a if a.starts_with("-I") => include_paths.push(a[2..].into()),
      a if a.starts_with("--") => return Err(format!("unknown option {a}")),
      a => match script_argument(a) {
        // In any order: `check a:b file.shs` and `check file.shs a:b`.
        Some((k, v)) => {
          defines.insert(k.to_string(), v.to_string());
        }
        None if file.is_none() => file = Some(a.to_string()),
        None => return Err(format!("script arguments are key:value, got {a}")),
      },
    }
  }
  Ok(Options {
    json,
    file: file.ok_or("missing file")?,
    defines,
    include_paths,
  })
}

/// `key:value` with a name-like key. A Windows drive path (`C:\\x`,
/// `C:/x`) is a file, not an argument.
fn script_argument(arg: &str) -> Option<(&str, &str)> {
  let (k, v) = arg.split_once(':')?;
  let name_like = k.starts_with(|c: char| c.is_ascii_alphabetic())
    && k
      .chars()
      .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
  let drive = k.len() == 1 && (v.starts_with('\\') || v.starts_with('/'));
  (name_like && !drive).then_some((k, v))
}

fn read(file: &str) -> Result<Source, String> {
  std::fs::read_to_string(file)
    .map(|text| Source::new(file, text))
    .map_err(|e| format!("{file}: {e}"))
}

fn print_report(report: &CheckReport, source: &Source, json: bool) {
  if json {
    println!("{}", report.to_json());
  } else {
    for d in &report.diagnostics {
      eprint!("{}", render(d, source));
    }
  }
}

fn check(o: &Options) -> Result<ExitCode, String> {
  let source = read(&o.file)?;
  // The program's source holds the included files, which diagnostics
  // point into.
  let (report, source) = match Program::load_with(source, &catalog(), &o.defines, &o.files()) {
    Ok(program) => (program.analyze(), program.source),
    Err((source, diagnostics)) => (
      CheckReport {
        file: o.file.clone(),
        diagnostics,
        wires: Vec::new(),
        functions: Vec::new(),
      },
      source,
    ),
  };
  print_report(&report, &source, o.json);
  if !o.json && report.ok() {
    eprintln!("{}: ok", o.file);
  }
  Ok(if report.ok() {
    ExitCode::SUCCESS
  } else {
    ExitCode::FAILURE
  })
}

fn run(o: &Options) -> Result<ExitCode, String> {
  let source = read(&o.file)?;
  let program = match Program::load_with(source, &catalog(), &o.defines, &o.files()) {
    Ok(p) => p,
    Err((source, diagnostics)) => {
      if o.json {
        println!("{}", run_json(&o.file, &diagnostics, &[], None));
      } else {
        for d in &diagnostics {
          eprint!("{}", render(d, &source));
        }
      }
      return Ok(ExitCode::FAILURE);
    }
  };
  run_on(&program, o)
}

fn run_on(program: &Program, o: &Options) -> Result<ExitCode, String> {
  let (result, log) = if o.json {
    otter_core::log::capture(|| program.run())
  } else {
    (program.run(), Vec::new())
  };
  let report = match result {
    Ok(r) => r,
    Err(diagnostics) => {
      if o.json {
        println!("{}", run_json(&o.file, &diagnostics, &log, None));
      } else {
        for d in &diagnostics {
          eprint!("{}", render(d, &program.source));
        }
      }
      return Ok(ExitCode::FAILURE);
    }
  };
  if o.json {
    println!("{}", run_json(&o.file, &[], &log, Some(&report)));
  } else {
    print_outcomes(&report);
  }
  Ok(if report.succeeded() {
    ExitCode::SUCCESS
  } else {
    ExitCode::FAILURE
  })
}

fn print_outcomes(report: &RunReport) {
  for (wire, outcome) in &report.outcomes {
    match outcome {
      Some(Outcome::Completed(v)) => println!("{wire}: {v}"),
      Some(Outcome::Stopped) => println!("{wire}: stopped"),
      Some(Outcome::Cancelled) => println!("{wire}: cancelled"),
      Some(Outcome::Failed(e)) => eprintln!("{wire}: failed: {e}"),
      None => println!("{wire}: still running after {} ticks", report.ticks),
    }
  }
  for (wire, outcome) in &report.spawned_failures {
    if let Outcome::Failed(e) = outcome {
      eprintln!("{wire} (spawned): failed: {e}");
    }
  }
}

/// `{wire, outcome, value?, error?}`; a wire still running is `running`.
fn outcome_json(wire: &str, outcome: Option<&Outcome>) -> String {
  let (kind, extra) = match outcome {
    Some(Outcome::Completed(v)) => (
      "completed",
      format!(",\"value\":{}", json_str(&v.to_string())),
    ),
    Some(Outcome::Stopped) => ("stopped", String::new()),
    Some(Outcome::Cancelled) => ("cancelled", String::new()),
    Some(Outcome::Failed(e)) => ("failed", format!(",\"error\":{}", json_str(&e.to_string()))),
    None => ("running", String::new()),
  };
  format!(
    "{{\"wire\":{},\"outcome\":\"{kind}\"{extra}}}",
    json_str(wire)
  )
}

fn run_json(
  file: &str,
  diagnostics: &[Diagnostic],
  log: &[String],
  report: Option<&RunReport>,
) -> String {
  let join = |items: Vec<String>| items.join(",");
  let ok = diagnostics.is_empty() && report.is_some_and(RunReport::succeeded);
  let outcomes = report.map_or(Vec::new(), |r| {
    r.outcomes
      .iter()
      .map(|(w, o)| outcome_json(w, o.as_ref()))
      .collect()
  });
  let spawned = report.map_or(Vec::new(), |r| {
    r.spawned_failures
      .iter()
      .map(|(w, o)| outcome_json(w, Some(o)))
      .collect()
  });
  format!(
    "{{\"ok\":{ok},\"file\":{},\"diagnostics\":[{}],\"log\":[{}],\"outcomes\":[{}],\"spawned_failures\":[{}]}}",
    json_str(file),
    join(diagnostics.iter().map(Diagnostic::to_json).collect()),
    join(log.iter().map(|l| json_str(l)).collect()),
    join(outcomes),
    join(spawned),
  )
}

/// `otter tool <file> [arguments]`: loads a tool (otter_lang::Tool); with
/// no arguments prints its definition as JSON, else calls it with the
/// arguments (a JSON object) and prints its output, the log on stderr.
fn tool(args: &[String]) -> Result<ExitCode, String> {
  let (file, arguments) = match args {
    [file] => (file, None),
    [file, arguments] => (file, Some(arguments)),
    _ => return Err("tool needs a file and, to call it, a JSON object of arguments".into()),
  };
  let source = read(file)?;
  let tool = match otter_lang::Tool::load(source, &catalog()) {
    Ok(tool) => tool,
    Err(e) => {
      eprintln!("{e}");
      return Ok(ExitCode::FAILURE);
    }
  };
  let Some(arguments) = arguments else {
    let d = tool.definition();
    println!(
      "{{\"name\":{},\"description\":{},\"parameters\":{}}}",
      json_str(&d.name),
      json_str(&d.description),
      d.parameters
    );
    return Ok(ExitCode::SUCCESS);
  };
  let input = otter_lang::ToolInput::from_json(arguments)
    .map_err(|e| e.to_string())?
    .with_working_dir(
      std::env::current_dir()
        .map(|d| d.display().to_string())
        .unwrap_or_default(),
    );
  match tool.call(&input) {
    Ok(out) => {
      for line in &out.log {
        eprintln!("{line}");
      }
      print!("{}", out.text);
      if !out.text.ends_with('\n') {
        println!();
      }
      Ok(ExitCode::SUCCESS)
    }
    Err(e) => {
      if let otter_lang::ToolError::Failed { log, .. } = &e {
        for line in log {
          eprintln!("{line}");
        }
      }
      eprintln!("{}: failed: {e}", tool.definition().name);
      Ok(ExitCode::FAILURE)
    }
  }
}

fn main() -> ExitCode {
  let args: Vec<String> = std::env::args().skip(1).collect();
  let result = match args.first().map(String::as_str) {
    Some("check") => options(&args[1..]).and_then(|o| check(&o)),
    Some("run") => options(&args[1..]).and_then(|o| run(&o)),
    Some("watch") => options(&args[1..]).and_then(|o| {
      if o.json {
        return Err("watch does not support --json".into());
      }
      watch::watch(&o)
    }),
    Some("describe") if args.len() == 2 => match catalog().describe_json(&args[1]) {
      Some(json) => {
        println!("{json}");
        Ok(ExitCode::SUCCESS)
      }
      None => {
        let c = catalog();
        let names: Vec<String> = c.names().iter().map(|n| n.to_string()).collect();
        let near = otter_lang::problem::closest(&args[1], names, 3);
        let hint = if near.is_empty() {
          String::new()
        } else {
          format!(" (did you mean: {})", near.join(", "))
        };
        Err(format!("no shard named {}{hint}", args[1]))
      }
    },
    Some("tool") => tool(&args[1..]),
    Some("search") if args.len() == 2 => {
      for s in catalog().search(&args[1]) {
        println!("{}: {}", s.name(), s.desc.summary);
      }
      Ok(ExitCode::SUCCESS)
    }
    Some("catalog") if args.len() == 1 => {
      println!("{}", catalog().index_json());
      Ok(ExitCode::SUCCESS)
    }
    _ => {
      eprintln!("{USAGE}");
      return ExitCode::from(2);
    }
  };
  result.unwrap_or_else(|e| {
    eprintln!("otter: {e}");
    ExitCode::from(2)
  })
}
