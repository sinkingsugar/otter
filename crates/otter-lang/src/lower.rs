//! Lowering: syntax tree to `WireDef`/`ShardDef`, against a catalog.
//!
//! Lowering resolves names and argument forms (the catalog's parameter
//! declarations decide whether `{}` is a flow or a table, and whether a
//! bare name is a variable or a wire), and records each emitted shard's
//! span in a [`SourceMap`] keyed by its occurrence path, the same path
//! compose diagnostics carry. Spans never enter the definitions, which are
//! compose cache keys.
//!
//! The top level holds declarations (`@fn`, `@wire`, `@mesh`, `@schedule`,
//! `@run`); loose code becomes the `root` wire when there is no `@run`
//! (docs/surface-syntax-review.md §5.10). A function (golden path §3.1) is
//! declared with its signature and called like a shard; its body lowers
//! under the function's own source-map root. Constructs that need shards
//! not ported yet are rejected explicitly, with what to write instead.

use otter_core::hash::{HashMap, HashSet};
use std::sync::Arc;

use otter_core::describe::{self, Forms, ParamDecl};
use otter_core::diagnostic::{Diagnostic, PathStep};
use otter_core::shards::data::{SEQ_MAKE, STRING_FORMAT, TABLE_MAKE, TAKE};
use otter_core::shards::{BIND, CONST, GET, RUN, SUB};
use otter_core::{
  Arg, Catalog, FunctionDef, FunctionParam, ParamValue, ShardDef, Type, Var, WireDef,
  function::FlowType,
};

use crate::ast::*;
use crate::lexer::AssignOp;
use crate::problem::{Problem, closest};
use crate::source::Span;

/// The name of the wire that holds loose top-level code.
pub const ROOT_WIRE: &str = "root";

/// Where each emitted shard came from, by occurrence path.
#[derive(Default, Debug)]
pub struct SourceMap {
  /// Roots by wire name; child paths share prefixes instead of copying a
  /// complete path into each descendant. Flat ownership keeps drop iterative.
  roots: HashMap<String, usize>,
  nodes: Vec<SourceNode>,
  /// Each wire's declaration.
  wires: HashMap<String, Span>,
}

#[derive(Default, Debug)]
struct SourceNode {
  children: Vec<(PathStep, usize)>,
  span: Option<Span>,
}

// A borrowed ordering key keeps lookups allocation-free, including wide flows.
fn source_step_key(step: &PathStep) -> (u8, usize, &str) {
  match step {
    PathStep::Wire(name) => (0, 0, name),
    PathStep::Shard { index, name } => (1, *index, name),
    PathStep::Param(name) => (2, 0, name),
    PathStep::Item(index) => (3, *index, ""),
    PathStep::Function(name) => (4, 0, name),
    PathStep::Evaluation => (5, 0, ""),
  }
}

impl SourceNode {
  fn search(&self, step: &PathStep) -> Result<usize, usize> {
    self
      .children
      .binary_search_by(|(key, _)| source_step_key(key).cmp(&source_step_key(step)))
  }
}

impl SourceMap {
  fn insert(&mut self, wire: &str, path: Vec<PathStep>, span: Span) {
    let mut node = if let Some(&root) = self.roots.get(wire) {
      root
    } else {
      let root = self.nodes.len();
      self.nodes.push(SourceNode::default());
      self.roots.insert(wire.to_owned(), root);
      root
    };
    for step in path {
      node = match self.nodes[node].search(&step) {
        Ok(i) => self.nodes[node].children[i].1,
        Err(i) => {
          let child = self.nodes.len();
          self.nodes.push(SourceNode::default());
          // Most source paths append siblings in source order. Keep compact
          // sorted edges so wide flows still have logarithmic lookup.
          let edges = &mut self.nodes[node].children;
          if edges.is_empty() {
            edges.reserve_exact(1);
          }
          edges.insert(i, (step, child));
          child
        }
      };
    }
    self.nodes[node].span = Some(span);
  }

  /// The source span for a compose diagnostic: the shard at the end of its
  /// path, or the closest enclosing one that is known. A nested Wire resets
  /// the lookup to that definition, independent of its calling occurrence.
  pub fn locate(&self, d: &Diagnostic) -> Option<Span> {
    let k = d
      .path
      .iter()
      .rposition(|s| matches!(s, PathStep::Wire(_) | PathStep::Function(_)))?;
    let key = match &d.path[k] {
      PathStep::Wire(wire) => wire.clone(),
      PathStep::Function(function) => function_key(function),
      _ => return None,
    };
    let mut span = self.wires.get(&key).copied();
    let Some(&root) = self.roots.get(&key) else {
      return span;
    };
    let mut node = root;
    span = self.nodes[node].span.or(span);
    for step in &d.path[k + 1..] {
      let Ok(i) = self.nodes[node].search(step) else {
        break;
      };
      node = self.nodes[node].children[i].1;
      span = self.nodes[node].span.or(span);
    }
    span
  }
}

/// Functions and wires share the source map's tables under distinct keys.
fn function_key(name: &str) -> String {
  format!("@fn {name}")
}

#[derive(Debug)]
pub struct MeshDecl {
  pub name: String,
  pub span: Span,
  pub scheduled: Vec<(String, Span)>,
}

#[derive(Debug)]
pub struct RunDecl {
  pub mesh: String,
  pub fps: Option<f64>,
  pub iterations: Option<i64>,
  pub span: Span,
}

/// A lowered program.
#[derive(Debug, Default)]
pub struct Lowered {
  pub wires: Vec<WireDef>,
  /// Declared functions, in source order; a call site names one.
  pub functions: Vec<FunctionDef>,
  pub meshes: Vec<MeshDecl>,
  pub run: Option<RunDecl>,
  /// Set when loose top-level code was wrapped into [`ROOT_WIRE`].
  pub root: bool,
  pub map: SourceMap,
}

struct Lowerer<'a> {
  catalog: &'a Catalog,
  defines: &'a std::collections::HashMap<String, String>,
  problems: Vec<Problem>,
  map: SourceMap,
  /// The wire (or function root) being lowered.
  wire: String,
  /// Counter for the temporaries that hold computed values.
  temps: usize,
  /// Declared functions' shapes, known before any call lowers.
  functions: HashMap<String, FnShape>,
  /// `@const` declarations: the value block, lowered at each read (a `#( )`
  /// inside it is recorded under the reading occurrence).
  consts: HashMap<String, Block>,
  /// Constants being lowered, innermost last (`compose-time-cycle`).
  const_stack: Vec<String>,
  /// Constants already lowered once: their problems were reported then,
  /// and lowering them again at a read reports nothing new.
  consts_checked: HashSet<String>,
  /// Script arguments a default typed (`@const(... ignore-redefined:
  /// true)`): read from `consts`, where the parsed argument is.
  typed_defines: HashSet<String>,
  /// Constants without a `#( )`, lowered once to a value every read
  /// shares (`None` when lowering failed, reported then).
  const_values: HashMap<String, Option<Var>>,
  /// Reads of constants so far, to tell a constant built from others.
  const_reads: usize,
  /// Source-map nodes that reads of constants holding a `#( )` added so
  /// far (see [`EVAL_CONST_NODES`]).
  const_expansion: usize,
  /// The read that took those past [`EVAL_CONST_NODES`]; kept apart from
  /// `problems`, which a repeated read truncates.
  const_overflow: Option<Problem>,
  /// The flow parameters of the function whose body is being lowered: a
  /// shard's flow argument naming one runs it (`{ Run(name) }`).
  flow_params: Vec<String>,
  /// What `@read` reads from: the source (which says which file a span is
  /// in, for relative paths) and the reader. `None` when lowering a tree
  /// alone.
  files: Option<(&'a crate::Source, &'a dyn crate::Files)>,
  /// The files `@read` read, by name.
  reads: Vec<String>,
}

/// What a value constant built from others may expand to, by its size as
/// text: the platform's default value limit
/// (`EvalLimits::default().value_bytes`). Constants built from each other
/// multiply at every level, so without a bound a few lines expand past any
/// memory before compose-time limits apply.
fn const_limit() -> usize {
  otter_core::compose_time::EvalLimits::default().value_bytes
}

/// How many source-map nodes the reads of constants holding a `#( )` may
/// add in total. Such a constant is lowered again at each read (its
/// pipelines are recorded under the reading occurrence), and each node is
/// a lowered step with its location, about a hundred bytes: a bound on
/// nodes bounds that memory, where one on source text would not.
const EVAL_CONST_NODES: usize = if cfg!(target_os = "espidf") {
  128
} else {
  32_768
};

/// What lowering needs of a declared function: its parameter names, and
/// whether each is a flow parameter.
struct FnShape {
  params: Vec<(String, bool)>,
}

/// One parameter as lowering sees it, from a native declaration or a
/// function's signature.
struct DeclView {
  name: String,
  forms: Forms,
  variadic: bool,
}

impl DeclView {
  fn native(d: &ParamDecl) -> DeclView {
    DeclView {
      name: d.name.to_string(),
      forms: d.forms,
      variadic: d.requirement == describe::Requirement::Variadic,
    }
  }

  /// A function's parameter: a value, or a block (`{...}`, or a flow
  /// parameter of the calling function, passed on by name).
  fn function(name: &str, flow: bool) -> DeclView {
    DeclView {
      name: name.to_string(),
      forms: if flow {
        Forms::FLOW.or(Forms::VARIABLE)
      } else {
        Forms::LITERAL.or(Forms::VARIABLE)
      },
      variadic: false,
    }
  }
}

/// What a `Name(...)` block resolves to.
enum Target {
  Native(&'static otter_core::ShardType),
  Function(String),
}

impl Target {
  fn name(&self) -> &str {
    match self {
      Target::Native(ty) => ty.name(),
      Target::Function(name) => name,
    }
  }
}

/// Lowers a parsed program. `defines` are the script arguments (`@name`).
pub fn lower(
  program: &Program,
  catalog: &Catalog,
  defines: &std::collections::HashMap<String, String>,
) -> (Lowered, Vec<Problem>) {
  let (lowered, problems, _) = lower_files(program, catalog, defines, None);
  (lowered, problems)
}

/// [`lower`], with `@read` reading from `files`, relative to the file of
/// `source` that names the path. Returns the names of the files it read.
pub fn lower_with(
  program: &Program,
  catalog: &Catalog,
  defines: &std::collections::HashMap<String, String>,
  source: &crate::Source,
  files: &dyn crate::Files,
) -> (Lowered, Vec<Problem>, Vec<String>) {
  lower_files(program, catalog, defines, Some((source, files)))
}

fn lower_files<'a>(
  program: &Program,
  catalog: &'a Catalog,
  defines: &'a std::collections::HashMap<String, String>,
  files: Option<(&'a crate::Source, &'a dyn crate::Files)>,
) -> (Lowered, Vec<Problem>, Vec<String>) {
  let mut l = Lowerer {
    catalog,
    defines,
    problems: Vec::new(),
    map: SourceMap::default(),
    wire: String::new(),
    temps: 0,
    functions: HashMap::default(),
    consts: HashMap::default(),
    const_stack: Vec::new(),
    consts_checked: HashSet::default(),
    typed_defines: HashSet::default(),
    const_values: HashMap::default(),
    const_reads: 0,
    const_expansion: 0,
    const_overflow: None,
    flow_params: Vec::new(),
    files,
    reads: Vec::new(),
  };
  let mut out = Lowered::default();
  // Script-argument defaults before anything reads a name: a function's
  // parameter defaults may read one, and must see it with its type. The
  // same pass finds the functions, whose signatures come next.
  let mut bodies = Vec::new();
  for stmt in &program.statements {
    match top_level_func(stmt) {
      Some((name, _, _)) if name.node == "fn" => bodies.push(Signature::Pending(stmt)),
      Some((name, params, block)) if name.node == "const" && is_default(params) => {
        l.piped_declaration(name, stmt);
        l.const_decl(block, params);
      }
      _ => {}
    }
  }
  // Function signatures next: a call may precede its declaration, and
  // bodies may call each other.
  for signature in &mut bodies {
    if let Signature::Pending(stmt) = *signature
      && let Statement::Pipeline(pipe) = stmt
      && let block = &pipe.blocks[0]
      && let BlockKind::Func { name, params } = &block.kind
    {
      let params = params.as_ref();
      l.piped_declaration(name, stmt);
      *signature = match l.fn_header(&mut out, block, params) {
        Some((index, body)) => Signature::Lowered(
          u32::try_from(index).expect("fewer than 2^32 functions"),
          body,
          block.span,
        ),
        None => Signature::Failed,
      };
    }
  }
  // Constants next: bodies and wires read them, and a constant may call a
  // function. Each is checked once here, under its own source-map root.
  for stmt in &program.statements {
    if let Some((name, params, block)) = top_level_func(stmt)
      && name.node == "const"
      && !is_default(params)
    {
      l.piped_declaration(name, stmt);
      l.const_decl(block, params);
    }
  }
  let mut names: Vec<String> = l.consts.keys().cloned().collect();
  names.sort();
  for name in names {
    l.wire = format!("@const {name}");
    l.temps = 0;
    let _ = l.read_const(&name, &[]);
  }
  for signature in bodies {
    let Signature::Lowered(index, body, span) = signature else {
      continue;
    };
    let index = index as usize;
    let name = out.functions[index].name.clone();
    l.wire = function_key(&name);
    l.temps = 0;
    l.map.wires.insert(l.wire.clone(), span);
    l.flow_params = out.functions[index]
      .params
      .iter()
      .filter(|p| p.is_flow())
      .map(|p| p.name.clone())
      .collect();
    out.functions[index].body = match body {
      Some(stmts) => l.statements(stmts.iter(), &[]),
      None => Vec::new(),
    };
  }
  l.flow_params.clear();
  let mut loose: Vec<&Statement> = Vec::new();
  for stmt in &program.statements {
    match top_level_func(stmt) {
      Some((name, _, _)) if name.node == "fn" || name.node == "const" => {}
      Some((name, params, block)) => l.declaration(&mut out, name, params, block, stmt),
      None => loose.push(stmt),
    }
  }
  if let Some(first) = loose.first() {
    if out.run.is_some() {
      l.problems.push(
        Problem::construct(
          statement_span(first),
          "generic",
          "code-outside-wire",
          "code outside a wire: with `@run`, the top level only declares".into(),
        )
        .help("put this code in a `@wire(...)` and `@schedule` it on the mesh"),
      );
    } else if out.wires.iter().any(|w| w.name == ROOT_WIRE) {
      l.problems.push(Problem::construct(
        statement_span(first),
        "generic",
        "root-wire-name",
        format!(
          "loose code runs as the `{ROOT_WIRE}` wire, but a wire is already named `{ROOT_WIRE}`"
        ),
      ));
    } else {
      l.wire = ROOT_WIRE.to_string();
      l.temps = 0;
      let span = statement_span(first).to(statement_span(loose[loose.len() - 1]));
      l.map.wires.insert(ROOT_WIRE.into(), span);
      let flow = l.statements(loose.iter().copied(), &[]);
      out.wires.push(WireDef {
        name: ROOT_WIRE.into(),
        looped: false,
        flow,
      });
      out.root = true;
    }
  }
  l.check_references(&out);
  if out.run.is_none()
    && let Some((_, span)) = out.meshes.iter().flat_map(|m| m.scheduled.iter()).next()
  {
    l.problems.push(
      Problem::construct(
        *span,
        "generic",
        "schedule-without-run",
        "`@schedule` has no effect without `@run`".into(),
      )
      .help("add `@run(mesh)` to start the mesh, or remove the schedule"),
    );
  }
  for node in &mut l.map.nodes {
    node.children.shrink_to_fit();
  }
  l.map.nodes.shrink_to_fit();
  out.map = l.map;
  l.problems.extend(l.const_overflow);
  let reads = std::mem::take(&mut l.reads);
  (out, l.problems, reads)
}

/// A top-level `@name(...)` statement (one block, nothing piped).
fn top_level_func(stmt: &Statement) -> Option<(&Name, Option<&Params>, &Block)> {
  let Statement::Pipeline(pipe) = stmt else {
    return None;
  };
  match &pipe.blocks[0].kind {
    BlockKind::Func { name, params }
      if matches!(
        name.node.as_str(),
        "fn" | "wire" | "mesh" | "schedule" | "run" | "define" | "template" | "const"
      ) =>
    {
      Some((name, params.as_ref(), &pipe.blocks[0]))
    }
    _ => None,
  }
}

/// A function's signature while the program's top level is lowered: found
/// (with the defaults), then lowered, its body after the constants. The
/// index is a `u32` so the tag shares its word: the list is no larger
/// than the `(index, body, span)` list it replaced on 64-bit targets (4
/// bytes larger per function on 32-bit ones, where that tuple has no
/// padding to share).
enum Signature<'s> {
  Pending(&'s Statement),
  Lowered(u32, Option<&'s [Statement]>, Span),
  Failed,
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(
  std::mem::size_of::<Signature>() <= std::mem::size_of::<(usize, Option<&[Statement]>, Span)>()
);

/// Whether a `@const`'s parameters end with `ignore-redefined: true`: a
/// default for a script argument.
fn is_default(params: Option<&Params>) -> bool {
  params.and_then(|p| p.items.last()).is_some_and(|last| {
    last
      .name
      .as_ref()
      .is_some_and(|n| n.node == "ignore-redefined")
      && matches!(
        &last.value.blocks[..],
        [b] if b.kind == BlockKind::Literal(Literal::Bool(true))
      )
  })
}

fn statement_span(stmt: &Statement) -> Span {
  match stmt {
    Statement::Assign { op_span, var, .. } => op_span.to(var.span),
    Statement::Pipeline(p) => p.span,
  }
}

/// A param's value as a plain name (`main`, or `"main"`).
fn name_of(pipe: &Pipe) -> Option<String> {
  match &pipe.blocks[..] {
    [b] => match &b.kind {
      BlockKind::Var { name, path } if path.is_empty() => Some(name.node.clone()),
      BlockKind::Literal(Literal::String(s)) => Some(s.clone()),
      _ => None,
    },
    _ => None,
  }
}

/// A function's name: uppercase, like a shard's, since calls read as shards.
fn upper_name_of(pipe: &Pipe) -> Option<String> {
  match &pipe.blocks[..] {
    [b] => match &b.kind {
      BlockKind::Shard { name, params: None } => Some(name.node.clone()),
      _ => None,
    },
    _ => None,
  }
}

/// The type names written in signatures.
const TYPE_NAMES: &[&str] = &[
  "None", "Any", "Bool", "Int", "Float", "Float2", "Float3", "Float4", "String", "Bytes", "Seq",
  "Table",
];

fn named_type(name: &str) -> Option<Type> {
  Some(match name {
    "None" => Type::none(),
    "Any" => Type::any(),
    "Bool" => Type::bool(),
    "Int" => Type::int(),
    "Float" => Type::float(),
    "Float2" => Type::float2(),
    "Float3" => Type::float3(),
    "Float4" => Type::float4(),
    "String" => Type::string(),
    "Bytes" => Type::bytes(),
    "Seq" => Type::seq(Type::any()),
    "Table" => Type::any_table(),
    _ => return None,
  })
}

impl Lowerer<'_> {
  fn problem(&mut self, p: Problem) {
    self.problems.push(p);
  }

  fn piped_declaration(&mut self, name: &Name, stmt: &Statement) {
    if let Statement::Pipeline(pipe) = stmt
      && pipe.blocks.len() > 1
    {
      self.problem(Problem::construct(
        pipe.blocks[1].span,
        "generic",
        "piped-declaration",
        format!(
          "the result of `@{}` cannot be piped: it is a declaration",
          name.node
        ),
      ));
    }
  }

  /// `@fn(Name input: T output: T params: {...} ... { body })`: the
  /// signature, registered before any body lowers. Returns the function's
  /// index and its body block.
  fn fn_header<'s>(
    &mut self,
    out: &mut Lowered,
    block: &'s Block,
    params: Option<&'s Params>,
  ) -> Option<(usize, Option<&'s [Statement]>)> {
    let usage = "`@fn(Name input: Int output: Int params: {factor: Float} { ... })`";
    let Some(params) = params else {
      self.problem(Problem::construct(
        block.span,
        "generic",
        "declaration",
        format!("a function needs a name, a signature and a body: {usage}"),
      ));
      return None;
    };
    let (positional, named) = split_params(params);
    let Some(name) = positional.first().and_then(|p| upper_name_of(p)) else {
      let hint = match positional.first().and_then(|p| name_of(p)) {
        Some(lower) => format!(
          "a function name starts with an uppercase letter, since a call reads like a shard: `@fn({}{} ...)`",
          lower[..1].to_uppercase(),
          &lower[1..]
        ),
        None => format!("a function needs a name: {usage}"),
      };
      self.problem(Problem::construct(
        positional.first().map_or(block.span, |p| p.span),
        "generic",
        "declaration",
        hint,
      ));
      return None;
    };
    if self.catalog.get(&name).is_some() {
      self.problem(
        Problem::construct(
          positional[0].span,
          "generic",
          "function-name-collision",
          format!("`{name}` is already a shard in the catalog; pick another name"),
        )
        .shard(&name),
      );
      return None;
    }
    if self.functions.contains_key(&name) {
      self.problem(Problem::construct(
        block.span,
        "generic",
        "duplicate-function",
        format!("function `{name}` is declared twice"),
      ));
      return None;
    }
    let body = match positional.get(1).map(|p| &p.blocks[..]) {
      Some([b]) => match &b.kind {
        BlockKind::Flow(stmts) => Some(stmts.as_slice()),
        BlockKind::EmptyBraces => None,
        _ => {
          self.problem(Problem::construct(
            b.span,
            "generic",
            "declaration",
            format!("a function's body is a flow in braces: {usage}"),
          ));
          return None;
        }
      },
      Some(_) => {
        self.problem(Problem::construct(
          positional[1].span,
          "generic",
          "declaration",
          format!("a function's body is a flow in braces: {usage}"),
        ));
        return None;
      }
      None => {
        self.problem(Problem::construct(
          block.span,
          "generic",
          "declaration",
          format!("a function needs a body: {usage}"),
        ));
        return None;
      }
    };
    if positional.len() > 2 {
      self.problem(Problem::construct(
        positional[2].span,
        "generic",
        "declaration",
        format!("too many arguments: {usage}"),
      ));
      return None;
    }
    let mut def = FunctionDef::new(&name, Type::none(), Type::none());
    let (mut input, mut output, mut declared_params) = (None, None, None);
    let mut ok = true;
    for (pname, value) in &named {
      match pname.node.as_str() {
        "input" => input = self.type_expr(value),
        "output" => output = self.type_expr(value),
        "params" => declared_params = self.fn_params(value),
        "stateful" | "pure" => match &value.blocks[..] {
          [b] if matches!(b.kind, BlockKind::Literal(Literal::Bool(_))) => {
            let flag = b.kind == BlockKind::Literal(Literal::Bool(true));
            if pname.node == "stateful" {
              def.stateful = flag;
            } else {
              def.pure = flag;
            }
          }
          _ => {
            self.problem(
              Problem::construct(
                value.span,
                "generic",
                "declaration",
                format!("`{}` takes `true` or `false`", pname.node),
              )
              .param(&pname.node),
            );
            ok = false;
          }
        },
        "uses" | "mutates" => match self.name_list(value, &pname.node) {
          Some(names) if pname.node == "uses" => def.uses = names,
          Some(names) => def.mutates = names,
          None => ok = false,
        },
        other => {
          self.problem(
            Problem::construct(
              pname.span,
              "generic",
              "unsupported",
              format!(
                "function option `{other}` is not supported (supported: input, output, params, stateful, pure, uses, mutates)"
              ),
            )
            .did_you_mean(closest(
              other,
              ["input", "output", "params", "stateful", "pure", "uses", "mutates"]
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
              2,
            ))
            .param(other),
          );
          ok = false;
        }
      }
    }
    let missing: Vec<&str> = [
      ("input", named.iter().any(|(n, _)| n.node == "input")),
      ("output", named.iter().any(|(n, _)| n.node == "output")),
      ("params", named.iter().any(|(n, _)| n.node == "params")),
    ]
    .into_iter()
    .filter(|(_, given)| !given)
    .map(|(n, _)| n)
    .collect();
    if !missing.is_empty() {
      self.problem(Problem::construct(
        block.span,
        "generic",
        "declaration",
        format!(
          "`@fn` needs `{}:` (`params: {{}}` when there are none): {usage}",
          missing.join(":`, `")
        ),
      ));
      return None;
    }
    let (Some(input), Some(output), Some(declared_params)) = (input, output, declared_params)
    else {
      return None;
    };
    if !ok {
      return None;
    }
    def.input = input;
    def.output = output;
    def.params = declared_params;
    self.functions.insert(
      name.clone(),
      FnShape {
        params: def
          .params
          .iter()
          .map(|p| (p.name.clone(), p.is_flow()))
          .collect(),
      },
    );
    out.functions.push(def);
    Some((out.functions.len() - 1, body))
  }

  /// A type in a signature: a name (`Int`), `[T]`, `{key: T ...}`, `{}`
  /// (any table) or a union written with `|` (`Int | None`).
  fn type_expr(&mut self, pipe: &Pipe) -> Option<Type> {
    let mut members = Vec::with_capacity(pipe.blocks.len());
    for block in &pipe.blocks {
      members.push(self.type_block(block)?);
    }
    Some(if members.len() == 1 {
      members[0]
    } else {
      Type::union(members)
    })
  }

  fn type_block(&mut self, block: &Block) -> Option<Type> {
    match &block.kind {
      BlockKind::Shard { name, params: None } => match named_type(&name.node)
        .or_else(|| self.catalog.object_type(&name.node).map(|o| o.to_type()))
      {
        Some(ty) => Some(ty),
        None => {
          let objects = self.catalog.objects();
          let mut hint = TYPE_NAMES.join(", ");
          if !objects.is_empty() {
            hint.push_str("; objects: ");
            hint.push_str(
              &objects
                .iter()
                .map(|o| o.name)
                .collect::<Vec<_>>()
                .join(", "),
            );
          }
          self.problem(
            Problem::construct(
              name.span,
              "generic",
              "unknown-type",
              format!(
                "unknown type `{}` (types: {hint}, `[T]`, `{{key: T}}`, `T | None`)",
                name.node,
              ),
            )
            .did_you_mean(closest(
              &name.node,
              TYPE_NAMES
                .iter()
                .copied()
                .chain(objects.iter().map(|o| o.name))
                .map(str::to_string)
                .collect::<Vec<_>>(),
              3,
            )),
          );
          None
        }
      },
      BlockKind::Seq(items) => match &items[..] {
        [item] => Some(Type::seq(self.type_expr(item)?)),
        _ => {
          self.problem(Problem::construct(
            block.span,
            "generic",
            "declaration",
            "a sequence type names one element type: `[Int]`".into(),
          ));
          None
        }
      },
      BlockKind::Table(entries) => {
        let mut keys = Vec::with_capacity(entries.len());
        let mut seen = Vec::new();
        for (key, value) in entries {
          let k = self.table_key(key, &seen)?;
          seen.push(k.clone());
          keys.push((k, self.type_expr(value)?));
        }
        Some(Type::fixed_table(keys))
      }
      BlockKind::EmptyBraces => Some(Type::any_table()),
      _ => {
        self.problem(Problem::construct(
          block.span,
          "generic",
          "declaration",
          "expected a type here (`Int`, `[Float]`, `{x: Int}`, `String | None`)".into(),
        ));
        None
      }
    }
  }

  /// `params: {name: Type ...}`: a typed required parameter, or a literal
  /// default (`{factor: 2.0}`) whose type is the literal's.
  fn fn_params(&mut self, pipe: &Pipe) -> Option<Vec<FunctionParam>> {
    let entries = match &pipe.blocks[..] {
      [b] => match &b.kind {
        BlockKind::EmptyBraces => return Some(Vec::new()),
        BlockKind::Table(entries) => entries,
        _ => {
          self.problem(Problem::construct(
            pipe.span,
            "generic",
            "declaration",
            "`params` is a table of parameter names and types: `params: {factor: Float}` (`params: {}` for none)".into(),
          ));
          return None;
        }
      },
      _ => {
        self.problem(Problem::construct(
          pipe.span,
          "generic",
          "declaration",
          "`params` is a table of parameter names and types: `params: {factor: Float}`".into(),
        ));
        return None;
      }
    };
    let mut params = Vec::with_capacity(entries.len());
    let mut seen = Vec::new();
    for (key, value) in entries {
      let name = self.table_key(key, &seen)?;
      seen.push(name.clone());
      if !name.starts_with(|c: char| c.is_ascii_lowercase()) {
        self.problem(
          Problem::construct(
            key.span,
            "generic",
            "parameter-name",
            format!("parameter names are lowercase: `{}:`", name.to_lowercase()),
          )
          .fix(name.to_lowercase()),
        );
        return None;
      }
      if name == "input" {
        self.problem(
          Problem::construct(
            key.span,
            "generic",
            "reserved-name",
            "`input` is reserved: it names the entry value of the function; pick another parameter name".into(),
          )
          .param("input"),
        );
        return None;
      }
      let param = match &value.blocks[..] {
        [b] if is_constant(b) && !matches!(b.kind, BlockKind::EmptyBraces) => {
          let default = self.literal_now(b)?;
          FunctionParam {
            name,
            ty: default.type_of(),
            default: Some(default),
            flow: None,
          }
        }
        [b] if matches!(&b.kind, BlockKind::Shard { name, .. } if name.node == "Flow") => {
          FunctionParam {
            name,
            ty: Type::any(),
            default: None,
            flow: Some(self.flow_type(b)?),
          }
        }
        _ => FunctionParam {
          name,
          ty: self.type_expr(value)?,
          default: None,
          flow: None,
        },
      };
      params.push(param);
    }
    Some(params)
  }

  /// A flow parameter's type: `Flow` (any input, Run passes its input
  /// through), `Flow(input: T)`, or `Flow(input: T output: U)` (Run outputs
  /// the block's output).
  fn flow_type(&mut self, block: &Block) -> Option<FlowType> {
    let usage = "`Flow`, `Flow(input: Int)` or `Flow(input: Int output: Int)`";
    let BlockKind::Shard { params, .. } = &block.kind else {
      unreachable!("a Flow block")
    };
    let mut ty = FlowType {
      input: Type::any(),
      output: None,
    };
    for param in params.as_ref().map(|p| &p.items[..]).unwrap_or(&[]) {
      match param.name.as_ref().map(|n| n.node.as_str()) {
        Some("input") => ty.input = self.type_expr(&param.value)?,
        Some("output") => ty.output = Some(self.type_expr(&param.value)?),
        _ => {
          self.problem(Problem::construct(
            param.value.span,
            "generic",
            "declaration",
            format!("a flow parameter's type names its `input:` and `output:` types: {usage}"),
          ));
          return None;
        }
      }
    }
    Some(ty)
  }

  /// `uses: [gain]` or `uses: gain`: mesh variable names.
  fn name_list(&mut self, pipe: &Pipe, option: &str) -> Option<Vec<String>> {
    let items: Vec<&Pipe> = match &pipe.blocks[..] {
      [b] => match &b.kind {
        BlockKind::Seq(items) => items.iter().collect(),
        BlockKind::Var { path, .. } if path.is_empty() => vec![pipe],
        _ => Vec::new(),
      },
      _ => Vec::new(),
    };
    let mut names = Vec::with_capacity(items.len());
    for item in &items {
      match name_of(item) {
        Some(name) => names.push(name),
        None => {
          self.problem(
            Problem::construct(
              item.span,
              "generic",
              "declaration",
              format!("`{option}` lists mesh variable names: `{option}: [gain count]`"),
            )
            .param(option),
          );
          return None;
        }
      }
    }
    if items.is_empty() {
      self.problem(
        Problem::construct(
          pipe.span,
          "generic",
          "declaration",
          format!("`{option}` lists mesh variable names: `{option}: [gain count]`"),
        )
        .param(option),
      );
      return None;
    }
    Some(names)
  }

  fn declaration(
    &mut self,
    out: &mut Lowered,
    name: &Name,
    params: Option<&Params>,
    block: &Block,
    stmt: &Statement,
  ) {
    self.piped_declaration(name, stmt);
    let empty = Params {
      items: Vec::new(),
      span: block.span,
    };
    let params = params.unwrap_or(&empty);
    let (positional, named) = split_params(params);
    match name.node.as_str() {
      "wire" => self.wire_decl(out, block, &positional, &named),
      "mesh" => match positional.first().and_then(|p| name_of(p)) {
        Some(mesh) if positional.len() == 1 && named.is_empty() => {
          if out.meshes.iter().any(|m| m.name == mesh) {
            self.problem(Problem::construct(
              block.span,
              "generic",
              "duplicate-mesh",
              format!("mesh `{mesh}` is declared twice"),
            ));
          }
          out.meshes.push(MeshDecl {
            name: mesh,
            span: block.span,
            scheduled: Vec::new(),
          });
        }
        _ => self.problem(Problem::construct(
          block.span,
          "generic",
          "declaration",
          "`@mesh` takes one name: `@mesh(main)`".into(),
        )),
      },
      "schedule" => match (
        positional.first().and_then(|p| name_of(p)),
        positional.get(1).and_then(|p| name_of(p)),
      ) {
        (Some(mesh), Some(wire)) if positional.len() == 2 && named.is_empty() => {
          match out.meshes.iter_mut().find(|m| m.name == mesh) {
            Some(m) => m.scheduled.push((wire, block.span)),
            None => self.problem(Problem::construct(
              positional[0].span,
              "generic",
              "unknown-mesh",
              format!("mesh `{mesh}` is not declared (declare it first with `@mesh({mesh})`)"),
            )),
          }
        }
        _ => self.problem(Problem::construct(
          block.span,
          "generic",
          "declaration",
          "`@schedule` takes a mesh and a wire: `@schedule(main my-wire)`".into(),
        )),
      },
      "run" => self.run_decl(out, block, &positional, &named),
      other => self.problem(Problem::construct(
        name.span,
        "generic",
        "unsupported",
        format!("`@{other}` is not supported yet"),
      )),
    }
  }

  fn wire_decl(
    &mut self,
    out: &mut Lowered,
    block: &Block,
    positional: &[&Pipe],
    named: &[(&Name, &Pipe)],
  ) {
    let usage = "`@wire(name { ... } looped: true)`";
    let Some(name) = positional.first().and_then(|p| name_of(p)) else {
      self.problem(Problem::construct(
        block.span,
        "generic",
        "declaration",
        format!("a wire needs a name: {usage}"),
      ));
      return;
    };
    let mut looped = false;
    for (pname, value) in named {
      match (pname.node.as_str(), &value.blocks[..]) {
        ("looped", [b]) if matches!(b.kind, BlockKind::Literal(Literal::Bool(_))) => {
          looped = b.kind == BlockKind::Literal(Literal::Bool(true));
        }
        ("looped", _) => self.problem(
          Problem::construct(
            value.span,
            "generic",
            "declaration",
            "`looped` takes `true` or `false`".into(),
          )
          .param("looped"),
        ),
        (other, _) => self.problem(
          Problem::construct(
            pname.span,
            "generic",
            "unsupported",
            format!("wire option `{other}` is not supported (supported: looped)"),
          )
          .did_you_mean(closest(other, ["looped".to_string()], 1))
          .param(other),
        ),
      }
    }
    if positional.len() > 2 {
      self.problem(Problem::construct(
        positional[2].span,
        "generic",
        "declaration",
        format!("too many arguments: {usage}"),
      ));
    }
    if out.wires.iter().any(|w| w.name == name) {
      self.problem(Problem::construct(
        block.span,
        "generic",
        "duplicate-wire",
        format!("wire `{name}` is declared twice"),
      ));
      return;
    }
    self.wire = name.clone();
    // Temporary identities must be stable when an unrelated wire is edited.
    // Compose gives each temporary Ref occurrence a fresh slot on inlining.
    self.temps = 0;
    self.map.wires.insert(name.clone(), block.span);
    let flow = match positional.get(1).map(|p| &p.blocks[..]) {
      Some([b]) => match &b.kind {
        BlockKind::Flow(stmts) => self.statements(stmts.iter(), &[]),
        BlockKind::EmptyBraces => Vec::new(),
        _ => {
          self.problem(Problem::construct(
            b.span,
            "generic",
            "declaration",
            format!("a wire's body is a flow in braces: {usage}"),
          ));
          Vec::new()
        }
      },
      Some(_) => {
        self.problem(Problem::construct(
          positional[1].span,
          "generic",
          "declaration",
          format!("a wire's body is a flow in braces: {usage}"),
        ));
        Vec::new()
      }
      None => Vec::new(),
    };
    out.wires.push(WireDef { name, looped, flow });
  }

  fn run_decl(
    &mut self,
    out: &mut Lowered,
    block: &Block,
    positional: &[&Pipe],
    named: &[(&Name, &Pipe)],
  ) {
    let Some(mesh) = positional.first().and_then(|p| name_of(p)) else {
      self.problem(Problem::construct(
        block.span,
        "generic",
        "declaration",
        "`@run` takes a mesh: `@run(main fps: 30)`".into(),
      ));
      return;
    };
    if out.run.is_some() {
      self.problem(Problem::construct(
        block.span,
        "generic",
        "declaration",
        "only one `@run` is allowed".into(),
      ));
      return;
    }
    let mut run = RunDecl {
      mesh,
      fps: None,
      iterations: None,
      span: block.span,
    };
    for (pname, value) in named {
      let literal = match &value.blocks[..] {
        [b] => match b.kind {
          BlockKind::Literal(Literal::Int(i)) => Some(i as f64),
          BlockKind::Literal(Literal::Float(f)) => Some(f),
          _ => None,
        },
        _ => None,
      };
      match (pname.node.as_str(), literal) {
        // The frame interval must be a representable duration.
        ("fps", Some(f)) if f > 0.0 && std::time::Duration::try_from_secs_f64(1.0 / f).is_ok() => {
          run.fps = Some(f)
        }
        // Below 2^63, the exact Int range.
        ("iterations", Some(i))
          if i >= 1.0 && i.fract() == 0.0 && i < 9_223_372_036_854_775_808.0 =>
        {
          run.iterations = Some(i as i64)
        }
        ("fps" | "iterations", _) => self.problem(
          Problem::construct(
            value.span,
            "generic",
            "declaration",
            if pname.node == "fps" {
              "`fps` takes a positive number of frames per second".to_string()
            } else {
              "`iterations` takes a positive whole number".to_string()
            },
          )
          .param(&pname.node),
        ),
        (other, _) => self.problem(
          Problem::construct(
            pname.span,
            "generic",
            "unsupported",
            format!("`@run` option `{other}` is not supported (supported: fps, iterations)"),
          )
          .did_you_mean(closest(
            other,
            ["fps".to_string(), "iterations".to_string()],
            2,
          )),
        ),
      }
    }
    out.run = Some(run);
  }

  /// After all declarations: scheduled wires and the run mesh exist.
  fn check_references(&mut self, out: &Lowered) {
    let wires: Vec<String> = out.wires.iter().map(|w| w.name.clone()).collect();
    for mesh in &out.meshes {
      for (wire, span) in &mesh.scheduled {
        if !wires.contains(wire) {
          self.problems.push(
            Problem::construct(
              *span,
              "generic",
              "unknown-wire",
              format!("wire `{wire}` is not declared"),
            )
            .did_you_mean(closest(wire, wires.clone(), 3)),
          );
        }
      }
    }
    if let Some(run) = &out.run
      && !out.meshes.iter().any(|m| m.name == run.mesh)
    {
      self.problems.push(Problem::construct(
        run.span,
        "generic",
        "unknown-mesh",
        format!(
          "mesh `{}` is not declared (`@mesh({})`)",
          run.mesh, run.mesh
        ),
      ));
    }
  }

  /// Lowers statements into a flow; `prefix` is the occurrence path of the
  /// flow below its wire.
  fn statements<'s>(
    &mut self,
    stmts: impl IntoIterator<Item = &'s Statement>,
    prefix: &[PathStep],
  ) -> Vec<ShardDef> {
    let mut out = Vec::new();
    self.statements_into(&mut out, stmts, prefix);
    out
  }

  /// Appends lowered statements to a flow.
  fn statements_into<'s>(
    &mut self,
    out: &mut Vec<ShardDef>,
    stmts: impl IntoIterator<Item = &'s Statement>,
    prefix: &[PathStep],
  ) {
    for stmt in stmts {
      match stmt {
        Statement::Assign {
          op: AssignOp::Bind,
          op_span,
          var,
        } => {
          let span = op_span.to(var.span);
          self.emit(
            out,
            prefix,
            span,
            ShardDef::with_args(&BIND, vec![Arg::pos(ParamValue::Var(var.node.clone()))]),
          );
        }
        Statement::Pipeline(pipe) => {
          for block in &pipe.blocks {
            self.block(out, prefix, block);
          }
        }
      }
    }
  }

  fn emit(&mut self, out: &mut Vec<ShardDef>, prefix: &[PathStep], span: Span, def: ShardDef) {
    let mut path = prefix.to_vec();
    path.push(PathStep::Shard {
      index: out.len(),
      name: def.name().to_string(),
    });
    self.map.insert(&self.wire, path, span);
    out.push(def);
  }

  fn unsupported(&mut self, span: Span, message: impl Into<String>) {
    self.problem(Problem::construct(
      span,
      "generic",
      "unsupported",
      message.into(),
    ));
  }

  fn block(&mut self, out: &mut Vec<ShardDef>, prefix: &[PathStep], block: &Block) {
    let span = block.span;
    match &block.kind {
      BlockKind::Shard { name, params } => self.shard(out, prefix, block, name, params.as_ref()),
      BlockKind::Var { name, path } if path.is_empty() => {
        self.emit(
          out,
          prefix,
          span,
          ShardDef::with_args(&GET, vec![Arg::pos(ParamValue::Var(name.node.clone()))]),
        );
      }
      BlockKind::Var { name, path } => {
        self.emit(
          out,
          prefix,
          name.span,
          ShardDef::with_args(&GET, vec![Arg::pos(ParamValue::Var(name.node.clone()))]),
        );
        self.take_path(out, prefix, path);
      }
      // `.a.b`: the same Takes on the step's input.
      BlockKind::Path(path) => self.take_path(out, prefix, path),
      // `(a | b)` in a flow runs in place.
      BlockKind::Expr(stmts) => self.statements_into(out, stmts.iter(), prefix),
      BlockKind::Func { name, params: None } => {
        let at = Self::const_at(prefix, out.len());
        if let Some(v) = self.read_name(name, &at) {
          self.emit(
            out,
            prefix,
            span,
            ShardDef::with_args(&CONST, vec![Arg::pos(v)]),
          );
        }
      }
      BlockKind::Func {
        name,
        params: Some(params),
      } if name.node == "read" => {
        if let Some(v) = self.read_file(block, params) {
          self.emit(
            out,
            prefix,
            span,
            ShardDef::with_args(&CONST, vec![Arg::pos(ParamValue::Value(v))]),
          );
        }
      }
      BlockKind::Func { name, .. } if name.node == "include" => {
        self.unsupported(
          span,
          "`@include` goes at the top level of a file, outside every wire and function",
        );
      }
      BlockKind::Func {
        name,
        params: Some(_),
      } if self.functions.contains_key(&name.node) => {
        // One way to evaluate at compose time, one way to expand: they
        // never alias (docs/metaprogramming.md §2.1).
        self.problem(
          Problem::construct(
            span,
            "generic",
            "not-a-macro",
            format!(
              "`{}` is a function, not a macro: `@` expands macros",
              name.node
            ),
          )
          .help(format!(
            "call it as `{0}(...)`, or evaluate it at compose time with `#( {0}(...) )`",
            name.node
          )),
        );
      }
      BlockKind::Func {
        name,
        params: Some(_),
      } => {
        let what = if matches!(name.node.as_str(), "wire" | "mesh" | "schedule" | "run") {
          format!(
            "`@{}` is a declaration; it is only allowed at the top level",
            name.node
          )
        } else {
          format!(
            "`@{}(...)` (templates and builtins) is not supported yet",
            name.node
          )
        };
        self.unsupported(span, what);
      }
      BlockKind::Flow(_) => self.unsupported(
        span,
        "a flow in braces is a parameter value (`When({...} {...})`); on its own it does nothing",
      ),
      BlockKind::Eval(stmts) => {
        // Evaluated at compose time; the step is the resulting literal.
        let at = Self::const_at(prefix, out.len());
        let flow = self.eval_flow(stmts, &at);
        self.emit(
          out,
          prefix,
          span,
          ShardDef::with_args(&CONST, vec![Arg::pos(ParamValue::Eval(flow))]),
        );
      }
      BlockKind::Enum(t, v) => {
        self.unsupported(span, format!("enums (`{t}::{v}`) are not supported yet"))
      }
      BlockKind::FString(parts) => {
        // f"a {x} b" is Seq.Make("a " x " b") | String.Format.
        let snapshot = parts.iter().any(|p| match p {
          FPart::Expr(stmts, _) => match &stmts[..] {
            [Statement::Pipeline(pipe)] => !is_plain(pipe),
            _ => true,
          },
          FPart::Text(_) => false,
        });
        let mut items = Vec::new();
        for part in parts {
          match part {
            FPart::Text(t) => items.push(Arg::pos(ParamValue::Value(Var::string(t)))),
            FPart::Expr(stmts, part_span) => {
              match self.operand_of_statements(out, prefix, stmts, *part_span, snapshot) {
                Some(v) => items.push(Arg::pos(v)),
                None => return,
              }
            }
          }
        }
        self.emit(out, prefix, span, ShardDef::with_args(&SEQ_MAKE, items));
        self.emit(
          out,
          prefix,
          span,
          ShardDef::with_args(&STRING_FORMAT, vec![]),
        );
      }
      BlockKind::Seq(items) if !is_constant(block) => {
        let snapshot = items.iter().any(|i| !is_plain(i));
        let mut args = Vec::new();
        for item in items {
          match self.operand(out, prefix, item, snapshot) {
            Some(v) => args.push(Arg::pos(v)),
            None => return,
          }
        }
        self.emit(out, prefix, span, ShardDef::with_args(&SEQ_MAKE, args));
      }
      BlockKind::Table(entries) if !is_constant(block) => {
        let snapshot = entries.iter().any(|(_, v)| !is_plain(v));
        let mut keys = Vec::new();
        let mut args = Vec::new();
        for (key, value) in entries {
          let Some(k) = self.table_key(key, &keys) else {
            return;
          };
          keys.push(k);
          match self.operand(out, prefix, value, snapshot) {
            Some(v) => args.push(Arg::pos(v)),
            None => return,
          }
        }
        let keys = Var::from_seq(Arc::new(keys.iter().map(|k| Var::string(k)).collect()));
        args.insert(0, Arg::pos(ParamValue::Value(keys)));
        self.emit(out, prefix, span, ShardDef::with_args(&TABLE_MAKE, args));
      }
      _ => {
        let at = Self::const_at(prefix, out.len());
        if let Some(v) = self.constant(block, &at) {
          self.emit(
            out,
            prefix,
            span,
            ShardDef::with_args(&CONST, vec![Arg::pos(v)]),
          );
        }
      }
    }
  }

  /// The occurrence path of the value of a `Const` emitted at `index`.
  fn const_at(prefix: &[PathStep], index: usize) -> Vec<PathStep> {
    let mut at = prefix.to_vec();
    at.push(PathStep::Shard {
      index,
      name: CONST.name().to_string(),
    });
    at.push(PathStep::Param("value".into()));
    at
  }

  /// A table key as a string; reports non-string and repeated keys.
  fn table_key(&mut self, key: &Spanned<TableKey>, seen: &[String]) -> Option<String> {
    let text = match &key.node {
      TableKey::Name(s) | TableKey::String(s) => s.clone(),
      TableKey::Int(i) => {
        self.problem(
          Problem::construct(
            key.span,
            "generic",
            "table-key",
            format!("table keys are strings in Otter: write `\"{i}\"`"),
          )
          .fix(format!("\"{i}\"")),
        );
        return None;
      }
      TableKey::Value(_) => {
        self.unsupported(
          key.span,
          "table keys are strings in Otter (sequence and value keys are not supported)",
        );
        return None;
      }
    };
    if seen.contains(&text) {
      self.problem(Problem::construct(
        key.span,
        "generic",
        "duplicate-key",
        format!("table key `{text}` appears twice"),
      ));
      return None;
    }
    Some(text)
  }

  /// `.key` / `.0` segments as Take shards.
  fn take_path(&mut self, out: &mut Vec<ShardDef>, prefix: &[PathStep], path: &[Spanned<PathKey>]) {
    for segment in path {
      let key = match &segment.node {
        PathKey::Key(k) => Var::string(k),
        PathKey::Index(i) => Var::Int(*i),
      };
      self.emit(
        out,
        prefix,
        segment.span,
        ShardDef::with_args(&TAKE, vec![Arg::pos(ParamValue::Value(key))]),
      );
    }
  }

  /// A fresh temporary's name. `%` cannot start a name in source, so it
  /// never collides with a user variable.
  fn temp(&mut self) -> String {
    self.temps += 1;
    format!("%{}", self.temps)
  }

  /// Emits `SubFlow({ <computation> = %n })` and returns `%n`. The
  /// computation runs on every activation right before the shard that uses
  /// the value, and the input flows past it unchanged.
  fn hoist(
    &mut self,
    out: &mut Vec<ShardDef>,
    prefix: &[PathStep],
    span: Span,
    lower: impl FnOnce(&mut Self, &mut Vec<ShardDef>, &[PathStep]),
  ) -> String {
    let name = self.temp();
    let mut inner_prefix = prefix.to_vec();
    inner_prefix.push(PathStep::Shard {
      index: out.len(),
      name: SUB.name().to_string(),
    });
    inner_prefix.push(PathStep::Param("action".into()));
    let mut body = Vec::new();
    lower(self, &mut body, &inner_prefix);
    // A `%` binding declares a fresh slot at each occurrence, so a wire inlined
    // twice by Do (even with different input types) still composes.
    self.emit(
      &mut body,
      &inner_prefix,
      span,
      ShardDef::with_args(&BIND, vec![Arg::pos(ParamValue::Var(name.clone()))]),
    );
    self.emit(
      out,
      prefix,
      span,
      ShardDef::with_args(&SUB, vec![Arg::pos(ParamValue::Flow(body))]),
    );
    name
  }

  /// A literal-or-variable operand for one element: a literal, a plain
  /// variable, or a computed value hoisted into a temporary. With `snapshot`,
  /// a plain variable is hoisted too, so it is read in source order relative
  /// to computed elements (`[n Inc(n)]` reads n before incrementing).
  fn operand(
    &mut self,
    out: &mut Vec<ShardDef>,
    prefix: &[PathStep],
    pipe: &Pipe,
    snapshot: bool,
  ) -> Option<ParamValue> {
    if let [b] = &pipe.blocks[..] {
      match &b.kind {
        BlockKind::Var { name, path } if path.is_empty() && !snapshot => {
          return Some(ParamValue::Var(name.node.clone()));
        }
        // A literal holding a `#( )` is hoisted like a computed value: its
        // place among the builder's arguments is not known yet.
        _ if is_constant(b) && !self.contains_eval(b) => return self.constant(b, &[]),
        _ => {}
      }
    }
    let before = self.problems.len();
    let name = self.hoist(out, prefix, pipe.span, |l, body, p| {
      for block in &pipe.blocks {
        l.block(body, p, block);
      }
    });
    (self.problems.len() == before).then_some(ParamValue::Var(name))
  }

  /// Like [`Self::operand`], for statements (an f-string interpolation).
  fn operand_of_statements(
    &mut self,
    out: &mut Vec<ShardDef>,
    prefix: &[PathStep],
    stmts: &[Statement],
    span: Span,
    snapshot: bool,
  ) -> Option<ParamValue> {
    if let [Statement::Pipeline(pipe)] = stmts {
      return self.operand(out, prefix, pipe, snapshot);
    }
    let before = self.problems.len();
    let name = self.hoist(out, prefix, span, |l, body, p| {
      l.statements_into(body, stmts.iter(), p)
    });
    (self.problems.len() == before).then_some(ParamValue::Var(name))
  }

  /// `@name`: a script argument (a string), or a constant. `at` is where
  /// the value goes (see [`Self::constant`]).
  fn read_name(&mut self, name: &Name, at: &[PathStep]) -> Option<ParamValue> {
    if let Some(v) = self.defines.get(&name.node)
      && !self.typed_defines.contains(&name.node)
    {
      return Some(ParamValue::Value(Var::string(v)));
    }
    if self.consts.contains_key(&name.node) {
      if let Some(first) = self.const_stack.iter().position(|c| *c == name.node) {
        let mut chain = self.const_stack[first..].to_vec();
        chain.push(name.node.clone());
        self.problem(Problem::construct(
          name.span,
          "generic",
          "compose-time-cycle",
          format!(
            "`@{}` is read while its own value is computed ({})",
            name.node,
            chain.join(" -> ")
          ),
        ));
        return None;
      }
      self.const_reads += 1;
      if let Some(value) = self.const_values.get(&name.node) {
        return value.clone().map(ParamValue::Value);
      }
      let block = self.consts[&name.node].clone();
      if !self.contains_eval(&block) {
        return self.read_value_const(&name.node, &block, at);
      }
      if self.const_overflow.is_some() {
        return None;
      }
      if self.const_expansion > EVAL_CONST_NODES {
        self.const_overflow = Some(
          Problem::construct(
            name.span,
            "generic",
            "expansion-budget",
            format!(
              "constants holding `#( )` lower past {EVAL_CONST_NODES} steps here: each read of `@{}` lowers its pipelines again",
              name.node
            ),
          )
          .help("constants built from each other multiply at every level; read those holding `#( )` in fewer places"),
        );
        return None;
      }
      // Charged with the nodes this read added itself: a nested read
      // charges its own as it completes, so a runaway chain stops within
      // one constant's own lowering of the bound.
      let (nodes, charged) = (self.map.nodes.len(), self.const_expansion);
      let value = self.read_const(&name.node, at);
      let added = self.map.nodes.len() - nodes;
      self.const_expansion += added.saturating_sub(self.const_expansion - charged);
      return value;
    }
    let mut known: Vec<String> = self.defines.keys().cloned().collect();
    known.extend(self.consts.keys().cloned());
    self.problem(
      Problem::construct(
        name.span,
        "generic",
        "unknown-define",
        format!(
          "no value for `@{0}`: pass it as `{0}:value`, or declare it with `@const({0} value)`",
          name.node
        ),
      )
      .did_you_mean(closest(&name.node, known, 3)),
    );
    None
  }

  /// A constant without a `#( )`: lowered at its first read and shared by
  /// every later one. Built from other constants, its size as text must be
  /// within [`const_limit`] (measured with an early exit, so a value that
  /// shares one constant many times costs at most the limit to measure).
  fn read_value_const(&mut self, name: &str, block: &Block, at: &[PathStep]) -> Option<ParamValue> {
    let reads = self.const_reads;
    let value = match self.read_const(name, at) {
      Some(ParamValue::Value(v)) => Some(v),
      _ => None,
    };
    let value = value.filter(|v| {
      let fits = self.const_reads == reads
        || otter_core::compose_time::text_size(v, const_limit()).is_some();
      if !fits {
        self.problem(
          Problem::construct(
            block.span,
            "generic",
            "expansion-budget",
            format!(
              "`@{name}` expands past {} bytes as text: constants built from each other multiply at every level",
              const_limit()
            ),
          )
          .help("build large values from fewer copies, or compute them with `#( )`"),
        );
      }
      fits
    });
    self.const_values.insert(name.to_string(), value.clone());
    value.map(ParamValue::Value)
  }

  /// A constant's value, lowered where it is read. Its problems are
  /// reported the first time it is lowered (every constant is lowered once
  /// before any body), not at every read.
  fn read_const(&mut self, name: &str, at: &[PathStep]) -> Option<ParamValue> {
    let block = self.consts.get(name)?.clone();
    self.const_stack.push(name.to_string());
    let before = self.problems.len();
    let value = self.constant(&block, at);
    self.const_stack.pop();
    if !self.consts_checked.insert(name.to_string()) {
      self.problems.truncate(before);
    }
    value
  }

  /// `@const(name value)`: a program-level constant read as `@name`
  /// (docs/metaprogramming.md §2.1); `value` is a literal or a `#( )`. Its
  /// name shares the namespace of the script arguments: a script argument
  /// of the same name is an error, unless `ignore-redefined: true` makes
  /// the constant a default the argument replaces (1.x's `@define(...
  /// IgnoreRedefined: true)`).
  fn const_decl(&mut self, block: &Block, params: Option<&Params>) {
    let usage = "`@const(name value)`, where value is a literal or `#( ... )`, optionally `ignore-redefined: true`";
    let mut items = params.map(|p| &p.items[..]).unwrap_or(&[]);
    let mut default = false;
    if let [rest @ .., last] = items
      && last
        .name
        .as_ref()
        .is_some_and(|n| n.node == "ignore-redefined")
    {
      match &last.value.blocks[..] {
        [b] if matches!(b.kind, BlockKind::Literal(Literal::Bool(_))) => {
          default = b.kind == BlockKind::Literal(Literal::Bool(true));
          items = rest;
        }
        _ => {
          self.problem(Problem::construct(
            last.value.span,
            "generic",
            "declaration",
            "`ignore-redefined:` takes `true` or `false`".into(),
          ));
          return;
        }
      }
    }
    let name = match items {
      [n, _] if n.name.is_none() => match &n.value.blocks[..] {
        [b] => match &b.kind {
          BlockKind::Var { name, path } if path.is_empty() => Some(name.clone()),
          _ => None,
        },
        _ => None,
      },
      _ => None,
    };
    let value = match items {
      [_, v] if v.name.is_none() => match &v.value.blocks[..] {
        [b] if is_constant(b) => Some(b.clone()),
        _ => None,
      },
      _ => None,
    };
    let (Some(name), Some(value)) = (name, value) else {
      self.problem(Problem::construct(
        block.span,
        "generic",
        "declaration",
        format!("a constant needs a name and a value: {usage}"),
      ));
      return;
    };
    // A default: the script argument of the same name, when given, wins,
    // read as the default's type (arguments arrive as strings).
    if default {
      let BlockKind::Literal(
        kind @ (Literal::Int(_) | Literal::Float(_) | Literal::Bool(_) | Literal::String(_)),
      ) = &value.kind
      else {
        self.problem(Problem::construct(
          value.span,
          "generic",
          "declaration",
          "a default for a script argument is a number, a string or a bool literal".into(),
        ));
        return;
      };
      if let Some(given) = self.defines.get(&name.node) {
        // The argument replaces this default, not a second declaration.
        if self.consts.contains_key(&name.node) {
          self.problem(Problem::construct(
            name.span,
            "generic",
            "duplicate-binding",
            format!("`@{}` is already defined", name.node),
          ));
          return;
        }
        let parsed = match kind {
          Literal::Int(_) => given.trim().parse().ok().map(Literal::Int),
          Literal::Float(_) => given.trim().parse().ok().map(Literal::Float),
          Literal::Bool(_) => match given.trim() {
            "true" => Some(Literal::Bool(true)),
            "false" => Some(Literal::Bool(false)),
            _ => None,
          },
          Literal::String(_) => Some(Literal::String(given.clone())),
          Literal::None => None,
        };
        let Some(parsed) = parsed else {
          self.problem(Problem::construct(
            name.span,
            "generic",
            "script-argument-type",
            format!(
              "script argument `{}` is `{given}`, which is not {} like its default",
              name.node,
              match kind {
                Literal::Int(_) => "an Int",
                Literal::Float(_) => "a Float",
                Literal::Bool(_) => "a Bool (`true` or `false`)",
                _ => "of a supported type",
              }
            ),
          ));
          return;
        };
        self.typed_defines.insert(name.node.clone());
        self.consts.insert(
          name.node,
          Block {
            kind: BlockKind::Literal(parsed),
            ..value
          },
        );
        return;
      }
    }
    if self.defines.contains_key(&name.node) || self.consts.contains_key(&name.node) {
      self.problem(Problem::construct(
        name.span,
        "generic",
        "duplicate-binding",
        format!(
          "`@{}` is already defined{}",
          name.node,
          if self.defines.contains_key(&name.node) {
            " as a script argument"
          } else {
            ""
          }
        ),
      ));
      return;
    }
    self.consts.insert(name.node, value);
  }

  /// The flow of a `#( ... )` whose value goes at `at`: its shards are
  /// recorded below `at` and [`PathStep::Evaluation`], the path compose
  /// diagnostics inside the evaluation carry.
  fn eval_flow(&mut self, stmts: &[Statement], at: &[PathStep]) -> Vec<ShardDef> {
    let mut prefix = at.to_vec();
    prefix.push(PathStep::Evaluation);
    self.statements(stmts.iter(), &prefix)
  }

  /// Whether a literal holds a `#( )`, directly, in an element, or through
  /// a constant.
  fn contains_eval(&self, block: &Block) -> bool {
    fn walk(l: &Lowerer<'_>, block: &Block, seen: &mut Vec<String>) -> bool {
      let single = |pipe: &Pipe, seen: &mut Vec<String>| match &pipe.blocks[..] {
        [b] => walk(l, b, seen),
        _ => false,
      };
      match &block.kind {
        BlockKind::Eval(_) => true,
        BlockKind::Func { name, params: None } => match l.consts.get(&name.node) {
          Some(value) if !seen.contains(&name.node) => {
            seen.push(name.node.clone());
            walk(l, value, seen)
          }
          // A cycle is reported where it is read.
          _ => false,
        },
        BlockKind::Seq(items) => items.iter().any(|i| single(i, seen)),
        BlockKind::Table(entries) => entries.iter().any(|(_, v)| single(v, seen)),
        _ => false,
      }
    }
    walk(self, block, &mut Vec::new())
  }

  /// `@read("path")`: the file's text, or with `bytes: true` its bytes, as
  /// a literal. The path is relative to the file naming it.
  fn read_file(&mut self, block: &Block, params: &Params) -> Option<Var> {
    let usage = "`@read(\"file.txt\")`, or `@read(\"file.bin\" bytes: true)` for its bytes";
    let mut path = None;
    let mut bytes = false;
    for param in &params.items {
      let literal = match &param.value.blocks[..] {
        [b] => match &b.kind {
          BlockKind::Literal(literal) => Some(literal),
          _ => None,
        },
        _ => None,
      };
      match (param.name.as_ref().map(|n| n.node.as_str()), literal) {
        (None, Some(Literal::String(p))) if path.is_none() => path = Some(p.clone()),
        (Some("bytes"), Some(Literal::Bool(b))) => bytes = *b,
        _ => {
          self.problem(Problem::construct(
            param.value.span,
            "generic",
            "declaration",
            format!("`@read` takes a file name and an optional `bytes:` flag: {usage}"),
          ));
          return None;
        }
      }
    }
    let Some(path) = path else {
      self.problem(Problem::construct(
        block.span,
        "generic",
        "declaration",
        format!("`@read` needs a file name: {usage}"),
      ));
      return None;
    };
    let Some((source, files)) = self.files else {
      self.unsupported(
        block.span,
        "`@read` needs a program loaded with files to read",
      );
      return None;
    };
    let from = source.file_name(block.span.start).to_string();
    let file = match files.read(&from, &path) {
      Ok(file) => file,
      Err(message) => {
        self.problem(Problem::construct(
          block.span,
          "generic",
          "file-not-found",
          format!("cannot read {path}: {message}"),
        ));
        return None;
      }
    };
    if !self.reads.contains(&file.name) {
      self.reads.push(file.name.clone());
    }
    if bytes {
      return Some(Var::bytes(&file.bytes));
    }
    match String::from_utf8(file.bytes) {
      Ok(text) => Some(Var::string(&text)),
      Err(_) => {
        self.problem(Problem::construct(
          block.span,
          "generic",
          "not-utf8",
          format!(
            "{} is not UTF-8 text; read it with `bytes: true`",
            file.name
          ),
        ));
        None
      }
    }
  }

  /// A literal value: numbers, strings, `none`, booleans, `@name`, `#( )`,
  /// and sequences and tables of them. Reports anything else. A literal
  /// holding a `#( )` is an `Eval` of the pipeline that builds it (its
  /// elements evaluated in turn); `at` is the occurrence path of the
  /// argument that holds the value, where those pipelines are recorded.
  fn constant(&mut self, block: &Block, at: &[PathStep]) -> Option<ParamValue> {
    let value = |v: Var| Some(ParamValue::Value(v));
    match &block.kind {
      BlockKind::Literal(Literal::None) => value(Var::None),
      BlockKind::Literal(Literal::Bool(b)) => value(Var::Bool(*b)),
      BlockKind::Literal(Literal::Int(i)) => value(Var::Int(*i)),
      BlockKind::Literal(Literal::Float(f)) => value(Var::Float(*f)),
      BlockKind::Literal(Literal::String(s)) => value(Var::string(s)),
      BlockKind::Func { name, params: None } => self.read_name(name, at),
      BlockKind::Func {
        name,
        params: Some(params),
      } if name.node == "read" => self.read_file(block, params).map(ParamValue::Value),
      BlockKind::EmptyBraces => value(Var::table(Vec::<(&str, Var)>::new())),
      BlockKind::Eval(stmts) => Some(ParamValue::Eval(self.eval_flow(stmts, at))),
      BlockKind::Seq(items) => {
        let builder = Self::builder_path(at, SEQ_MAKE.name(), "items");
        let mut values = Vec::with_capacity(items.len());
        for (k, item) in items.iter().enumerate() {
          let mut element = builder.clone();
          element.push(PathStep::Item(k));
          values.push(self.constant_pipe(item, &element)?);
        }
        Some(match Self::all_literal(values) {
          Ok(vars) => ParamValue::Value(Var::from_seq(Arc::new(vars))),
          Err(values) => ParamValue::Eval(vec![ShardDef::with_args(
            &SEQ_MAKE,
            values.into_iter().map(Arg::pos).collect(),
          )]),
        })
      }
      BlockKind::Table(entries) => {
        let builder = Self::builder_path(at, TABLE_MAKE.name(), "values");
        let mut keys: Vec<String> = Vec::with_capacity(entries.len());
        let mut values = Vec::with_capacity(entries.len());
        for (k, (key, item)) in entries.iter().enumerate() {
          let key = self.table_key(key, &keys)?;
          keys.push(key);
          let mut element = builder.clone();
          element.push(PathStep::Item(k));
          values.push(self.constant_pipe(item, &element)?);
        }
        Some(match Self::all_literal(values) {
          Ok(vars) => ParamValue::Value(Var::table(keys.into_iter().zip(vars))),
          Err(values) => {
            let keys = Var::from_seq(Arc::new(keys.iter().map(|k| Var::string(k)).collect()));
            let args = std::iter::once(ParamValue::Value(keys))
              .chain(values)
              .map(Arg::pos)
              .collect();
            ParamValue::Eval(vec![ShardDef::with_args(&TABLE_MAKE, args)])
          }
        })
      }
      _ => {
        self.unsupported(block.span, "expected a literal value here");
        None
      }
    }
  }

  /// Where the builder of a literal holding a `#( )` records its element
  /// `k`: below `at`, the evaluation, its one builder shard and `param`.
  fn builder_path(at: &[PathStep], builder: &str, param: &str) -> Vec<PathStep> {
    let mut path = at.to_vec();
    path.extend([
      PathStep::Evaluation,
      PathStep::Shard {
        index: 0,
        name: builder.to_string(),
      },
      PathStep::Param(param.to_string()),
    ]);
    path
  }

  /// The elements' values when all are literal, else the elements as they
  /// are (for a builder shard evaluated at compose time).
  fn all_literal(values: Vec<ParamValue>) -> Result<Vec<Var>, Vec<ParamValue>> {
    if !values.iter().all(|v| matches!(v, ParamValue::Value(_))) {
      return Err(values);
    }
    Ok(
      values
        .into_iter()
        .map(|v| match v {
          ParamValue::Value(v) => v,
          _ => unreachable!("checked"),
        })
        .collect(),
    )
  }

  fn constant_pipe(&mut self, pipe: &Pipe, at: &[PathStep]) -> Option<ParamValue> {
    match &pipe.blocks[..] {
      [b] => self.constant(b, at),
      _ => {
        self.unsupported(
          pipe.span,
          "computed elements in sequences and tables (`[a | F]`) are not supported yet; compute them into variables first, or at compose time with `#( ... )`",
        );
        None
      }
    }
  }

  /// A literal that must be known while lowering (a case value, a
  /// parameter's default): a `#( )` is not supported there.
  fn literal_now(&mut self, block: &Block) -> Option<Var> {
    if self.contains_eval(block) {
      self.unsupported(
        block.span,
        "this value must be a plain literal: `#( )`, and constants computed with it, are not supported here",
      );
      return None;
    }
    match self.constant(block, &[])? {
      ParamValue::Value(v) => Some(v),
      _ => unreachable!("no evaluation inside"),
    }
  }

  fn shard(
    &mut self,
    out: &mut Vec<ShardDef>,
    prefix: &[PathStep],
    block: &Block,
    name: &Name,
    params: Option<&Params>,
  ) {
    let (target, decls): (Target, Option<Vec<DeclView>>) = if let Some(ty) =
      self.catalog.get(&name.node)
    {
      (
        Target::Native(ty),
        match ty.desc.params {
          describe::Params::Declared(d) => Some(d.iter().map(DeclView::native).collect()),
          describe::Params::Undeclared => None,
        },
      )
    } else if let Some(shape) = self.functions.get(&name.node) {
      (
        Target::Function(name.node.clone()),
        Some(
          shape
            .params
            .iter()
            .map(|(name, flow)| DeclView::function(name, *flow))
            .collect(),
        ),
      )
    } else {
      // Suggest aliases too, so `Ad` suggests `Add` as written in scripts.
      let all: Vec<String> = self.catalog.names().iter().map(|n| n.to_string()).collect();
      let suggestions = closest(&name.node, all, 3);
      if let Some(help) = match name.node.as_str() {
        "Set" => Some(
          "declare a mutable variable with `value | Var(name)`, assign it with `value | Update(name)`",
        ),
        "Ref" => Some("bind an immutable name with `value = name`"),
        "Do" => Some(
          "declare a function with `@fn(Name input: T output: T params: {} { ... })` and call it by name",
        ),
        _ => None,
      } {
        self.problem(
          Problem::construct(
            name.span,
            "unknown-shard",
            "removed-shard",
            format!("`{}` is removed in Otter: {help}", name.node),
          )
          .shard(&name.node),
        );
        return;
      }
      if matches!(name.node.as_str(), "And" | "Or") {
        let (word, all) = if name.node == "And" {
          ("And", "All")
        } else {
          ("Or", "Any")
        };
        self.problem(
          Problem::construct(
            name.span,
            "unknown-shard",
            "and-or",
            format!("`{word}` is not a shard in Otter: combine conditions with `{all}(...)`"),
          )
          .help(format!("`If({{a | {word} | b}} ...)` becomes `If({all}(a b) ...)`; conditions can be Bool variables or flows"))
          .did_you_mean(vec![all.to_string()])
          .shard(&name.node),
        );
        return;
      }
      let message = if name.node.starts_with(|c: char| c.is_ascii_lowercase()) {
        format!(
          "`{}` is not a shard: shard names start with an uppercase letter",
          name.node
        )
      } else {
        format!("unknown shard `{}`", name.node)
      };
      let mut suggestions = suggestions;
      suggestions.extend(closest(
        &name.node,
        self.functions.keys().cloned().collect::<Vec<_>>(),
        3,
      ));
      self.problem(
        Problem::construct(name.span, "unknown-shard", "unknown-shard", message)
          .did_you_mean(suggestions)
          .shard(&name.node),
      );
      return;
    };
    let owner = target.name().to_string();
    let decls = decls.as_deref();
    let mut resolved: Vec<(&Param, Option<&DeclView>)> = Vec::new();
    let mut variadic_items: Vec<usize> = Vec::new();
    let mut ok = true;
    let mut position = 0;
    for param in params.map(|p| &p.items[..]).unwrap_or(&[]) {
      let decl = match (&param.name, decls) {
        (Some(pname), Some(decls)) => match decls.iter().find(|d| d.name == pname.node) {
          Some(d) => Some(d),
          None => {
            let known: Vec<String> = decls.iter().map(|d| d.name.clone()).collect();
            self.problem(
              Problem::construct(
                pname.span,
                "generic",
                "unknown-argument",
                format!(
                  "{owner} has no parameter `{}` (parameters: {})",
                  pname.node,
                  known.join(", ")
                ),
              )
              .did_you_mean(closest(&pname.node, known.clone(), 3))
              .shard(&owner)
              .param(&pname.node),
            );
            ok = false;
            continue;
          }
        },
        (None, Some(decls)) => {
          // From its position on, a variadic parameter takes the rest;
          // the parameters after it are given by name.
          let d = match decls.iter().position(|d| d.variadic) {
            Some(v) if position >= v => decls.get(v),
            _ => decls.get(position),
          };
          position += 1;
          d
        }
        (_, None) => None,
      };
      resolved.push((param, decl));
      // Variadic arguments are numbered for occurrence paths (`Item`), in
      // ascending order.
      if decl.is_some_and(|d| d.variadic) {
        variadic_items.push(resolved.len() - 1);
      }
    }
    // Computed values first: they are hoisted into Subs placed before the
    // shard, which moves the shard's index (nested flows depend on it).
    let mut values: Vec<Option<ParamValue>> = Vec::with_capacity(resolved.len());
    // When something is computed, plain variables read as values are
    // snapshotted too, in order, so they are read before later computations.
    let snapshot = resolved.iter().any(|(p, d)| !is_direct(&p.value, *d));
    for (param, decl) in &resolved {
      let read_operand =
        decl.is_some_and(|d| d.forms.contains(Forms::LITERAL) && d.forms.contains(Forms::VARIABLE));
      if snapshot && read_operand && is_plain_var(&param.value) {
        let v = self.operand(out, prefix, &param.value, true);
        ok &= v.is_some();
        values.push(v);
        continue;
      }
      // A parameter taking only a variable names one (`Var(x)`, `Update(x)`,
      // `Keep(x 0)`): a computed value or a path cannot be a name.
      if let Some(d) = decl.filter(|d| names_variable(d))
        && !is_plain_var(&param.value)
      {
        self.problem(
          Problem::construct(
            param.value.span,
            "generic",
            "expected-variable-name",
            format!(
              "{owner}.{} takes a variable name such as `x`, not a computed value or a path",
              d.name
            ),
          )
          .shard(&owner)
          .param(&d.name),
        );
        ok = false;
        values.push(None);
        continue;
      }
      if is_direct(&param.value, *decl) {
        values.push(None);
        continue;
      }
      if decl.is_some_and(|d| !d.forms.contains(Forms::VARIABLE)) {
        let d = decl.expect("checked");
        self.problem(
          Problem::construct(
            param.value.span,
            "generic",
            "computed-literal",
            format!(
              "{owner}.{} takes a literal; a computed value cannot be passed here",
              d.name
            ),
          )
          .shard(&owner)
          .param(&d.name),
        );
        ok = false;
        values.push(None);
        continue;
      }
      let v = self.operand(out, prefix, &param.value, false);
      ok &= v.is_some();
      values.push(v);
    }
    let index = out.len();
    let mut args = Vec::new();
    for (n, ((param, decl), hoisted)) in resolved.iter().zip(values).enumerate() {
      let value = match hoisted {
        Some(v) => Some(v),
        None if !is_direct(&param.value, *decl) => None,
        None => {
          let mut nested = prefix.to_vec();
          nested.push(PathStep::Shard {
            index,
            name: owner.clone(),
          });
          if let Some(d) = decl {
            nested.push(PathStep::Param(d.name.clone()));
            if let Ok(item) = variadic_items.binary_search(&n) {
              nested.push(PathStep::Item(item));
            }
          }
          self.param_value(&param.value, *decl, &nested)
        }
      };
      match value {
        Some(value) => args.push(match &param.name {
          Some(n) => Arg::named(&n.node, value),
          None => Arg::pos(value),
        }),
        None => ok = false,
      }
    }
    if ok {
      let def = match target {
        Target::Native(ty) => ShardDef::with_args(ty, args),
        Target::Function(name) => ShardDef::call(&name, args),
      };
      self.emit(out, prefix, block.span, def);
    }
  }

  /// One argument, in the form its declaration accepts. Where a flow is
  /// accepted, anything that is not otherwise a valid argument becomes a
  /// flow: `If(All(a b) ...)` is `If({All(a b)} ...)`, `When(far ...)` is
  /// `When({far} ...)`, re-evaluated each time the shard runs it.
  fn param_value(
    &mut self,
    pipe: &Pipe,
    decl: Option<&DeclView>,
    nested: &[PathStep],
  ) -> Option<ParamValue> {
    let accepts = |f: Forms| decl.is_none_or(|d| d.forms.contains(f));
    let flow_accepted = decl.is_some_and(|d| d.forms.contains(Forms::FLOW));
    let as_flow = |l: &mut Self| {
      let mut flow = Vec::new();
      for block in &pipe.blocks {
        l.block(&mut flow, nested, block);
      }
      Some(ParamValue::Flow(flow))
    };
    let [b] = &pipe.blocks[..] else {
      return if flow_accepted {
        as_flow(self)
      } else {
        unreachable!("computed values are hoisted before param_value")
      };
    };
    match &b.kind {
      BlockKind::Seq(items) if decl.is_some_and(|d| d.forms.contains(Forms::CASES)) => {
        self.cases(items, b.span, nested)
      }
      BlockKind::Flow(stmts) => Some(ParamValue::Flow(self.statements(stmts.iter(), nested))),
      BlockKind::EmptyBraces if flow_accepted => Some(ParamValue::Flow(Vec::new())),
      // A shard's flow argument naming a flow parameter runs its block.
      BlockKind::Var { name, path }
        if path.is_empty()
          && flow_accepted
          && !accepts(Forms::VARIABLE)
          && self.flow_params.contains(&name.node) =>
      {
        let mut flow = Vec::new();
        let run = ShardDef::with_args(&RUN, vec![Arg::pos(ParamValue::Var(name.node.clone()))]);
        self.emit(&mut flow, nested, b.span, run);
        Some(ParamValue::Flow(flow))
      }
      BlockKind::Var { name, path } if path.is_empty() => {
        if accepts(Forms::WIRE) && !accepts(Forms::VARIABLE) {
          Some(ParamValue::Wire(name.node.clone()))
        } else if accepts(Forms::VARIABLE) || !flow_accepted {
          Some(ParamValue::Var(name.node.clone()))
        } else {
          as_flow(self)
        }
      }
      _ if is_constant(b) && (accepts(Forms::LITERAL) || !flow_accepted) => {
        self.constant(b, nested)
      }
      _ if flow_accepted => as_flow(self),
      _ => self.constant(b, nested),
    }
  }
}

impl Lowerer<'_> {
  /// `[value {flow} value {flow}]` as cases; `none` matches anything.
  fn cases(&mut self, items: &[Pipe], span: Span, nested: &[PathStep]) -> Option<ParamValue> {
    if !items.len().is_multiple_of(2) {
      self.problem(Problem::construct(
        span,
        "generic",
        "cases",
        "cases come in pairs: `[value {flow} value {flow}]`".into(),
      ));
      return None;
    }
    let mut cases = Vec::with_capacity(items.len() / 2);
    for (k, pair) in items.chunks(2).enumerate() {
      let value = match &pair[0].blocks[..] {
        [b] if is_constant(b) => self.literal_now(b)?,
        _ => {
          self.problem(Problem::construct(
            pair[0].span,
            "generic",
            "cases",
            "a case's value must be a literal (`none` matches anything)".into(),
          ));
          return None;
        }
      };
      let mut path = nested.to_vec();
      path.push(PathStep::Item(k));
      let flow = match &pair[1].blocks[..] {
        [b] => match &b.kind {
          BlockKind::Flow(stmts) => self.statements(stmts.iter(), &path),
          BlockKind::EmptyBraces => Vec::new(),
          _ => {
            self.problem(Problem::construct(
              pair[1].span,
              "generic",
              "cases",
              "each case value is followed by a flow in braces".into(),
            ));
            return None;
          }
        },
        _ => {
          self.problem(Problem::construct(
            pair[1].span,
            "generic",
            "cases",
            "each case value is followed by a flow in braces".into(),
          ));
          return None;
        }
      };
      cases.push((value, flow));
    }
    Some(ParamValue::Cases(cases))
  }
}

/// Positional and named parameters, in order.
fn split_params(params: &Params) -> (Vec<&Pipe>, Vec<(&Name, &Pipe)>) {
  let mut positional = Vec::new();
  let mut named = Vec::new();
  for p in &params.items {
    match &p.name {
      Some(n) => named.push((n, &p.value)),
      None => positional.push(&p.value),
    }
  }
  (positional, named)
}

/// Whether a block is a literal value: a literal, `@name`, `{}`, `#( )`
/// (evaluated at compose time), or a sequence or table of them.
fn is_constant(block: &Block) -> bool {
  let single = |pipe: &Pipe| matches!(&pipe.blocks[..], [b] if is_constant(b));
  match &block.kind {
    BlockKind::Literal(_)
    | BlockKind::EmptyBraces
    | BlockKind::Func { params: None, .. }
    | BlockKind::Eval(_) => true,
    BlockKind::Func { name, .. } => name.node == "read",
    BlockKind::Seq(items) => items.iter().all(single),
    BlockKind::Table(entries) => entries.iter().all(|(_, v)| single(v)),
    _ => false,
  }
}

/// Whether a parameter value is passed as it is (a flow, `{}`, a plain
/// variable or wire name, or a literal) rather than computed first.
fn is_direct(pipe: &Pipe, decl: Option<&DeclView>) -> bool {
  // A flow parameter takes anything, as a flow (see `param_value`).
  if decl.is_some_and(|d| d.forms.contains(Forms::FLOW)) {
    return true;
  }
  match &pipe.blocks[..] {
    [b] => match &b.kind {
      BlockKind::Flow(_) | BlockKind::EmptyBraces => true,
      BlockKind::Seq(_) if decl.is_some_and(|d| d.forms.contains(Forms::CASES)) => true,
      BlockKind::Var { path, .. } => path.is_empty(),
      _ => is_constant(b),
    },
    _ => false,
  }
}

/// A parameter that takes only a variable: its value is a name, never a
/// computed value.
fn names_variable(d: &DeclView) -> bool {
  d.forms.contains(Forms::VARIABLE)
    && !d.forms.contains(Forms::LITERAL)
    && !d.forms.contains(Forms::FLOW)
}

/// A plain variable read (no path).
fn is_plain_var(pipe: &Pipe) -> bool {
  matches!(&pipe.blocks[..], [b] if matches!(&b.kind, BlockKind::Var { path, .. } if path.is_empty()))
}

/// A literal or a plain variable: needs no computation.
fn is_plain(pipe: &Pipe) -> bool {
  is_plain_var(pipe) || matches!(&pipe.blocks[..], [b] if is_constant(b))
}

#[cfg(test)]
mod source_map_tests {
  use super::*;
  use otter_core::diagnostic::Phase;

  #[test]
  fn source_paths_share_prefixes_and_keep_nearest_locations() {
    let mut map = SourceMap::default();
    let parent = PathStep::Shard {
      index: 0,
      name: "When".into(),
    };
    let branch = PathStep::Param("then".into());
    let leaf = |index| PathStep::Shard {
      index,
      name: "Const".into(),
    };
    map.wires.insert("w".into(), Span::new(0, 100));
    map.insert("w", vec![parent.clone()], Span::new(1, 90));
    // Insert siblings out of order to exercise sorted lookup and shared ancestry.
    for i in [2, 0, 1] {
      map.insert(
        "w",
        vec![parent.clone(), branch.clone(), leaf(i)],
        Span::new(10 + i, 11 + i),
      );
    }
    assert_eq!(map.nodes.len(), 6); // root, parent, branch, three leaves
    let mut d = Diagnostic::new(Phase::Compose, "", "", "");
    for i in 0..3 {
      d.path = vec![
        PathStep::Wire("w".into()),
        parent.clone(),
        branch.clone(),
        leaf(i),
      ];
      assert_eq!(map.locate(&d), Some(Span::new(10 + i, 11 + i)));
      d.path.push(PathStep::Item(17));
      assert_eq!(map.locate(&d), Some(Span::new(10 + i, 11 + i)));
    }
    d.path = vec![PathStep::Wire("w".into()), parent.clone(), branch, leaf(9)];
    assert_eq!(map.locate(&d), Some(Span::new(1, 90)));
    // A nested wire starts a new definition; never fall back into its caller.
    map.wires.insert("other".into(), Span::new(200, 220));
    d.path.push(PathStep::Wire("other".into()));
    d.path.push(parent.clone());
    assert_eq!(map.locate(&d), Some(Span::new(200, 220)));
    d.path.push(PathStep::Wire("missing".into()));
    assert_eq!(map.locate(&d), None);
    d.path = vec![parent];
    assert_eq!(map.locate(&d), None);
  }
}
