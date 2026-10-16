//! Tools: scripts a host loads while it runs and calls with a table of
//! arguments, getting text back (docs/host-requirements.md H8, host side,
//! the smallest version: compile at run time, describe, call).
//!
//! A tool script declares functions and ends with its definition, a table
//! its top-level code outputs:
//!
//! ```shards
//! @fn(Greet input: {input: {} env: {} working-dir: String} output: String params: {} {
//!   input.input | Take("name") | ExpectString = name
//!   f"Hello, {name}!"
//! })
//!
//! {
//!   definition: {
//!     name: "greet"
//!     description: "Greets someone."
//!     parameters: {type: "object" properties: {name: {type: "string"}} required: ["name"]}
//!   }
//!   use: "Greet"
//! }
//! ```
//!
//! - `definition.name` (a String) is the tool's name, `definition.description`
//!   (a String, optional) what it does, `definition.parameters` (a table,
//!   optional) its arguments as JSON Schema, given to a model as written.
//! - `use` names the function a call runs. Its declared input must accept
//!   the call table `{input: {} env: {} working-dir: String}`: `input` the
//!   arguments (a table, usually parsed from the model's JSON), `env` the
//!   host's settings for the tool (a table of whatever the host passes),
//!   `working-dir` the directory the tool works in. Its output is the
//!   result: a String as it is, any other value as JSON.
//!
//! [`Tool::load`] parses, lowers and composes everything once, so every
//! problem is a located diagnostic before the tool is offered; then it runs
//! the top-level code to read the definition, which must have no effects
//! (no I/O, no waiting, no clock). [`Tool::call`] runs the function on a
//! fresh mesh on the calling thread and blocks until it finishes: from
//! async code, run it on a blocking thread (`tokio::task::spawn_blocking`).
//! Calls are independent and may run concurrently on several threads; a
//! [`Tool`] is cheap to clone and `Send + Sync`.
//!
//! What the script may use is the catalog the host loads it with: leave
//! out what a tool must not reach (a host's forbidden list is a smaller
//! catalog).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use otter_core::diagnostic::{Diagnostic, PathStep};
use otter_core::fs_access::FsAccess;
use otter_core::service::Services;
use otter_core::shard::ShardDef;
use otter_core::signature::Effects;
use otter_core::{CancelToken, Catalog, CompiledWire, Mesh, Outcome, Type, Var, WireDef};

use crate::files::{Files, FsFiles};
use crate::problem::render;
use crate::program::Program;
use crate::source::Source;

/// The internal wire a call runs: it calls the tool's function.
const CALL_WIRE: &str = "%tool-call";

/// A loaded tool: its definition and its compiled program.
#[derive(Clone)]
pub struct Tool {
  inner: Arc<Inner>,
}

struct Inner {
  program: Program,
  definition: ToolDefinition,
  /// The function a call runs.
  entry: String,
  /// The wire a call runs, composed once at load: every call spawns it on
  /// a fresh mesh declaring the same program.
  call: Arc<CompiledWire>,
}

/// What a model is told about a tool.
#[derive(Clone, Debug)]
pub struct ToolDefinition {
  pub name: String,
  pub description: String,
  /// The JSON Schema of the arguments as JSON text, written from the
  /// script's table when the tool loaded.
  pub parameters: String,
}

/// What a call passes to the tool's function.
#[derive(Clone, Debug)]
pub struct ToolInput {
  /// The arguments: a table (see [`ToolInput::from_json`]).
  pub input: Var,
  /// The host's settings for the tool: a table.
  pub env: Var,
  pub working_dir: String,
}

impl ToolInput {
  /// The arguments as a table, with an empty `env` and `working-dir`.
  pub fn new(input: Var) -> ToolInput {
    ToolInput {
      input,
      env: Var::table(Vec::<(&str, Var)>::new()),
      working_dir: String::new(),
    }
  }

  /// Arguments from a model's JSON (an object; `null` or empty text is no
  /// arguments).
  pub fn from_json(arguments: &str) -> Result<ToolInput, ToolError> {
    let input = if arguments.trim().is_empty() {
      Var::None
    } else {
      otter_core::json::parse(arguments).map_err(ToolError::Input)?
    };
    let input = match input {
      Var::None => Var::table(Vec::<(&str, Var)>::new()),
      Var::Table(_) => input,
      other => {
        return Err(ToolError::Input(format!(
          "the arguments must be a JSON object, got {}",
          other.type_of()
        )));
      }
    };
    Ok(ToolInput::new(input))
  }

  pub fn with_env(mut self, env: Var) -> ToolInput {
    self.env = env;
    self
  }

  pub fn with_working_dir(mut self, dir: impl Into<String>) -> ToolInput {
    self.working_dir = dir.into();
    self
  }

  fn to_var(&self) -> Result<Var, ToolError> {
    if self.input.as_table().is_none() {
      return Err(ToolError::Input(format!(
        "input must be a table, got {}",
        self.input.type_of()
      )));
    }
    if self.env.as_table().is_none() {
      return Err(ToolError::Input(format!(
        "env must be a table, got {}",
        self.env.type_of()
      )));
    }
    Ok(Var::table([
      ("input", self.input.clone()),
      ("env", self.env.clone()),
      ("working-dir", Var::string(&self.working_dir)),
    ]))
  }
}

/// The type of the call table, which the tool's function must accept.
pub fn call_type() -> Type {
  Type::fixed_table([
    ("input", Type::any_table()),
    ("env", Type::any_table()),
    ("working-dir", Type::string()),
  ])
}

/// How a call runs: services for the script's shards, a cancellation token,
/// a time limit and a fuel budget. With any of the last three the call runs
/// metered (`otter_core::Budget`): they are checked every few thousand
/// instructions, inside loops that never suspend too, and the call runs the
/// engine's metered form, slower on compute-heavy code. Without them it
/// runs unmetered.
#[derive(Clone, Default)]
pub struct CallOptions {
  /// Inserted into the call's mesh (docs/embedding.md, services).
  pub services: Services,
  /// Cancel it (from any thread) to cancel the call: it ends with
  /// [`ToolError::Cancelled`], at once even while the call waits.
  pub cancel: Option<CancelToken>,
  /// The longest the call may run; then it ends with
  /// [`ToolError::TimedOut`].
  pub timeout: Option<Duration>,
  /// The most work the call may do, in engine dispatches and VM
  /// instructions; spent, the call fails ([`ToolError::Failed`]).
  pub fuel: Option<u64>,
}

/// What a call returned.
#[derive(Clone, Debug)]
pub struct ToolOutput {
  /// The function's output: a String as it is, any other value as JSON.
  pub text: String,
  /// The lines the call logged (`Log`), in order; nothing is printed.
  pub log: Vec<String>,
}

/// Why loading or calling a tool failed.
#[derive(Clone, Debug)]
pub enum ToolError {
  /// The script does not compile: located diagnostics, and the same
  /// rendered for people.
  Compile {
    diagnostics: Vec<Diagnostic>,
    rendered: String,
  },
  /// The script compiles but does not define a tool as this module says.
  Definition(String),
  /// The call's input is not what a tool takes.
  Input(String),
  /// The script failed while running (`Fail`, an activation error): the
  /// message, and the lines it logged.
  Failed {
    message: String,
    log: Vec<String>,
  },
  /// The script ended with `Stop` before producing an output. `log`, here
  /// and below: the lines the call logged before it ended.
  Stopped {
    log: Vec<String>,
  },
  Cancelled {
    log: Vec<String>,
  },
  TimedOut {
    limit: Duration,
    log: Vec<String>,
  },
}

impl std::fmt::Display for ToolError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      ToolError::Compile { rendered, .. } => write!(f, "{}", rendered.trim_end()),
      ToolError::Definition(m) => write!(f, "not a tool: {m}"),
      ToolError::Input(m) => write!(f, "invalid tool input: {m}"),
      ToolError::Failed { message, .. } => write!(f, "{message}"),
      ToolError::Stopped { .. } => write!(f, "the tool stopped without a result"),
      ToolError::Cancelled { .. } => write!(f, "the tool call was cancelled"),
      ToolError::TimedOut { limit, .. } => write!(f, "the tool call timed out after {limit:?}"),
    }
  }
}

impl std::error::Error for ToolError {}

/// How many ticks the definition may take: it has no effects, so it
/// finishes in its first.
const DEFINITION_TICKS: usize = 4;

impl Tool {
  /// Loads a tool from source, reading `@include` and `@read` files from
  /// the filesystem relative to the source's name.
  pub fn load(source: Source, catalog: &Catalog) -> Result<Tool, ToolError> {
    Tool::load_with(source, catalog, &HashMap::new(), &FsFiles::default())
  }

  /// [`Tool::load`] with defines (read as `@name`) and a file reader.
  pub fn load_with(
    source: Source,
    catalog: &Catalog,
    defines: &HashMap<String, String>,
    files: &dyn Files,
  ) -> Result<Tool, ToolError> {
    let compile_error = |source: &Source, diagnostics: Vec<Diagnostic>| {
      let rendered = diagnostics.iter().map(|d| render(d, source)).collect();
      ToolError::Compile {
        diagnostics,
        rendered,
      }
    };
    let program = match Program::load_with(source, catalog, defines, files) {
      Ok(program) => program,
      Err((source, diagnostics)) => return Err(compile_error(&source, diagnostics)),
    };
    // Every wire and function, called or not, composes.
    let diagnostics = program.compose();
    if !diagnostics.is_empty() {
      return Err(compile_error(&program.source, diagnostics));
    }
    if program.lowered.run.is_some() {
      return Err(ToolError::Definition(
        "a tool has no `@run`: its top-level code outputs the definition".into(),
      ));
    }
    if !program.lowered.root {
      return Err(ToolError::Definition(
        "the script has no top-level code: it must end with the tool's table, `{definition: {...} use: \"Function\"}`".into(),
      ));
    }
    let root = crate::lower::ROOT_WIRE;
    let mut mesh = program.mesh();
    let compiled = mesh.compile(root, Type::none()).map_err(|e| {
      compile_error(
        &program.source,
        vec![program.diagnostic(PathStep::Wire(root.into()), e)],
      )
    })?;
    let effects = compiled.flow.analysis.effects;
    if effects.io || effects.suspends || effects.time || effects.random || effects.unknown {
      return Err(ToolError::Definition(
        "the top-level code must only build the definition table: no I/O, waiting or clock".into(),
      ));
    }
    let id = mesh
      .spawn(&compiled, Var::None)
      .map_err(|e| ToolError::Definition(e.to_string()))?;
    mesh.run(DEFINITION_TICKS);
    let value = match mesh.take_outcome(id) {
      Some(Outcome::Completed(value)) => value,
      Some(Outcome::Failed(e)) => return Err(ToolError::Definition(e.to_string())),
      _ => {
        return Err(ToolError::Definition(
          "the top-level code did not output the definition".into(),
        ));
      }
    };
    let (definition, entry) = read_definition(&value)?;
    // The function must accept the call table: compose the call once now.
    let call = call_mesh(&program, &entry, &Services::default())
      .compile(CALL_WIRE, call_type())
      .map_err(|e| {
        let d = program.diagnostic(PathStep::Function(entry.clone()), e);
        compile_error(&program.source, vec![d])
      })?;
    Ok(Tool {
      inner: Arc::new(Inner {
        program,
        definition,
        entry,
        call,
      }),
    })
  }

  pub fn definition(&self) -> &ToolDefinition {
    &self.inner.definition
  }

  /// What a call may do (I/O, waiting, the clock, randomness, or effects a
  /// host shard did not declare), inferred at load through everything the
  /// function calls. A tool with [`Effects::NONE`] only computes: a host
  /// may run it without asking. Declared by shards, a trusted contract, not
  /// a sandbox.
  pub fn effects(&self) -> Effects {
    self.inner.call.flow.analysis.effects
  }

  /// The name of the function a call runs.
  pub fn entry(&self) -> &str {
    &self.inner.entry
  }

  /// The files the script included or read.
  pub fn files(&self) -> &[String] {
    &self.inner.program.files
  }

  /// Calls the tool with default options. Blocks until it finishes.
  pub fn call(&self, input: &ToolInput) -> Result<ToolOutput, ToolError> {
    self.call_with(input, &CallOptions::default())
  }

  /// Calls the tool: a fresh mesh on this thread, ticked until the
  /// function finishes. While it waits (on I/O, which runs on `otter-io`'s
  /// runtime, or a `Pause`) the thread sleeps until the I/O completes, the
  /// pause is due, the time limit passes or the call is cancelled
  /// (`Mesh::wait`). Blocks until it finishes, fails, is cancelled or times
  /// out.
  pub fn call_with(
    &self,
    input: &ToolInput,
    options: &CallOptions,
  ) -> Result<ToolOutput, ToolError> {
    let value = input.to_var()?;
    let services = call_services(&options.services, &input.working_dir)?;
    let (result, log) = otter_core::log::capture(|| self.run(value, options, &services));
    match result {
      Ok(Var::String(ref s)) => Ok(ToolOutput {
        text: s.to_string(),
        log,
      }),
      Ok(other) => Ok(ToolOutput {
        text: otter_core::json::write(&other).unwrap_or_else(|_| other.text()),
        log,
      }),
      Err(RunError::Failed(message)) => Err(ToolError::Failed { message, log }),
      Err(RunError::Stopped) => Err(ToolError::Stopped { log }),
      Err(RunError::Cancelled) => Err(ToolError::Cancelled { log }),
      Err(RunError::TimedOut(limit)) => Err(ToolError::TimedOut { limit, log }),
      Err(RunError::Input(m)) => Err(ToolError::Input(m)),
    }
  }

  fn run(&self, value: Var, options: &CallOptions, services: &Services) -> Result<Var, RunError> {
    let mut mesh = call_mesh(&self.inner.program, &self.inner.entry, services);
    let id = mesh
      .spawn(&self.inner.call, value)
      .map_err(|e| RunError::Input(e.to_string()))?;
    let started = Instant::now();
    let deadline = options.timeout.map(|t| started + t);
    let budget =
      (options.cancel.is_some() || deadline.is_some() || options.fuel.is_some()).then(|| {
        otter_core::Budget {
          fuel: options.fuel,
          cancel: options.cancel.clone(),
          deadline,
        }
        .start()
      });
    // A budgeted run stops inside a tick with `Error::Cancelled`: past the
    // deadline that is a time-out.
    let interrupted = || match options.timeout {
      Some(limit) if started.elapsed() >= limit => RunError::TimedOut(limit),
      _ => RunError::Cancelled,
    };
    loop {
      // `tick` counts every running instance, including wires the call
      // spawned: the call is done when its own instance is. Dropping the
      // mesh cancels what it left running.
      let running = match &budget {
        Some(budget) => mesh.tick_budgeted(budget),
        None => mesh.tick(),
      };
      if running == 0 || mesh.outcome(id).is_some() {
        break;
      }
      if options.cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
        mesh.cancel(id);
        return Err(RunError::Cancelled);
      }
      if let Some(limit) = options.timeout
        && started.elapsed() >= limit
      {
        mesh.cancel(id);
        return Err(RunError::TimedOut(limit));
      }
      mesh.wait(deadline, options.cancel.as_ref());
    }
    match mesh.take_outcome(id) {
      Some(Outcome::Completed(value)) => Ok(value),
      Some(Outcome::Failed(otter_core::Error::Cancelled)) => Err(interrupted()),
      // Fuel spent: in the engine's step (`out-of-fuel`), or in a charge
      // shared with compose-time evaluation, worded as one.
      Some(Outcome::Failed(otter_core::Error::Diagnostic(d)))
        if d.code == "out-of-fuel" || d.code == "expansion-budget" =>
      {
        Err(RunError::Failed(format!(
          "the call ran out of fuel (budget {})",
          options.fuel.unwrap_or(u64::MAX)
        )))
      }
      Some(Outcome::Failed(e)) => Err(RunError::Failed(failure_message(&e))),
      Some(Outcome::Stopped) => Err(RunError::Stopped),
      Some(Outcome::Cancelled) | None => Err(RunError::Cancelled),
    }
  }
}

/// A call's services: the host's, and unless they hold an [`FsAccess`],
/// file access confined to the call's working directory, or none without
/// one. A tool is usually written by a model, which rarely thinks to scope
/// its files: confinement is the default, and a host that wants more says
/// so (`FsAccess::unrestricted()` in `CallOptions::services`).
/// A working directory that does not exist holds no files: no access.
fn call_services(services: &Services, working_dir: &str) -> Result<Services, ToolError> {
  if services.get::<FsAccess>().is_some() {
    return Ok(services.clone());
  }
  let access = match working_dir {
    "" => FsAccess::denied(),
    dir => FsAccess::within(dir).unwrap_or_else(|_| FsAccess::denied()),
  };
  let mut services = services.clone();
  services.insert(access);
  Ok(services)
}

/// A fresh mesh for a call: the program, the call wire, the services.
fn call_mesh(program: &Program, entry: &str, services: &Services) -> Mesh {
  let mut mesh = program.mesh();
  *mesh.services_mut() = services.clone();
  mesh.add_wire(WireDef {
    name: CALL_WIRE.into(),
    looped: false,
    flow: vec![ShardDef::call(entry, Vec::new())],
  });
  mesh
}

/// How a call ended without an output; `call_with` adds the log.
enum RunError {
  Failed(String),
  Stopped,
  Cancelled,
  TimedOut(Duration),
  Input(String),
}

/// An activation error's message without the wrapper's prefix.
fn failure_message(e: &otter_core::Error) -> String {
  match e {
    otter_core::Error::Activation(m) => m.clone(),
    other => other.to_string(),
  }
}

fn read_definition(value: &Var) -> Result<(ToolDefinition, String), ToolError> {
  let bad = |m: &str| ToolError::Definition(m.to_string());
  let Some(table) = value.as_table() else {
    return Err(ToolError::Definition(format!(
      "the top-level code must output `{{definition: {{...}} use: \"Function\"}}`, got {}",
      value.type_of()
    )));
  };
  let Some(definition) = table.get("definition").and_then(Var::as_table) else {
    return Err(bad("`definition` must be a table"));
  };
  let Some(entry) = table.get("use").and_then(Var::as_str) else {
    return Err(bad(
      "`use` must name the function a call runs, as a String: `use: \"Search\"`",
    ));
  };
  let Some(name) = definition.get("name").and_then(Var::as_str) else {
    return Err(bad("`definition.name` must be a String"));
  };
  if name.is_empty() {
    return Err(bad("`definition.name` must not be empty"));
  }
  let description = match definition.get("description") {
    None => String::new(),
    Some(d) => d
      .as_str()
      .ok_or_else(|| bad("`definition.description` must be a String"))?
      .to_string(),
  };
  let parameters = match definition.get("parameters") {
    None => Var::table([
      ("type", Var::string("object")),
      ("properties", Var::table(Vec::<(&str, Var)>::new())),
    ]),
    Some(p) if p.as_table().is_some() => p.clone(),
    Some(_) => return Err(bad("`definition.parameters` must be a table (JSON Schema)")),
  };
  let parameters = otter_core::json::write(&parameters)
    .map_err(|e| ToolError::Definition(format!("`definition.parameters`: {e}")))?;
  Ok((
    ToolDefinition {
      name: name.to_string(),
      description,
      parameters,
    },
    entry.to_string(),
  ))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn tools_are_send_and_sync() {
    fn check<T: Send + Sync>() {}
    check::<Tool>();
    check::<ToolError>();
  }
}
