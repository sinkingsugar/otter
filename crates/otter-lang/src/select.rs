//! `@if(condition {then} {else})`: compose-time selection
//! (docs/metaprogramming.md §2.6).
//!
//! The frontend decides each `@if` while it loads, before lowering: the
//! condition is a compose-time Bool (a literal, `@name` or `#( )`), and the
//! taken branch replaces the `@if` as if it had been written there. The
//! other branch was parsed (its syntax errors are reported with the
//! file's) and is dropped: it is never lowered, scope-checked or composed,
//! so shards, parameters and names a host lacks may appear in it.
//!
//! At the top level of a file, an `@if` written alone as a statement
//! splices declarations (`@fn`, `@const`, `@wire`, `@include`, ...) or
//! loose code, and is decided in order, while files are included: its
//! condition sees the constants declared above it (its branches may
//! declare constants themselves). Everywhere else (a step in a wire,
//! function or block, a parameter, an element) it is decided after the
//! whole top level is known, and sees every constant.

use std::collections::HashMap;

use otter_core::{Catalog, Var};

use crate::ast::*;
use crate::lower::{Conditional, condition_value, top_level_func, upper_name_of};
use crate::problem::Problem;
use crate::source::Span;

/// What `@read` in a condition reads from (see `lower_with`).
pub(crate) type FilesRef<'s> = Option<(&'s crate::Source, &'s dyn crate::Files)>;

pub(crate) struct Selector<'a> {
  catalog: &'a Catalog,
  defines: &'a HashMap<String, String>,
  /// The top-level `@const` declarations a condition may read, each
  /// selected (its own `@if`s decided) when a condition first needs it.
  consts: Vec<Const>,
  /// The constants whose `@if`s are being decided, innermost last
  /// (`compose-time-cycle`).
  selecting: Vec<String>,
  /// Whether decisions are recorded in `decided`: not while a constant is
  /// selected for a condition, since the program's own copy of it is
  /// decided (and recorded) where it stands.
  recording: bool,
  /// The declared functions' names: a condition cannot call one.
  functions: Vec<String>,
  /// Every `@if` decided, in the order decided.
  pub(crate) decided: Vec<Conditional>,
  /// The files `@read` read while deciding.
  pub(crate) reads: Vec<String>,
}

/// A constant a condition may read.
struct Const {
  name: String,
  /// Its declaration as written.
  written: Statement,
  /// Whether its value holds an `@if`: it is read selected, then.
  selects: bool,
  state: ConstState,
}

enum ConstState {
  Written,
  Selecting,
  Selected(Statement),
  /// Deciding one of its `@if`s failed (reported then).
  Failed,
}

impl Const {
  /// The declaration a condition reads: `None` while it holds an `@if`
  /// not decided yet.
  fn readable(&self) -> Option<&Statement> {
    match &self.state {
      ConstState::Selected(stmt) => Some(stmt),
      _ if !self.selects => Some(&self.written),
      _ => None,
    }
  }

  /// Its value, where the names it reads are.
  fn value(stmt: &Statement) -> Option<&Pipe> {
    let (_, params, _) = top_level_func(stmt)?;
    params?.items.get(1).map(|p| &p.value)
  }
}

/// The top-level statements before a point, in pieces that stay where
/// they are (no copy while nothing needs them): `statements` follow those
/// of `parent`.
pub(crate) struct Before<'p> {
  pub(crate) statements: &'p [Statement],
  pub(crate) parent: Option<&'p Before<'p>>,
}

/// The usage every shape problem repeats.
const USAGE: &str = "`@if(condition {then})` or `@if(condition {then} {else})`, where the condition is `true`, `false`, `@name` or `#( ... )`";

impl<'a> Selector<'a> {
  pub(crate) fn new(catalog: &'a Catalog, defines: &'a HashMap<String, String>) -> Selector<'a> {
    Selector {
      catalog,
      defines,
      consts: Vec::new(),
      selecting: Vec::new(),
      recording: true,
      functions: Vec::new(),
      decided: Vec::new(),
      reads: Vec::new(),
    }
  }

  /// Sets what the next condition may depend on: the declarations
  /// before it.
  pub(crate) fn scope(&mut self, before: &Before<'_>) {
    fn walk(s: &mut Selector<'_>, b: &Before<'_>) {
      if let Some(parent) = b.parent {
        walk(s, parent);
      }
      for stmt in b.statements {
        s.declare(stmt);
      }
    }
    self.consts.clear();
    self.functions.clear();
    walk(self, before);
  }

  /// Records a top-level statement a later condition may depend on: a
  /// constant it reads, or a function it must not call.
  fn declare(&mut self, stmt: &Statement) {
    let Some((name, params, _)) = top_level_func(stmt) else {
      return;
    };
    match name.node.as_str() {
      "const" => {
        let Some(name) =
          params
            .and_then(|p| p.items.first())
            .and_then(|p| match &p.value.blocks[..] {
              [b] => match &b.kind {
                BlockKind::Var { name, .. } => Some(name.node.clone()),
                _ => None,
              },
              _ => None,
            })
        else {
          return;
        };
        // What a constant selects to depends on what is visible: a wider
        // scope decides again.
        for c in &mut self.consts {
          c.state = ConstState::Written;
        }
        self.consts.push(Const {
          name,
          written: stmt.clone(),
          selects: contains_if(std::slice::from_ref(stmt)),
          state: ConstState::Written,
        });
      }
      "fn" => {
        if let Some(f) = params
          .and_then(|p| p.items.first())
          .and_then(|p| upper_name_of(&p.value))
        {
          self.functions.push(f);
        }
      }
      _ => {}
    }
  }

  /// Decides a whole program (a tree lowered without the include pass, or
  /// after it): its top-level `@if`s in order, then every other one.
  pub(crate) fn program(
    &mut self,
    statements: Vec<Statement>,
    files: FilesRef<'_>,
    problems: &mut Vec<Problem>,
  ) -> Vec<Statement> {
    let top = self.top_level(statements, files, problems);
    // Bodies see every constant, wherever it is declared.
    self.consts.clear();
    self.functions.clear();
    for stmt in &top {
      self.declare(stmt);
    }
    top
      .into_iter()
      .map(|mut stmt| {
        self.statement(&mut stmt, files, problems);
        stmt
      })
      .collect()
  }

  fn top_level(
    &mut self,
    statements: Vec<Statement>,
    files: FilesRef<'_>,
    problems: &mut Vec<Problem>,
  ) -> Vec<Statement> {
    let mut out = Vec::with_capacity(statements.len());
    for stmt in statements {
      match if_statement(stmt) {
        Ok(block) => {
          if let Some(branch) = self.select(block, true, files, problems) {
            out.extend(self.top_level(branch, files, problems));
          }
        }
        Err(stmt) => {
          self.declare(&stmt);
          out.push(stmt);
        }
      }
    }
    out
  }

  /// The statements of the branch `@if` takes (empty when it takes none),
  /// or `None` when it cannot be decided (reported).
  pub(crate) fn select(
    &mut self,
    block: Block,
    top_level: bool,
    files: FilesRef<'_>,
    problems: &mut Vec<Problem>,
  ) -> Option<Vec<Statement>> {
    let span = block.span;
    let BlockKind::Func {
      params: Some(params),
      ..
    } = block.kind
    else {
      unreachable!("an `@if` site has parameters");
    };
    let mut items = params.items.into_iter();
    let (Some(mut cond), Some(then), other, None) =
      (items.next(), items.next(), items.next(), items.next())
    else {
      problems.push(Problem::construct(
        span,
        "generic",
        "declaration",
        format!("`@if` takes a condition and one or two blocks: {USAGE}"),
      ));
      return None;
    };
    for item in [Some(&cond), Some(&then), other.as_ref()]
      .into_iter()
      .flatten()
    {
      if let Some(name) = &item.name {
        problems.push(Problem::construct(
          name.span,
          "generic",
          "declaration",
          format!(
            "`@if` takes no named parameters (`{}:`): {USAGE}",
            name.node
          ),
        ));
        return None;
      }
    }
    let then = branch(then.value, problems)?;
    let other = match other {
      Some(p) => Some(branch(p.value, problems)?),
      None => None,
    };
    // A condition may itself hold an `@if` (inside a `#( )`).
    self.pipe(&mut cond.value, true, files, problems);
    let value = self.condition(&cond.value, top_level, files, problems)?;
    let (taken, statements) = match (value, then, other) {
      (true, (span, stmts), _) => (Some(span), stmts),
      (false, _, Some((span, stmts))) => (Some(span), stmts),
      (false, _, None) => (None, Vec::new()),
    };
    if self.recording {
      self.decided.push(Conditional { span, value, taken });
    }
    Some(statements)
  }

  /// The condition's value: a Bool known while loading.
  fn condition(
    &mut self,
    cond: &Pipe,
    top_level: bool,
    files: FilesRef<'_>,
    problems: &mut Vec<Problem>,
  ) -> Option<bool> {
    let block = match &cond.blocks[..] {
      [b]
        if matches!(
          b.kind,
          BlockKind::Literal(_) | BlockKind::Func { params: None, .. } | BlockKind::Eval(_)
        ) =>
      {
        b
      }
      _ => {
        let runtime = match &cond.blocks[..] {
          [b] => matches!(b.kind, BlockKind::Var { .. } | BlockKind::Path { .. }),
          _ => false,
        };
        problems.push(
          Problem::construct(
            cond.span,
            "generic",
            "if-condition",
            if runtime {
              "the condition of `@if` is a runtime value; `@if` is decided while loading".into()
            } else {
              "the condition of `@if` must be known while loading: `true`, `false`, `@name` or `#( ... )`".into()
            },
          )
          .help("to branch on a runtime value use `If(...)`, whose branches both compose; to compute a condition while loading, wrap it in `#( ... )`"),
        );
        return None;
      }
    };
    if let Some(f) = self.calls_function(block) {
      problems.push(
        Problem::construct(
          cond.span,
          "generic",
          "if-condition",
          format!(
            "the condition of `@if` calls the function `{f}`: conditions are decided while loading, before functions are lowered"
          ),
        )
        .help("compute it from literals, constants and shards, or select with constants (`@const`) the host can define"),
      );
      return None;
    }
    // The constants it reads, through each other, decided first.
    let mut names = Vec::new();
    names_read(block, &mut names);
    let mut seen = Vec::new();
    while let Some(name) = names.pop() {
      if seen.contains(&name) {
        continue;
      }
      let at = match &block.kind {
        BlockKind::Func { name, params: None } => name.span,
        _ => cond.span,
      };
      if !self.ensure(&name, at, files, problems) {
        // Reported, or a cascade of what was.
        return None;
      }
      for c in self.consts.iter().filter(|c| c.name == name) {
        if let Some(value) = c.readable().and_then(Const::value) {
          for b in &value.blocks {
            names_read(b, &mut names);
          }
        }
      }
      seen.push(name);
    }
    let readable: Vec<Statement> = self
      .consts
      .iter()
      .filter_map(|c| c.readable().cloned())
      .collect();
    let (value, mut found, reads) = condition_value(
      block,
      &readable,
      self.catalog,
      self.defines,
      files,
      top_level,
    );
    for read in reads {
      if !self.reads.contains(&read) {
        self.reads.push(read);
      }
    }
    if !found.is_empty() {
      problems.append(&mut found);
      return None;
    }
    match value? {
      Var::Bool(b) => Some(b),
      other => {
        let plain_define = matches!(
          &block.kind,
          BlockKind::Func { name, params: None }
            if self.defines.contains_key(&name.node) && !self.declares_default(&name.node)
        );
        let mut p = Problem::construct(
          cond.span,
          "generic",
          "if-condition",
          format!(
            "the condition of `@if` is {} (`{}`), not a Bool",
            article(&other.type_of().to_string()),
            other
          ),
        );
        p = if plain_define {
          let name = match &block.kind {
            BlockKind::Func { name, .. } => name.node.clone(),
            _ => unreachable!(),
          };
          p.help(format!(
            "script arguments and host defines arrive as strings: declare `@const({name} false ignore-redefined: true)` above it, so `{name}=true` is read as a Bool"
          ))
        } else {
          p.help("compare it into a Bool, for example `#( @n | IsMore(0) )`")
        };
        problems.push(p);
        None
      }
    }
  }

  /// Decides the `@if`s in the value of the constant `name` (each
  /// declaration of it), once per scope. One being decided already is a
  /// cycle, reported at `at`.
  fn ensure(
    &mut self,
    name: &str,
    at: Span,
    files: FilesRef<'_>,
    problems: &mut Vec<Problem>,
  ) -> bool {
    let mut ok = true;
    for i in 0..self.consts.len() {
      if self.consts[i].name != name || !self.consts[i].selects {
        continue;
      }
      match self.consts[i].state {
        ConstState::Selected(_) => continue,
        ConstState::Failed => {
          ok = false;
          continue;
        }
        ConstState::Selecting => {
          ok = false;
          let first = self.selecting.iter().position(|c| c == name).unwrap_or(0);
          let mut chain = self.selecting[first..].to_vec();
          chain.push(name.to_string());
          problems.push(Problem::construct(
            at,
            "generic",
            "compose-time-cycle",
            format!(
              "`@{name}` is read while its own `@if` is decided ({})",
              chain.join(" -> ")
            ),
          ));
          continue;
        }
        ConstState::Written => {}
      }
      self.consts[i].state = ConstState::Selecting;
      self.selecting.push(name.to_string());
      let mut stmt = self.consts[i].written.clone();
      let recording = std::mem::replace(&mut self.recording, false);
      let before = problems.len();
      self.statement(&mut stmt, files, problems);
      self.recording = recording;
      self.selecting.pop();
      self.consts[i].state = if problems.len() == before {
        ConstState::Selected(stmt)
      } else {
        ok = false;
        ConstState::Failed
      };
    }
    ok
  }

  /// Whether a constant declared so far is a default (`ignore-redefined:
  /// true`), which reads a script argument of its name as its own type.
  fn declares_default(&self, name: &str) -> bool {
    self.consts.iter().any(|c| {
      top_level_func(&c.written).is_some_and(|(_, params, _)| {
        params
          .and_then(|p| p.items.first())
          .is_some_and(|first| {
            matches!(&first.value.blocks[..], [b] if matches!(&b.kind, BlockKind::Var { name: n, .. } if n.node == name))
          })
          && crate::lower::is_default(params)
      })
    })
  }

  /// A declared function a condition calls, directly or through the
  /// constants it reads.
  fn calls_function(&self, block: &Block) -> Option<String> {
    let mut seen = Vec::new();
    let mut found = None;
    self.visit_calls(block, &mut seen, &mut found);
    found
  }

  fn visit_calls(&self, block: &Block, seen: &mut Vec<String>, found: &mut Option<String>) {
    if found.is_some() {
      return;
    }
    let mut stmts = |l: &Selector<'_>, stmts: &[Statement], seen: &mut Vec<String>| {
      for stmt in stmts {
        if let Statement::Pipeline(pipe) = stmt {
          for b in &pipe.blocks {
            l.visit_calls(b, seen, found);
          }
        }
      }
    };
    match &block.kind {
      BlockKind::Shard { name, params } => {
        if self.functions.contains(&name.node) {
          *found = Some(name.node.clone());
          return;
        }
        for p in params.iter().flat_map(|p| &p.items) {
          for b in &p.value.blocks {
            self.visit_calls(b, seen, found);
          }
        }
      }
      BlockKind::Func { name, params: None } => {
        if seen.contains(&name.node) {
          return;
        }
        seen.push(name.node.clone());
        for stmt in self.consts.iter().filter_map(Const::readable) {
          if let Some((_, Some(params), _)) = top_level_func(stmt)
            && let [n, v, ..] = &params.items[..]
            && matches!(&n.value.blocks[..], [b] if matches!(&b.kind, BlockKind::Var { name: c, .. } if c.node == name.node))
          {
            for b in &v.value.blocks {
              self.visit_calls(b, seen, found);
            }
          }
        }
      }
      BlockKind::Func {
        params: Some(params),
        ..
      } => {
        for p in &params.items {
          for b in &p.value.blocks {
            self.visit_calls(b, seen, found);
          }
        }
      }
      BlockKind::Seq(items) => {
        for b in items.iter().flat_map(|p| &p.blocks) {
          self.visit_calls(b, seen, found);
        }
      }
      BlockKind::Table(entries) => {
        for b in entries.iter().flat_map(|(_, p)| &p.blocks) {
          self.visit_calls(b, seen, found);
        }
      }
      BlockKind::Flow(s) | BlockKind::Expr(s) | BlockKind::Eval(s) => stmts(self, s, seen),
      BlockKind::FString(parts) => {
        for part in parts {
          if let FPart::Expr(s, _) = part {
            stmts(self, s, seen);
          }
        }
      }
      _ => {}
    }
  }

  /// Decides every `@if` in a statement list: one written alone as a
  /// statement is replaced by its branch's statements.
  fn statements(
    &mut self,
    statements: Vec<Statement>,
    files: FilesRef<'_>,
    problems: &mut Vec<Problem>,
  ) -> Vec<Statement> {
    let mut out = Vec::with_capacity(statements.len());
    for stmt in statements {
      match if_statement(stmt) {
        Ok(block) => {
          if let Some(branch) = self.select(block, false, files, problems) {
            out.extend(self.statements(branch, files, problems));
          }
        }
        Err(mut stmt) => {
          self.statement(&mut stmt, files, problems);
          out.push(stmt);
        }
      }
    }
    out
  }

  fn statement(&mut self, stmt: &mut Statement, files: FilesRef<'_>, problems: &mut Vec<Problem>) {
    if let Statement::Pipeline(pipe) = stmt {
      self.pipe(pipe, false, files, problems);
    }
  }

  /// `value`: the pipe is a value (a parameter, an element, a table
  /// value), which an `@if` alone in it must give.
  fn pipe(
    &mut self,
    pipe: &mut Pipe,
    value: bool,
    files: FilesRef<'_>,
    problems: &mut Vec<Problem>,
  ) {
    let alone = value && pipe.blocks.len() == 1;
    for block in &mut pipe.blocks {
      self.block(block, alone, files, problems);
    }
  }

  fn stmts(
    &mut self,
    stmts: &mut Vec<Statement>,
    files: FilesRef<'_>,
    problems: &mut Vec<Problem>,
  ) {
    let taken = std::mem::take(stmts);
    *stmts = self.statements(taken, files, problems);
  }

  /// An `@if` among other blocks of a pipe, or inside a value, becomes its
  /// branch: the branch's one block, or its statements in parentheses
  /// (which run in place, in the enclosing scope).
  /// `value`: the block is a whole value, so a branch must give one.
  fn block(
    &mut self,
    block: &mut Block,
    value: bool,
    files: FilesRef<'_>,
    problems: &mut Vec<Problem>,
  ) {
    if is_if(block) {
      let span = block.span;
      let site = std::mem::replace(
        block,
        Block {
          kind: BlockKind::Expr(Vec::new()),
          span,
        },
      );
      if let Some(mut branch) = self.select(site, false, files, problems) {
        if value && branch.is_empty() {
          problems.push(
            Problem::construct(
              span,
              "generic",
              "if-no-value",
              "this `@if` stands for a value, but the branch it takes is empty".into(),
            )
            .help("give it the other branch (`@if(condition {then} {else})`), or make it a step of its own"),
          );
        } else if let [Statement::Pipeline(pipe)] = &mut branch[..]
          && pipe.blocks.len() == 1
        {
          *block = pipe.blocks.pop().expect("one block");
          self.block(block, value, files, problems);
        } else {
          block.kind = BlockKind::Expr(self.statements(branch, files, problems));
        }
      }
      return;
    }
    match &mut block.kind {
      BlockKind::Shard {
        params: Some(params),
        ..
      }
      | BlockKind::Func {
        params: Some(params),
        ..
      } => {
        for item in &mut params.items {
          self.pipe(&mut item.value, true, files, problems);
        }
      }
      BlockKind::Seq(items) => {
        for item in items {
          self.pipe(item, true, files, problems);
        }
      }
      BlockKind::Table(entries) => {
        for (key, value) in entries {
          if let TableKey::Value(b) = &mut key.node {
            self.block(b, true, files, problems);
          }
          self.pipe(value, true, files, problems);
        }
      }
      BlockKind::Flow(s) | BlockKind::Expr(s) | BlockKind::Eval(s) => {
        self.stmts(s, files, problems)
      }
      BlockKind::FString(parts) => {
        for part in parts {
          if let FPart::Expr(s, _) = part {
            self.stmts(s, files, problems);
          }
        }
      }
      _ => {}
    }
  }
}

/// A branch: a block in braces (`{}` is an empty one), with its span.
fn branch(pipe: Pipe, problems: &mut Vec<Problem>) -> Option<(Span, Vec<Statement>)> {
  let span = pipe.span;
  let mut blocks = pipe.blocks;
  if blocks.len() == 1 {
    let block = blocks.pop().expect("one block");
    match block.kind {
      BlockKind::Flow(stmts) => return Some((block.span, stmts)),
      BlockKind::EmptyBraces => return Some((block.span, Vec::new())),
      _ => {}
    }
  }
  problems.push(
    Problem::construct(
      span,
      "generic",
      "declaration",
      format!("the branches of `@if` are blocks in braces: {USAGE}"),
    )
    .help("wrap the code in `{ ... }`"),
  );
  None
}

fn article(ty: &str) -> String {
  match ty.chars().next() {
    Some('A' | 'E' | 'I' | 'O' | 'U') => format!("an {ty}"),
    _ => format!("a {ty}"),
  }
}

/// `@if(...)` with parameters.
pub(crate) fn is_if(block: &Block) -> bool {
  matches!(&block.kind, BlockKind::Func { name, params: Some(_) } if name.node == "if")
}

/// A statement that is `@if(...)` alone (nothing piped): its block.
pub(crate) fn if_statement(stmt: Statement) -> Result<Block, Statement> {
  match stmt {
    Statement::Pipeline(mut pipe) if pipe.blocks.len() == 1 && is_if(&pipe.blocks[0]) => {
      Ok(pipe.blocks.pop().expect("one block"))
    }
    other => Err(other),
  }
}

/// Whether a program holds an `@if` anywhere (lowering copies the tree to
/// decide them only when it does).
pub(crate) fn contains_if(statements: &[Statement]) -> bool {
  fn pipe(p: &Pipe) -> bool {
    p.blocks.iter().any(block)
  }
  fn block(b: &Block) -> bool {
    if is_if(b) {
      return true;
    }
    match &b.kind {
      BlockKind::Shard {
        params: Some(params),
        ..
      }
      | BlockKind::Func {
        params: Some(params),
        ..
      } => params.items.iter().any(|i| pipe(&i.value)),
      BlockKind::Seq(items) => items.iter().any(pipe),
      BlockKind::Table(entries) => entries
        .iter()
        .any(|(k, v)| matches!(&k.node, TableKey::Value(b) if block(b)) || pipe(v)),
      BlockKind::Flow(s) | BlockKind::Expr(s) | BlockKind::Eval(s) => contains_if(s),
      BlockKind::FString(parts) => parts
        .iter()
        .any(|p| matches!(p, FPart::Expr(s, _) if contains_if(s))),
      _ => false,
    }
  }
  statements.iter().any(|s| match s {
    Statement::Pipeline(p) => pipe(p),
    _ => false,
  })
}

/// The names `@name` reads in a block, anywhere inside it.
fn names_read(block: &Block, out: &mut Vec<String>) {
  fn pipe(p: &Pipe, out: &mut Vec<String>) {
    p.blocks.iter().for_each(|b| names_read(b, out));
  }
  fn stmts(s: &[Statement], out: &mut Vec<String>) {
    for stmt in s {
      if let Statement::Pipeline(p) = stmt {
        pipe(p, out);
      }
    }
  }
  match &block.kind {
    BlockKind::Func { name, params: None } => out.push(name.node.clone()),
    BlockKind::Shard {
      params: Some(params),
      ..
    }
    | BlockKind::Func {
      params: Some(params),
      ..
    } => params.items.iter().for_each(|i| pipe(&i.value, out)),
    BlockKind::Seq(items) => items.iter().for_each(|i| pipe(i, out)),
    BlockKind::Table(entries) => entries.iter().for_each(|(_, v)| pipe(v, out)),
    BlockKind::Flow(s) | BlockKind::Expr(s) | BlockKind::Eval(s) => stmts(s, out),
    BlockKind::FString(parts) => {
      for part in parts {
        if let FPart::Expr(s, _) = part {
          stmts(s, out);
        }
      }
    }
    _ => {}
  }
}
