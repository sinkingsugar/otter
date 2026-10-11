//! Loading, checking and running a source program on a mesh.
//!
//! `check` reports every problem it can, located in the source: syntax
//! problems first (lowering a broken tree only cascades), then lowering and
//! compose problems. Compose diagnostics carry occurrence paths; the source
//! map turns them into file, line and column.

use otter_core::hash::{HashMap, HashSet};
use std::time::{Duration, Instant};

use otter_core::diagnostic::{Diagnostic, Phase, json_str};
use otter_core::diagnostic::{PathStep, path_json};
use otter_core::signature::{Analysis, Occurrence};
use otter_core::{Catalog, Error, InstanceId, Mesh, Outcome, Type, Var};

use crate::ast::{BlockKind, Literal, Statement};
use crate::files::{Files, FsFiles};
use crate::lower::{Lowered, ROOT_WIRE, lower_with};
use crate::parser::parse;
use crate::problem::{Problem, locate};
use crate::source::Source;

/// A source program, parsed and lowered.
pub struct Program {
  pub source: Source,
  pub lowered: Lowered,
  /// The files the program included or read, by name, in the order it
  /// read them: what a reload depends on besides the source.
  pub files: Vec<String>,
}

/// The result of `check`, in the 1.x `shards check --json` envelope.
pub struct CheckReport {
  pub file: String,
  pub diagnostics: Vec<Diagnostic>,
  /// Successful root composes, including nested occurrences. Failed roots
  /// report diagnostics rather than presenting a partial analysis as complete.
  pub wires: Vec<WireAnalysis>,
  /// Every declared function that composed, with its signature (inferred
  /// effects and mesh access included) and its declaration's location.
  pub functions: Vec<FunctionReport>,
}

pub struct FunctionReport {
  pub signature: otter_core::signature::Signature<'static>,
  pub line: Option<u32>,
  pub column: Option<u32>,
}

impl FunctionReport {
  fn to_json(&self) -> String {
    let mut fields = vec![format!("\"signature\":{}", self.signature.to_json())];
    if let (Some(line), Some(column)) = (self.line, self.column) {
      fields.push(format!("\"line\":{line},\"column\":{column}"));
    }
    format!("{{{}}}", fields.join(","))
  }
}

pub struct WireAnalysis {
  pub name: String,
  pub analysis: Analysis,
  pub occurrences: Vec<LocatedOccurrence>,
}

pub struct LocatedOccurrence {
  pub occurrence: Occurrence,
  pub span: Option<crate::Span>,
  pub line: Option<u32>,
  pub column: Option<u32>,
}

impl WireAnalysis {
  fn to_json(&self) -> String {
    let occurrences = self
      .occurrences
      .iter()
      .map(|o| {
        let mut fields = vec![
          format!("\"path\":{}", path_json(&o.occurrence.path)),
          format!("\"input\":{}", json_str(&o.occurrence.input.to_string())),
          format!("\"output\":{}", json_str(&o.occurrence.output.to_string())),
          format!("\"effects\":{}", o.occurrence.effects.to_json()),
          format!("\"lifetime\":{}", json_str(o.occurrence.lifetime.name())),
        ];
        if let Some(call) = &o.occurrence.call {
          let reason = call
            .reason
            .as_ref()
            .map(|r| format!(",\"reason\":{}", json_str(&r.to_string())))
            .unwrap_or_default();
          fields.push(format!(
            "\"call\":{{\"path\":{}{reason}}}",
            json_str(call.path)
          ));
        }
        if let Some(span) = o.span {
          fields.push(format!(
            "\"span\":{{\"start\":{},\"end\":{}}}",
            span.start, span.end
          ));
        }
        if let (Some(line), Some(column)) = (o.line, o.column) {
          fields.push(format!("\"line\":{line},\"column\":{column}"));
        }
        format!("{{{}}}", fields.join(","))
      })
      .collect::<Vec<_>>()
      .join(",");
    format!(
      "{{\"wire\":{},\"effects\":{},\"lifetime\":{},\"uses\":{},\"mutates\":{},\"occurrences\":[{occurrences}]}}",
      json_str(&self.name),
      self.analysis.effects.to_json(),
      json_str(self.analysis.lifetime.name()),
      Analysis::mesh_json(&self.analysis.uses),
      Analysis::mesh_json(&self.analysis.mutates)
    )
  }
}

impl CheckReport {
  pub fn ok(&self) -> bool {
    self.diagnostics.is_empty()
  }

  pub fn to_json(&self) -> String {
    let diagnostics: Vec<String> = self.diagnostics.iter().map(Diagnostic::to_json).collect();
    format!(
      "{{\"ok\":{},\"file\":{},\"diagnostics\":[{}],\"wires\":[{}],\"functions\":[{}]}}",
      self.ok(),
      json_str(&self.file),
      diagnostics.join(","),
      self
        .wires
        .iter()
        .map(WireAnalysis::to_json)
        .collect::<Vec<_>>()
        .join(","),
      self
        .functions
        .iter()
        .map(FunctionReport::to_json)
        .collect::<Vec<_>>()
        .join(",")
    )
  }
}

impl Program {
  /// Parses and lowers, reading included files from the filesystem
  /// ([`FsFiles`], relative to the source's name). Syntax problems stop
  /// before lowering.
  pub fn load(
    source: Source,
    catalog: &Catalog,
    defines: &std::collections::HashMap<String, String>,
  ) -> Result<Program, (Source, Vec<Diagnostic>)> {
    Program::load_with(source, catalog, defines, &FsFiles::default())
  }

  /// [`Program::load`], with `@include` and `@read` reading from `files`.
  pub fn load_with(
    mut source: Source,
    catalog: &Catalog,
    defines: &std::collections::HashMap<String, String>,
    files: &dyn Files,
  ) -> Result<Program, (Source, Vec<Diagnostic>)> {
    let (mut tree, mut problems) = parse(&source);
    let mut read = Vec::new();
    if problems.is_empty() {
      let mut seen = vec![files.key(&source.name)];
      let statements = std::mem::take(&mut tree.statements);
      tree.statements = include(
        &mut source,
        statements,
        files,
        &mut seen,
        &mut read,
        &mut problems,
        0,
      );
    }
    if !problems.is_empty() {
      let diagnostics = problems.iter().map(|p| p.to_diagnostic(&source)).collect();
      return Err((source, diagnostics));
    }
    let (lowered, problems, reads) = lower_with(&tree, catalog, defines, &source, files);
    if !problems.is_empty() {
      let diagnostics = problems.iter().map(|p| p.to_diagnostic(&source)).collect();
      return Err((source, diagnostics));
    }
    read.extend(reads);
    Ok(Program {
      source,
      lowered,
      files: read,
    })
  }

  /// The wires that start when the program runs: those scheduled on the
  /// `@run` mesh, or the root wire.
  pub fn entries(&self) -> Vec<String> {
    match &self.lowered.run {
      Some(run) => self
        .lowered
        .meshes
        .iter()
        .filter(|m| m.name == run.mesh)
        .flat_map(|m| m.scheduled.iter().map(|(w, _)| w.clone()))
        .collect(),
      None if self.lowered.root => vec![ROOT_WIRE.to_string()],
      None => self
        .lowered
        .meshes
        .iter()
        .flat_map(|m| m.scheduled.iter().map(|(w, _)| w.clone()))
        .collect(),
    }
  }

  /// A compose error as a located diagnostic; `root` is the wire or
  /// function it was composed for, used when the error names no position.
  pub(crate) fn diagnostic(&self, root: PathStep, err: Error) -> Diagnostic {
    let mut d = match err {
      Error::Diagnostic(d) => *d,
      other => Diagnostic::new(
        Phase::Compose,
        "compose-error",
        "compose-error",
        other.to_string(),
      ),
    };
    let span = self.lowered.map.locate(&d).or_else(|| {
      let mut probe = Diagnostic::new(Phase::Compose, "", "", "");
      probe.path = vec![root];
      self.lowered.map.locate(&probe)
    });
    if let Some(span) = span {
      locate(&mut d, &self.source, span);
    } else {
      d.file = Some(self.source.name.clone());
    }
    if let Some(related) = &mut d.related {
      let mut probe = Diagnostic::new(Phase::Compose, "", "", "");
      probe.path = related.path.clone();
      if let Some(span) = self.lowered.map.locate(&probe) {
        let (line, column) = self.source.line_col(span.start);
        related.line = Some(line);
        related.column = Some(column);
      }
    }
    // `x-1` is one name in Shards; say so when it is the unknown variable.
    if d.code == "unknown-variable"
      && let Some(name) = d
        .message
        .strip_prefix("unknown variable ")
        .map(str::to_string)
      && let Some((_, tail)) = name.rsplit_once('-')
      && !tail.is_empty()
      && tail.chars().all(|c| c.is_ascii_digit())
    {
      d.message.push_str(&format!(
        "; `-` is part of names, so `{name}` is one name: subtract with `Math.Subtract` (`Sub`)"
      ));
    }
    d
  }

  /// The wires each wire references (`Do`, `Spawn`), by name.
  fn references(&self) -> HashMap<&str, Vec<String>> {
    fn refs(flow: &[otter_core::ShardDef], out: &mut Vec<String>) {
      for def in flow {
        for arg in &def.args {
          match &arg.value {
            otter_core::ParamValue::Wire(name) => out.push(name.clone()),
            otter_core::ParamValue::Flow(f) | otter_core::ParamValue::Eval(f) => refs(f, out),
            otter_core::ParamValue::Cases(cases) => cases.iter().for_each(|(_, f)| refs(f, out)),
            _ => {}
          }
        }
      }
    }
    self
      .lowered
      .wires
      .iter()
      .map(|w| {
        let mut out = Vec::new();
        refs(&w.flow, &mut out);
        (w.name.as_str(), out)
      })
      .collect()
  }

  /// Wires not reachable from the entries: they would only run if
  /// scheduled, with no input, so `check` composes them that way. One root
  /// per group: a wire reachable from an earlier unreachable root is
  /// composed through it (an unreachable cycle reports its recursion once).
  pub(crate) fn unreachable_roots(&self) -> Vec<String> {
    let refs = self.references();
    let mut seen: Vec<String> = Vec::new();
    let visit = |start: &str, seen: &mut Vec<String>| {
      let mut stack = vec![start.to_string()];
      while let Some(w) = stack.pop() {
        if seen.contains(&w) {
          continue;
        }
        if let Some(next) = refs.get(w.as_str()) {
          stack.extend(next.iter().cloned());
        }
        seen.push(w);
      }
    };
    for entry in self.entries() {
      visit(&entry, &mut seen);
    }
    let mut roots = Vec::new();
    for w in &self.lowered.wires {
      if !seen.contains(&w.name) {
        roots.push(w.name.clone());
        visit(&w.name, &mut seen);
      }
    }
    roots
  }

  /// Composes every wire: the entries (wires they reach through `Do` and
  /// `Spawn` compose with them), then one root per unreachable group.
  /// Returns diagnostics without materializing tooling occurrence reports;
  /// use `analyze` when those source-located reports are needed.
  pub fn compose(&self) -> Vec<Diagnostic> {
    self.compose_report(false).diagnostics
  }

  pub fn analyze(&self) -> CheckReport {
    self.compose_report(true)
  }

  fn compose_report(&self, include_analysis: bool) -> CheckReport {
    let mut mesh = self.mesh();
    if include_analysis {
      mesh.record_call_sites();
    }
    let mut out: Vec<Diagnostic> = Vec::new();
    let mut wires = Vec::new();
    for wire in self.entries().into_iter().chain(self.unreachable_roots()) {
      match mesh.compile(&wire, Type::none()) {
        Err(err) => {
          let d = self.diagnostic(PathStep::Wire(wire.clone()), err);
          if !out.contains(&d) {
            out.push(d);
          }
        }
        Ok(compiled) if include_analysis => {
          let analysis = compiled.flow.analysis.clone();
          let occurrences = analysis
            .occurrences
            .iter()
            .map(|mut occurrence| {
              occurrence.path.insert(0, PathStep::Wire(wire.clone()));
              let mut d = Diagnostic::new(Phase::Compose, "", "", "");
              d.path = occurrence.path.clone();
              let span = self.lowered.map.locate(&d);
              let position = span.map(|s| self.source.line_col(s.start));
              LocatedOccurrence {
                occurrence,
                span,
                line: position.map(|p| p.0),
                column: position.map(|p| p.1),
              }
            })
            .collect();
          wires.push(WireAnalysis {
            name: wire,
            analysis,
            occurrences,
          });
        }
        Ok(_) => {}
      }
    }
    // Every function is checked, called or not, against its declared input.
    let mut functions = Vec::new();
    for def in &self.lowered.functions {
      match mesh.compile_function(&def.name) {
        Err(err) => {
          let d = self.diagnostic(PathStep::Function(def.name.clone()), err);
          if !out.contains(&d) {
            out.push(d);
          }
        }
        Ok(compiled) if include_analysis => {
          let mut probe = Diagnostic::new(Phase::Compose, "", "", "");
          probe.path = vec![PathStep::Function(def.name.clone())];
          let position = self
            .lowered
            .map
            .locate(&probe)
            .map(|s| self.source.line_col(s.start));
          functions.push(FunctionReport {
            signature: compiled.signature(),
            line: position.map(|p| p.0),
            column: position.map(|p| p.1),
          });
        }
        Ok(_) => {}
      }
    }
    CheckReport {
      file: self.source.name.clone(),
      diagnostics: out,
      wires,
      functions,
    }
  }

  /// A fresh mesh with the program's wires and functions declared.
  pub(crate) fn mesh(&self) -> Mesh {
    let mut mesh = Mesh::new();
    self.declare_on(&mut mesh);
    mesh
  }

  /// Declares the program's wires and functions on `mesh`.
  pub(crate) fn declare_on(&self, mesh: &mut Mesh) {
    for def in &self.lowered.wires {
      mesh.add_wire(def.clone());
    }
    for def in &self.lowered.functions {
      mesh.add_function(def.clone());
    }
  }

  /// Runs the program on a fresh mesh: the entry wires, ticked at the
  /// `@run` rate (as fast as possible without one) until every instance
  /// finishes or the `iterations` limit is reached.
  pub fn run(&self) -> Result<RunReport, Vec<Diagnostic>> {
    let mut mesh = self.mesh();
    let entries = self.entries();
    mesh.reserve_instances(entries.len());
    let mut instances = Vec::with_capacity(entries.len());
    let mut errors = Vec::new();
    for wire in entries {
      match mesh.compile(&wire, Type::none()) {
        Ok(compiled) => match mesh.spawn(&compiled, Var::None) {
          Ok(id) => instances.push((wire, id)),
          Err(err) => errors.push(self.diagnostic(PathStep::Wire(wire.clone()), err)),
        },
        Err(err) => errors.push(self.diagnostic(PathStep::Wire(wire.clone()), err)),
      }
    }
    if !errors.is_empty() {
      return Err(errors);
    }
    let run = self.lowered.run.as_ref();
    // Lowering only accepts FPS values with a representable frame interval.
    let frame = run
      .and_then(|r| r.fps)
      .and_then(|fps| Duration::try_from_secs_f64(1.0 / fps).ok());
    let iterations = run.and_then(|r| r.iterations);
    let entry_ids: HashSet<InstanceId> = instances.iter().map(|(_, id)| *id).collect();
    // Outcomes of finished entry instances, kept here so the mesh can drop
    // every finished record each tick.
    let mut entry_outcomes: HashMap<InstanceId, Outcome> = HashMap::default();
    let mut spawned = Vec::new();
    let mut ticks = 0i64;
    let mut next = Instant::now();
    loop {
      mesh.tick();
      ticks += 1;
      // Retire every finished instance each tick (long-running programs
      // spawn many): one pass over the records, hashed lookups. Entries'
      // outcomes are kept for the report, and spawned failures.
      for (id, wire, outcome) in mesh.take_finished() {
        if entry_ids.contains(&id) {
          entry_outcomes.insert(id, outcome);
        } else if matches!(outcome, Outcome::Failed(_)) {
          spawned.push((wire, outcome));
        }
      }
      if mesh.running() == 0 || iterations.is_some_and(|n| ticks >= n) {
        break;
      }
      if let Some(frame) = frame {
        next = next.checked_add(frame).unwrap_or_else(Instant::now);
        let now = Instant::now();
        if next > now {
          std::thread::sleep(next - now);
        } else {
          next = now;
        }
      }
    }
    let outcomes = instances
      .into_iter()
      .map(|(wire, id)| (wire, entry_outcomes.remove(&id)))
      .collect();
    Ok(RunReport {
      ticks,
      outcomes,
      spawned_failures: spawned,
    })
  }
}

/// What a run did: ticks, and each entry wire's outcome (`None`: still
/// running when the `iterations` limit stopped the mesh).
#[derive(Debug)]
pub struct RunReport {
  pub ticks: i64,
  pub outcomes: Vec<(String, Option<Outcome>)>,
  /// Spawned instances that failed: their wire and outcome.
  pub spawned_failures: Vec<(String, Outcome)>,
}

impl RunReport {
  /// Whether no entry wire and no spawned instance failed.
  pub fn succeeded(&self) -> bool {
    self.spawned_failures.is_empty()
      && !self
        .outcomes
        .iter()
        .any(|(_, o)| matches!(o, Some(Outcome::Failed(_))))
  }
}

/// Checks a source: syntax, lowering and compose.
pub fn check(
  source: Source,
  catalog: &Catalog,
  defines: &std::collections::HashMap<String, String>,
) -> CheckReport {
  check_with(source, catalog, defines, &FsFiles::default())
}

/// [`check`], with `@include` and `@read` reading from `files`.
pub fn check_with(
  source: Source,
  catalog: &Catalog,
  defines: &std::collections::HashMap<String, String>,
  files: &dyn Files,
) -> CheckReport {
  let file = source.name.clone();
  match Program::load_with(source, catalog, defines, files) {
    Err((_, diagnostics)) => CheckReport {
      file,
      diagnostics,
      wires: Vec::new(),
      functions: Vec::new(),
    },
    Ok(program) => program.analyze(),
  }
}

/// How many files may include each other in a chain, so a malformed reader
/// cannot recurse without end (a file is included once, so real chains are
/// as long as there are files).
const MAX_INCLUDE_DEPTH: usize = 64;

/// The statements of a file with every top-level `@include("path")`
/// replaced by the included file's statements (expanded the same way). A
/// file is included once per program, wherever it is named again: its
/// declarations are visible from then on. The included files are added to
/// `source` (so their spans locate), and their names to `read`.
fn include(
  source: &mut Source,
  statements: Vec<Statement>,
  files: &dyn Files,
  seen: &mut Vec<String>,
  read: &mut Vec<String>,
  problems: &mut Vec<Problem>,
  depth: usize,
) -> Vec<Statement> {
  let mut out = Vec::with_capacity(statements.len());
  for statement in statements {
    let Some((path, span)) = include_target(&statement, problems) else {
      out.push(statement);
      continue;
    };
    let Some(path) = path else {
      continue;
    };
    if depth >= MAX_INCLUDE_DEPTH {
      problems.push(Problem::construct(
        span,
        "generic",
        "include-depth",
        format!("files include each other more than {MAX_INCLUDE_DEPTH} levels deep"),
      ));
      continue;
    }
    let from = source.file_name(span.start).to_string();
    let file = match files.read(&from, &path) {
      Ok(file) => file,
      Err(message) => {
        problems.push(Problem::construct(
          span,
          "generic",
          "file-not-found",
          format!("cannot include {path}: {message}"),
        ));
        continue;
      }
    };
    if seen.contains(&file.key) {
      continue;
    }
    seen.push(file.key.clone());
    let text = match String::from_utf8(file.bytes) {
      Ok(text) => text,
      Err(_) => {
        problems.push(Problem::construct(
          span,
          "generic",
          "not-utf8",
          format!("cannot include {}: it is not UTF-8 text", file.name),
        ));
        continue;
      }
    };
    read.push(file.name.clone());
    let range = source.add(file.name, &text);
    let (tree, mut parsed) = crate::parser::parse_range(source, range);
    let failed = !parsed.is_empty();
    problems.append(&mut parsed);
    if !failed {
      out.extend(include(
        source,
        tree.statements,
        files,
        seen,
        read,
        problems,
        depth + 1,
      ));
    }
  }
  out
}

/// For a top-level `@include(...)` statement, its path (`None`, with a
/// problem reported, when the statement is malformed) and span.
fn include_target(
  statement: &Statement,
  problems: &mut Vec<Problem>,
) -> Option<(Option<String>, crate::Span)> {
  let Statement::Pipeline(pipe) = statement else {
    return None;
  };
  let [block] = &pipe.blocks[..] else {
    return None;
  };
  let BlockKind::Func { name, params } = &block.kind else {
    return None;
  };
  if name.node != "include" {
    return None;
  }
  let path = match params.as_ref().map(|p| &p.items[..]) {
    Some([param]) if param.name.is_none() => match &param.value.blocks[..] {
      [b] => match &b.kind {
        BlockKind::Literal(Literal::String(path)) => Some(path.clone()),
        _ => None,
      },
      _ => None,
    },
    _ => None,
  };
  if path.is_none() {
    problems.push(Problem::construct(
      block.span,
      "generic",
      "declaration",
      "`@include` takes a file name as a string: `@include(\"lib.shs\")`".into(),
    ));
  }
  Some((path, block.span))
}
