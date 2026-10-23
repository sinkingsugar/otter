//! Recursive-descent parser with recovery: it reports every problem it can
//! in one pass, in language terms ("missing `}` for the flow opened at
//! 1:9"), and never names grammar rules.

use crate::ast::*;
use crate::lexer::{Lexer, Tok, Token};
use crate::problem::Problem;
use crate::source::{Source, Span};

pub struct Parser<'a> {
  source: &'a Source,
  tokens: Vec<Token>,
  pos: usize,
  /// Closers of the delimiters currently open, innermost last.
  open: Vec<Tok>,
  /// How many `[`, `{` and `(` enclose the current block.
  depth: usize,
  /// Parsing stopped (input nested too deeply): open delimiters are not
  /// reported as unclosed.
  aborted: bool,
  pub problems: Vec<Problem>,
  /// Tokens a `?` follows: the last segment of a lenient path (the
  /// lexer's `tokenize_marked`).
  lenient: Vec<usize>,
}

/// Parses a whole source. Problems include the lexer's.
pub fn parse(source: &Source) -> (Program, Vec<Problem>) {
  parse_range(source, Span::new(0, source.text.len()))
}

/// Parses one file of a source (`Source::add`): its spans are offsets in
/// the whole source.
pub fn parse_range(source: &Source, range: Span) -> (Program, Vec<Problem>) {
  let (tokens, mut problems, lenient) =
    Lexer::new(&source.text, range.start, range.end).tokenize_marked();
  let mut parser = Parser::new(source, tokens);
  parser.lenient = lenient;
  let statements = parser.statements(None);
  problems.append(&mut parser.problems);
  problems.sort_by_key(|p| p.span.start);
  (Program { statements }, problems)
}

impl<'a> Parser<'a> {
  fn new(source: &'a Source, tokens: Vec<Token>) -> Parser<'a> {
    Parser {
      source,
      tokens,
      pos: 0,
      open: Vec::new(),
      depth: 0,
      aborted: false,
      problems: Vec::new(),
      lenient: Vec::new(),
    }
  }

  fn peek(&self) -> &Token {
    &self.tokens[self.pos.min(self.tokens.len() - 1)]
  }

  fn peek_at(&self, ahead: usize) -> &Token {
    &self.tokens[(self.pos + ahead).min(self.tokens.len() - 1)]
  }

  fn bump(&mut self) -> Token {
    let t = self.peek().clone();
    if t.tok != Tok::Eof {
      self.pos += 1;
    }
    t
  }

  fn at(&self, tok: &Tok) -> bool {
    &self.peek().tok == tok
  }

  fn is_closer(tok: &Tok) -> bool {
    matches!(tok, Tok::RParen | Tok::RBracket | Tok::RBrace)
  }

  fn position(&self, span: Span) -> String {
    self.source.position(span.start)
  }

  fn problem(&mut self, p: Problem) {
    // After stopping on deep nesting, every open delimiter is "unclosed":
    // only the depth problem is useful.
    if self.aborted && matches!(p.code, "unclosed" | "dangling-pipe") {
      return;
    }
    self.problems.push(p);
  }

  /// Whether a closer belongs to an enclosing delimiter (then the inner
  /// construct stops and leaves it to its owner).
  fn closes_outer(&self, tok: &Tok) -> bool {
    self.open.iter().any(|o| o == tok)
  }

  /// Parses items until `closer` (consumed) or the end. Reports a missing
  /// closer at the opener's position.
  fn delimited<T>(
    &mut self,
    closer: Tok,
    what: String,
    open_span: Span,
    mut item: impl FnMut(&mut Self) -> Option<T>,
  ) -> (Vec<T>, Span) {
    self.open.push(closer.clone());
    let mut items = Vec::new();
    let end = loop {
      let t = self.peek().clone();
      if t.tok == closer {
        self.bump();
        break t.span.end;
      }
      if t.tok == Tok::Eof {
        self.problem(Problem::syntax(
          open_span,
          "unclosed",
          format!(
            "missing {} for the {what} opened at {}",
            closer.describe(),
            self.position(open_span)
          ),
        ));
        break t.span.start;
      }
      if Self::is_closer(&t.tok) {
        // Another closer: it belongs to an enclosing construct (this one
        // is missing its closer), or it matches nothing.
        let outer = {
          self.open.pop();
          let o = self.closes_outer(&t.tok);
          self.open.push(closer.clone());
          o
        };
        if outer {
          self.problem(Problem::syntax(
            open_span,
            "unclosed",
            format!(
              "missing {} for the {what} opened at {} (found {} at {})",
              closer.describe(),
              self.position(open_span),
              t.tok.describe(),
              self.position(t.span)
            ),
          ));
          break t.span.start;
        }
        self.problem(Problem::syntax(
          t.span,
          "unmatched",
          format!(
            "{} does not close anything; the {what} opened at {} needs {}",
            t.tok.describe(),
            self.position(open_span),
            closer.describe()
          ),
        ));
        self.bump();
        continue;
      }
      let before = self.pos;
      if let Some(v) = item(self) {
        items.push(v);
      }
      if self.pos == before {
        // No progress: skip the token (already reported by `item`).
        self.bump();
      }
    };
    self.open.pop();
    // Completed syntax lists are immutable; spare capacity otherwise compounds
    // across every small nested parameter/statement list.
    items.shrink_to_fit();
    (items, Span::new(open_span.start, end))
  }

  /// A sequence of statements, up to `closer` (not consumed) or the end.
  fn statements(&mut self, closer: Option<&Tok>) -> Vec<Statement> {
    let mut out = Vec::new();
    loop {
      let t = self.peek().clone();
      if t.tok == Tok::Eof || Some(&t.tok) == closer {
        break;
      }
      if Self::is_closer(&t.tok) {
        if self.closes_outer(&t.tok) {
          break;
        }
        self.problem(Problem::syntax(
          t.span,
          "unmatched",
          format!("{} does not close anything", t.tok.describe()),
        ));
        self.bump();
        continue;
      }
      let before = self.pos;
      if let Some(s) = self.statement() {
        out.push(s);
      }
      if self.pos == before {
        self.bump();
      }
    }
    out.shrink_to_fit();
    out
  }

  fn statement(&mut self) -> Option<Statement> {
    let t = self.peek().clone();
    if let Tok::Assign(op) = t.tok {
      self.bump();
      let v = self.peek().clone();
      return match v.tok {
        Tok::Ident(name) => {
          self.bump();
          if self.at(&Tok::Pipe) {
            self.bump();
          }
          Some(Statement::Assign {
            op,
            op_span: t.span,
            var: Spanned {
              node: name,
              span: v.span,
            },
          })
        }
        other => {
          self.problem(
            Problem::syntax(
              v.span,
              "expected-variable",
              format!(
                "`{}` must be followed by a variable name, found {}",
                op.text(),
                other.describe()
              ),
            )
            .help("`value = x` binds the immutable name x to the value"),
          );
          None
        }
      };
    }
    if self.at(&Tok::Pipe) {
      self.bump();
    }
    self.pipe().map(Statement::Pipeline)
  }

  /// `Block (| Block)*`.
  fn pipe(&mut self) -> Option<Pipe> {
    let first = self.block()?;
    let mut span = first.span;
    let mut blocks = vec![first];
    while self.at(&Tok::Pipe) {
      let bar = self.bump();
      match self.block() {
        Some(b) => {
          span = span.to(b.span);
          blocks.push(b);
        }
        None => {
          // `block` reported what it found; point at the dangling pipe too.
          if !Self::is_closer(&self.peek().tok) && self.peek().tok != Tok::Eof {
            break;
          }
          self.problem(Problem::syntax(
            bar.span,
            "dangling-pipe",
            "`|` must be followed by a shard or a value".into(),
          ));
          break;
        }
      }
    }
    blocks.shrink_to_fit();
    Some(Pipe { blocks, span })
  }

  fn block(&mut self) -> Option<Block> {
    // Depth counts what the reader sees nest: `[`, `{` and `(`, including a
    // shard's parameter list (`params_if_open`).
    let nests = matches!(
      self.peek().tok,
      Tok::LBracket | Tok::LBrace | Tok::LParen | Tok::Hash
    );
    if !nests {
      return self.block_inner();
    }
    if !self.enter() {
      return None;
    }
    let block = self.block_inner();
    self.depth -= 1;
    block
  }

  /// Enters one nesting level, or reports `too-deep` once and stops parsing
  /// (deeper input would only exhaust the stack, here or in lowering).
  fn enter(&mut self) -> bool {
    if self.depth >= MAX_DEPTH {
      let t = self.peek().clone();
      self.problem(Problem::syntax(
        t.span,
        "too-deep",
        format!("brackets, braces and parentheses nest more than {MAX_DEPTH} levels deep"),
      ));
      while self.peek().tok != Tok::Eof {
        self.bump();
      }
      self.aborted = true;
      return false;
    }
    self.depth += 1;
    true
  }

  fn block_inner(&mut self) -> Option<Block> {
    let t = self.peek().clone();
    let start = t.span;
    let kind = match t.tok {
      Tok::Upper(name) => {
        self.bump();
        let params = self.params_if_open(&name);
        BlockKind::Shard {
          name: Spanned {
            node: name,
            span: start,
          },
          params,
        }
      }
      Tok::Ident(name) => {
        self.bump();
        match name.as_str() {
          "none" => BlockKind::Literal(Literal::None),
          "true" => BlockKind::Literal(Literal::Bool(true)),
          "false" => BlockKind::Literal(Literal::Bool(false)),
          "null" => {
            self.problem(
              Problem::syntax(start, "null", "`null` is spelled `none` in Otter".into())
                .fix("none"),
            );
            BlockKind::Literal(Literal::None)
          }
          _ if self.at(&Tok::LParen) && !self.peek().spaced => {
            // `log(...)`: probably a shard; lowering reports it by name.
            let params = self.params_if_open(&name);
            BlockKind::Shard {
              name: Spanned {
                node: name,
                span: start,
              },
              params,
            }
          }
          _ => {
            let path = self.path();
            let lenient = !path.is_empty() && self.marked();
            BlockKind::Var {
              name: Spanned {
                node: name,
                span: start,
              },
              path,
              lenient,
            }
          }
        }
      }
      Tok::Dot => self.step_path(start)?,
      Tok::Int(i) => {
        self.bump();
        BlockKind::Literal(Literal::Int(i))
      }
      Tok::Float(f) => {
        self.bump();
        BlockKind::Literal(Literal::Float(f))
      }
      Tok::Str(s) => {
        self.bump();
        BlockKind::Literal(Literal::String(s))
      }
      Tok::FStr(content) => {
        self.bump();
        BlockKind::FString(self.fstring(content))
      }
      Tok::Enum(ty, value) => {
        self.bump();
        BlockKind::Enum(ty, value)
      }
      Tok::LBracket => {
        self.bump();
        let (items, span) =
          self.delimited(Tok::RBracket, "sequence".into(), start, |p| p.element());
        return Some(Block {
          kind: BlockKind::Seq(items),
          span,
        });
      }
      Tok::LBrace => {
        self.bump();
        return Some(self.braces(start));
      }
      Tok::LParen => {
        self.bump();
        let (stmts, span) = self.nested_statements(Tok::RParen, "expression", start);
        return Some(Block {
          kind: BlockKind::Expr(stmts),
          span,
        });
      }
      Tok::Hash if self.peek_at(1).tok == Tok::LParen => {
        self.bump();
        let open = self.bump().span;
        let (stmts, span) = self.nested_statements(Tok::RParen, "`#(...)` expression", open);
        return Some(Block {
          kind: BlockKind::Eval(stmts),
          span: start.to(span),
        });
      }
      Tok::At => {
        self.bump();
        let n = self.peek().clone();
        let name = match n.tok {
          Tok::Ident(name) | Tok::Upper(name) => {
            self.bump();
            name
          }
          other => {
            self.problem(Problem::syntax(
              n.span,
              "expected-name",
              format!("`@` must be followed by a name, found {}", other.describe()),
            ));
            return None;
          }
        };
        let params = self.params_if_open(&format!("@{name}"));
        BlockKind::Func {
          name: Spanned {
            node: name,
            span: n.span,
          },
          params,
        }
      }
      ref other => {
        if !Self::is_closer(other) && *other != Tok::Eof {
          self.problem(Problem::syntax(
            t.span,
            "expected-value",
            format!("expected a shard or a value, found {}", other.describe()),
          ));
          self.bump();
        }
        return None;
      }
    };
    let end = self.tokens[self.pos.saturating_sub(1)].span.end;
    Some(Block {
      kind,
      span: Span::new(start.start, end.max(start.end)),
    })
  }

  /// A sequence element or a table value: one pipe. An assignment here is a
  /// mistake (it only makes sense between statements).
  fn element(&mut self) -> Option<Pipe> {
    if let Tok::Assign(op) = self.peek().tok {
      let span = self.bump().span;
      self.problem(Problem::syntax(
        span,
        "assignment-in-value",
        format!(
          "`{}` assigns between statements; it cannot appear inside a value",
          op.text()
        ),
      ));
      return None;
    }
    if matches!(self.peek().tok, Tok::Dot) {
      self.path_in_value();
      return None;
    }
    self.pipe()
  }

  /// A path step (`.key`, `.0.key`) reading the step's input; `start` is
  /// the `.`. Out of line: rare, and kept out of `block_inner`.
  #[inline(never)]
  fn step_path(&mut self, start: Span) -> Option<BlockKind> {
    // A path step: `.key`, `.0.key`, reading the step's input.
    self.bump();
    let k = self.peek().clone();
    let key = match k.tok {
      Tok::Ident(ref s) | Tok::Upper(ref s) if !k.spaced => PathKey::Key(s.clone()),
      Tok::Int(i) if !k.spaced && i >= 0 => PathKey::Index(i),
      _ => {
        self.problem(
          Problem::syntax(
            start,
            "path",
            "`.` starting a step must be followed by a key or an index (`.key`, `.0`)".into(),
          )
          .help(
            "a path step reads the pipeline value: `x | .a.b` is `x | Take(\"a\") | Take(\"b\")`",
          ),
        );
        return None;
      }
    };
    self.bump();
    let mut path = vec![Spanned {
      node: key,
      span: k.span,
    }];
    path.extend(self.path());
    let lenient = self.marked();
    Some(BlockKind::Path { path, lenient })
  }

  /// Whether a `?` follows the token just consumed (a path's last
  /// segment): the path is lenient.
  fn marked(&self) -> bool {
    !self.lenient.is_empty() && self.lenient.binary_search(&(self.pos - 1)).is_ok()
  }

  /// A path step where a value is expected: `.5` is a misspelled 0.5, a
  /// path has no pipeline input there. Reports it and skips the pipe.
  #[cold]
  #[inline(never)]
  fn path_in_value(&mut self) {
    // A path step reads the pipeline input, and a value has none here.
    let dot = self.peek().clone();
    let next = self.peek_at(1).clone();
    if let Tok::Int(_) = next.tok
      && !next.spaced
    {
      // `.5`: a float written without its leading zero.
      let span = dot.span.to(next.span);
      let digits = &self.source.text[next.span.start..next.span.end];
      let fix = format!("0.{digits}");
      self.problem(
        Problem::syntax(
          span,
          "number-form",
          format!("a number cannot start with `.`: write {fix}"),
        )
        .fix(fix),
      );
      self.bump();
      self.bump();
      return;
    }
    let Some(block) = self.block() else {
      return;
    };
    self.problem(
      Problem::syntax(
        block.span,
        "path-in-value",
        "a path step cannot stand for a value here: it has no pipeline input".into(),
      )
      .help("a path step reads the pipeline input; bind it first or use a variable path"),
    );
    // The rest of the pipe still parses (and reports its problems).
    while self.at(&Tok::Pipe) {
      self.bump();
      if self.block().is_none() {
        return;
      }
    }
  }

  fn nested_statements(&mut self, closer: Tok, what: &str, open: Span) -> (Vec<Statement>, Span) {
    self.open.push(closer.clone());
    let stmts = self.statements(Some(&closer));
    self.open.pop();
    let t = self.peek().clone();
    let end = if t.tok == closer {
      self.bump();
      t.span.end
    } else {
      let found = if t.tok == Tok::Eof {
        String::new()
      } else {
        format!(" (found {} at {})", t.tok.describe(), self.position(t.span))
      };
      self.problem(Problem::syntax(
        open,
        "unclosed",
        format!(
          "missing {} for the {what} opened at {}{found}",
          closer.describe(),
          self.position(open)
        ),
      ));
      t.span.start
    };
    (stmts, Span::new(open.start, end))
  }

  /// `.key` / `.0` segments directly after a variable name.
  #[inline(always)]
  fn path(&mut self) -> Vec<Spanned<PathKey>> {
    let mut path = Vec::new();
    while self.at(&Tok::Dot) && !self.peek().spaced {
      let dot = self.bump();
      let k = self.peek().clone();
      let key = match k.tok {
        Tok::Ident(ref s) | Tok::Upper(ref s) if !k.spaced => PathKey::Key(s.clone()),
        Tok::Int(i) if !k.spaced && i >= 0 => PathKey::Index(i),
        _ => {
          self.problem(Problem::syntax(
            dot.span,
            "path",
            "`.` after a variable must be followed by a key or an index (`t.key`, `s.0`)".into(),
          ));
          break;
        }
      };
      self.bump();
      path.push(Spanned {
        node: key,
        span: k.span,
      });
    }
    path
  }

  /// `(...)` after a shard or `@name`, if present.
  fn params_if_open(&mut self, owner: &str) -> Option<Params> {
    if !self.at(&Tok::LParen) || !self.enter() {
      return None;
    }
    let open = self.bump().span;
    let (items, span) =
      self.delimited(Tok::RParen, format!("parameters of `{owner}`"), open, |p| {
        p.param()
      });
    self.depth -= 1;
    Some(Params { items, span })
  }

  fn param(&mut self) -> Option<Param> {
    let t = self.peek().clone();
    let name = match (&t.tok, &self.peek_at(1).tok) {
      (Tok::Ident(n), Tok::Colon) => {
        let n = n.clone();
        self.bump();
        self.bump();
        Some(Spanned {
          node: n,
          span: t.span,
        })
      }
      (Tok::Upper(n), Tok::Colon) => {
        // Uppercase names a shard; a label names a value, so it is lowercase.
        let n = n.clone();
        self.bump();
        self.bump();
        let fixed = n.to_lowercase();
        self.problem(
          Problem::syntax(
            t.span,
            "parameter-name",
            format!("parameter labels are lowercase: `{fixed}:`"),
          )
          .fix(fixed.clone()),
        );
        Some(Spanned {
          node: fixed,
          span: t.span,
        })
      }
      _ => None,
    };
    // `times: action: ...`: the next thing is another parameter name.
    let next_is_name =
      matches!(self.peek().tok, Tok::Upper(_) | Tok::Ident(_)) && self.peek_at(1).tok == Tok::Colon;
    if name.is_some() && (Self::is_closer(&self.peek().tok) || next_is_name) {
      self.problem(Problem::syntax(
        t.span,
        "missing-value",
        format!(
          "the parameter `{}:` needs a value after `:`",
          name.as_ref().map_or("", |n| n.node.as_str())
        ),
      ));
      return None;
    }
    let value = self.element();
    value.map(|value| Param { name, value })
  }

  /// `{` already consumed: an empty pair, a table, or a flow.
  fn braces(&mut self, open: Span) -> Block {
    if self.at(&Tok::RBrace) {
      let close = self.bump().span;
      return Block {
        kind: BlockKind::EmptyBraces,
        span: open.to(close),
      };
    }
    if self.table_ahead() {
      let (entries, span) = self.delimited(Tok::RBrace, "table".into(), open, |p| p.table_entry());
      return Block {
        kind: BlockKind::Table(entries),
        span,
      };
    }
    let (stmts, span) = self.nested_statements(Tok::RBrace, "flow", open);
    Block {
      kind: BlockKind::Flow(stmts),
      span,
    }
  }

  /// Whether the tokens after a `{` start a table: a key (a name, string,
  /// number, or a bracketed value) followed by `:`.
  fn table_ahead(&self) -> bool {
    match self.peek().tok {
      Tok::Ident(_) | Tok::Upper(_) | Tok::Str(_) | Tok::Int(_) | Tok::Float(_) => {
        self.peek_at(1).tok == Tok::Colon
      }
      Tok::LBracket => {
        let mut depth = 0usize;
        let mut i = 0;
        loop {
          match self.peek_at(i).tok {
            Tok::LBracket => depth += 1,
            Tok::RBracket => {
              depth -= 1;
              if depth == 0 {
                return self.peek_at(i + 1).tok == Tok::Colon;
              }
            }
            Tok::Eof => return false,
            _ => {}
          }
          i += 1;
        }
      }
      _ => false,
    }
  }

  fn table_entry(&mut self) -> Option<(Spanned<TableKey>, Pipe)> {
    let t = self.peek().clone();
    let key = match t.tok {
      Tok::Ident(ref s) | Tok::Upper(ref s) => TableKey::Name(s.clone()),
      Tok::Str(ref s) => TableKey::String(s.clone()),
      Tok::Int(i) => TableKey::Int(i),
      Tok::LBracket | Tok::Float(_) => {
        let block = self.block()?;
        let span = block.span;
        if !self.at(&Tok::Colon) {
          self.problem(Problem::syntax(
            span,
            "table-key",
            "a table key must be followed by `:`".into(),
          ));
          return None;
        }
        self.bump();
        let value = self.element()?;
        return Some((
          Spanned {
            node: TableKey::Value(Box::new(block)),
            span,
          },
          value,
        ));
      }
      ref other => {
        self.problem(Problem::syntax(
          t.span,
          "table-key",
          format!(
            "expected a table key (`name:` or `\"key\":`), found {}",
            other.describe()
          ),
        ));
        return None;
      }
    };
    self.bump();
    if !self.at(&Tok::Colon) {
      self.problem(Problem::syntax(
        t.span,
        "table-key",
        format!("table key `{}` must be followed by `:`", key.text()),
      ));
      return None;
    }
    self.bump();
    if Self::is_closer(&self.peek().tok) || self.peek().tok == Tok::Eof {
      self.problem(Problem::syntax(
        t.span,
        "missing-value",
        format!("table key `{}` needs a value after `:`", key.text()),
      ));
      return None;
    }
    let value = self.element()?;
    Some((
      Spanned {
        node: key,
        span: t.span,
      },
      value,
    ))
  }

  /// Splits an f-string's content into text and `{pipeline}` parts.
  fn fstring(&mut self, content: Span) -> Vec<FPart> {
    let text = &self.source.text;
    let bytes = text.as_bytes();
    let mut parts = Vec::new();
    let mut buf = String::new();
    let mut i = content.start;
    while i < content.end {
      match bytes[i] {
        b'{' if bytes.get(i + 1) == Some(&b'{') => {
          buf.push('{');
          i += 2;
        }
        b'}' if bytes.get(i + 1) == Some(&b'}') => {
          buf.push('}');
          i += 2;
        }
        b'{' => {
          let open = i;
          let close = match_brace(bytes, i, content.end);
          let Some(close) = close else {
            self.problem(Problem::syntax(
              Span::new(open, open + 1),
              "unclosed",
              "this `{` in the f-string is never closed (write `{{` for a literal brace)".into(),
            ));
            break;
          };
          if !buf.is_empty() {
            parts.push(FPart::Text(std::mem::take(&mut buf)));
          }
          let (tokens, mut lex_problems, lenient) =
            Lexer::new(text, open + 1, close).tokenize_marked();
          self.problems.append(&mut lex_problems);
          let mut inner = Parser::new(self.source, tokens);
          inner.lenient = lenient;
          inner.depth = self.depth;
          let stmts = inner.statements(None);
          self.problems.append(&mut inner.problems);
          let span = Span::new(open, close + 1);
          if stmts.is_empty() {
            self.problem(Problem::syntax(
              span,
              "empty-interpolation",
              "`{}` in an f-string needs a value inside (write `{{}}` for literal braces)".into(),
            ));
          } else {
            parts.push(FPart::Expr(stmts, span));
          }
          i = close + 1;
        }
        b'}' => {
          self.problem(
            Problem::syntax(
              Span::new(i, i + 1),
              "unmatched",
              "`}` in an f-string closes nothing (write `}}` for a literal brace)".into(),
            )
            .fix("}}"),
          );
          i += 1;
        }
        b'\\' => {
          let c = text[i + 1..content.end].chars().next();
          i += 1 + c.map_or(0, char::len_utf8);
          match c {
            Some(c) if crate::lexer::escape(c).is_some() => buf.extend(crate::lexer::escape(c)),
            Some(other) => self.problem(
              Problem::syntax(
                Span::new(i - 1 - other.len_utf8(), i),
                "string-escape",
                format!("unknown escape `\\{other}` in a string"),
              )
              .help(crate::lexer::KNOWN_ESCAPES),
            ),
            None => {}
          }
        }
        _ => {
          let c = text[i..content.end].chars().next().unwrap_or('\0');
          buf.push(c);
          i += c.len_utf8();
        }
      }
    }
    if !buf.is_empty() {
      parts.push(FPart::Text(buf));
    }
    parts
  }
}

/// How deeply `[`, `{` and `(` may nest. Real scripts stay far below; the
/// limit keeps generated or hostile input from overflowing the parser's or
/// lowering's stack. Compose separately limits flow nesting
/// (`otter_core::compose::MAX_FLOW_DEPTH`), which also counts function
/// bodies and spawned wires.
pub const MAX_DEPTH: usize = 64;

/// The index of the `}` matching the `{` at `open`, skipping strings.
fn match_brace(bytes: &[u8], open: usize, end: usize) -> Option<usize> {
  let mut depth = 0usize;
  let mut i = open;
  while i < end {
    match bytes[i] {
      b'"' => {
        i += 1;
        while i < end && bytes[i] != b'"' {
          i += if bytes[i] == b'\\' { 2 } else { 1 };
        }
      }
      b'{' => depth += 1,
      b'}' => {
        depth -= 1;
        if depth == 0 {
          return Some(i);
        }
      }
      _ => {}
    }
    i += 1;
  }
  None
}
