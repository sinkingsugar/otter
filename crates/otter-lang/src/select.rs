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
  /// The top-level `@const` declarations a condition may read.
  consts: Vec<Statement>,
  /// The declared functions' names: a condition cannot call one.
  functions: Vec<String>,
  /// Every `@if` decided, in the order decided.
  pub(crate) decided: Vec<Conditional>,
  /// The files `@read` read while deciding.
  pub(crate) reads: Vec<String>,
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
      "const" => self.consts.push(stmt.clone()),
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
    self.pipe(&mut cond.value, files, problems);
    let value = self.condition(&cond.value, top_level, files, problems)?;
    let (taken, statements) = match (value, then, other) {
      (true, (span, stmts), _) => (Some(span), stmts),
      (false, _, Some((span, stmts))) => (Some(span), stmts),
      (false, _, None) => (None, Vec::new()),
    };
    self.decided.push(Conditional { span, value, taken });
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
    let (value, mut found, reads) = condition_value(
      block,
      &self.consts,
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

  /// Whether a constant declared so far is a default (`ignore-redefined:
  /// true`), which reads a script argument of its name as its own type.
  fn declares_default(&self, name: &str) -> bool {
    self.consts.iter().any(|stmt| {
      top_level_func(stmt).is_some_and(|(_, params, _)| {
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
        for stmt in &self.consts {
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
      self.pipe(pipe, files, problems);
    }
  }

  fn pipe(&mut self, pipe: &mut Pipe, files: FilesRef<'_>, problems: &mut Vec<Problem>) {
    for block in &mut pipe.blocks {
      self.block(block, files, problems);
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
  fn block(&mut self, block: &mut Block, files: FilesRef<'_>, problems: &mut Vec<Problem>) {
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
        if let [Statement::Pipeline(pipe)] = &mut branch[..]
          && pipe.blocks.len() == 1
        {
          *block = pipe.blocks.pop().expect("one block");
          self.block(block, files, problems);
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
          self.pipe(&mut item.value, files, problems);
        }
      }
      BlockKind::Seq(items) => {
        for item in items {
          self.pipe(item, files, problems);
        }
      }
      BlockKind::Table(entries) => {
        for (key, value) in entries {
          if let TableKey::Value(b) = &mut key.node {
            self.block(b, files, problems);
          }
          self.pipe(value, files, problems);
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
