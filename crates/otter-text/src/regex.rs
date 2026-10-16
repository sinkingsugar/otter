//! `Regex.Match`, `Regex.Search` and `Regex.Replace`, following 1.x
//! (`shards/modules/core/strings.cpp`) on the Rust `regex` engine instead of
//! `std::regex`: an RE2-style dialect that matches in time linear in the
//! input, with no backreferences or lookaround.
//!
//! A constant pattern compiles once, at compose, into the shared compiled
//! output (the compiled regexes are `Sync`; their scratch space is pooled
//! inside them). An invalid one is a compose error located at the argument.
//! A pattern from a variable compiles at activation, and each instance keeps
//! the last one it compiled, so a pattern that does not change compiles once
//! per instance. Every pattern is bounded ([`SIZE_LIMIT`], [`CACHE_LIMIT`]),
//! since scripts may pass patterns a model wrote.
//!
//! Differences from 1.x are listed in docs/current-state.md ("Deviations
//! from 1.x").

use std::borrow::Cow;
use std::sync::Arc;

use ::regex::{Regex, RegexBuilder};
use otter_core::compose::{Binding, ComposeCtx};
use otter_core::describe::{
  Forms, InputDesc, OutputDesc, ParamDecl, Params, Requirement, ShardDesc, Targets, TypeName,
};
use otter_core::diagnostic::{Diagnostic, Phase};
use otter_core::instance::{InstanceCtx, LeafCtx};
use otter_core::shard::Flow;
use otter_core::shards::Operand;
use otter_core::shards::leaf::{LeafShard, leaf_type};
use otter_core::{Args, Composed, Error, Result, ShardType, Type, Var, shard_doc};
use regex_automata::meta;
use regex_automata::util::captures::Captures;
use regex_automata::util::syntax;
use regex_syntax::hir::{Hir, Look};

/// The largest compiled program a pattern may produce, in bytes (the
/// `regex` crate's own default is 10 MiB). A larger one is an error, at
/// compose for a constant pattern.
pub const SIZE_LIMIT: usize = 1 << 20;

/// The most memory a pattern's lazy DFA may use for its cache, in bytes,
/// per concurrent search (the `regex` crate's default is 2 MiB). Past it
/// the search falls back to slower engines, still linear; it never fails.
pub const CACHE_LIMIT: usize = 1 << 20;

pub static MATCH: ShardType = leaf_type::<Match>();
pub static SEARCH: ShardType = leaf_type::<Search>();
pub static REPLACE: ShardType = leaf_type::<Replace>();

const STRING: &[TypeName] = &[TypeName::String];
const TEXT: Forms = Forms::LITERAL.or(Forms::VARIABLE);

const fn desc(
  name: &'static str,
  summary: &'static str,
  help: &'static str,
  params: &'static [ParamDecl],
  output: TypeName,
) -> ShardDesc {
  ShardDesc {
    name,
    version: 1,
    summary,
    help,
    params: Params::Declared(params),
    input: InputDesc::Types(STRING),
    output: OutputDesc::Fixed(output),
    targets: Targets::All,
    aliases: &[],
    effects: otter_core::signature::Effects::NONE,
    // The per-instance cache of a variable pattern changes no output.
    lifetime: otter_core::signature::Lifetime::Stateless,
  }
}

const REGEX_PARAM: ParamDecl = ParamDecl::new(
  "regex",
  shard_doc!(
    "The pattern: a String literal (compiled once, when the program composes) or a String variable (compiled when its value changes)."
  ),
  TEXT,
  STRING,
  Requirement::Required,
);

static PATTERN_PARAMS: &[ParamDecl] = &[REGEX_PARAM];

static REPLACE_PARAMS: &[ParamDecl] = &[
  REGEX_PARAM,
  ParamDecl::new(
    "replacement",
    shard_doc!(
      "What replaces each match: a String literal or variable. `$0` is the whole match, `$1` a group by number, `$name` or `${name}` a group by name (write `${1}x` when letters, digits or `_` follow), `$$` a literal `$`."
    ),
    TEXT,
    STRING,
    Requirement::Required,
  ),
];

// --- compiling ---

/// A pattern for searching and replacing.
fn compile_search(pattern: &str) -> std::result::Result<Regex, String> {
  RegexBuilder::new(pattern)
    .size_limit(SIZE_LIMIT)
    .dfa_size_limit(CACHE_LIMIT)
    .build()
    .map_err(|e| format!("invalid regex: {e}"))
}

/// A pattern that must match the whole input: the parsed pattern between
/// `\A` and `\z`. Built from the syntax tree, not by wrapping the text, so
/// no pattern text can escape the anchors (an unbalanced `)`, a trailing
/// `#` comment in `(?x)` mode). The anchors take no capture group, so group
/// numbers are the pattern's own. Same syntax options as the `regex` crate's
/// defaults (Unicode, UTF-8 matches only, nesting limit 250).
fn compile_whole(pattern: &str) -> std::result::Result<meta::Regex, String> {
  let hir = syntax::parse_with(pattern, &syntax::Config::new())
    .map_err(|e| format!("invalid regex: {e}"))?;
  let whole = Hir::concat(vec![Hir::look(Look::Start), hir, Hir::look(Look::End)]);
  meta::Builder::new()
    .configure(
      meta::Config::new()
        .nfa_size_limit(Some(SIZE_LIMIT))
        .onepass_size_limit(Some(SIZE_LIMIT))
        .hybrid_cache_capacity(CACHE_LIMIT),
    )
    .build_from_hir(&whole)
    .map_err(|e| {
      // The build error says which stage failed; its source says why
      // (exceeding a size limit, as the `regex` crate's message does).
      let mut message = format!("invalid regex: {e}");
      let mut source = std::error::Error::source(&e);
      while let Some(cause) = source {
        message.push_str(&format!(": {cause}"));
        source = cause.source();
      }
      message
    })
}

/// The pattern: compiled at compose, or a String variable compiled at
/// activation.
pub enum Pattern<R> {
  Const(R),
  Bound(Binding),
}

/// The last variable pattern an instance compiled, with its source.
pub struct LastPattern<R> {
  source: Arc<str>,
  regex: R,
}

fn compose_pattern<R>(
  args: &Args,
  ctx: &mut ComposeCtx<'_>,
  shard: &'static str,
  compile: fn(&str) -> std::result::Result<R, String>,
) -> Result<Pattern<R>> {
  match &Operand::compose_arg(args, "regex", shard, ctx)?.0 {
    Operand::Const(Var::String(source)) => compile(source).map(Pattern::Const).map_err(|e| {
      Error::Diagnostic(Box::new(
        Diagnostic::new(Phase::Compose, "compose-error", "invalid-argument-value", e)
          .shard(shard)
          .param("regex", Some(args.param_index("regex"))),
      ))
    }),
    Operand::Bound(binding) => Ok(Pattern::Bound(*binding)),
    Operand::Const(other) => unreachable!("decoder accepted {other:?} for {shard}.regex"),
  }
}

/// The regex to run: the constant, or the variable's current pattern,
/// compiled unless it is the one this instance compiled last. The flag says
/// whether it was compiled now.
fn resolve<'a, R>(
  pattern: &'a Pattern<R>,
  last: &'a mut Option<LastPattern<R>>,
  ctx: &impl LeafCtx,
  shard: &str,
  compile: fn(&str) -> std::result::Result<R, String>,
) -> Result<(&'a R, bool)> {
  let binding = match pattern {
    Pattern::Const(regex) => return Ok((regex, false)),
    Pattern::Bound(binding) => *binding,
  };
  let source = match &ctx.get(binding) {
    Var::String(source) => Arc::clone(source),
    other => {
      unreachable!("{shard}: the pattern variable was checked to be a String, got {other:?}")
    }
  };
  let fresh = match last {
    Some(cached) => !Arc::ptr_eq(&cached.source, &source) && cached.source != source,
    None => true,
  };
  if fresh {
    let regex = compile(&source).map_err(|e| Error::Activation(format!("{shard}: {e}")))?;
    *last = Some(LastPattern { source, regex });
  }
  Ok((&last.as_ref().expect("compiled above").regex, fresh))
}

fn text(input: &Var) -> &str {
  input
    .as_str()
    .expect("input checked to be a String at compose")
}

// --- Regex.Match ---

pub struct MatchState {
  last: Option<LastPattern<meta::Regex>>,
  /// Reused by every activation; made again when the pattern changes.
  captures: Option<Captures>,
}

pub struct Match;
impl LeafShard for Match {
  type Compiled = Pattern<meta::Regex>;
  type State = MatchState;
  const DESC: ShardDesc = desc(
    "Regex.Match",
    shard_doc!(
      "Matches the whole string against a pattern; outputs the match and its groups, or an empty sequence."
    ),
    shard_doc!(
      "The pattern must match the entire input, as if written between `\\A` and `\\z`. The output is the whole string followed by each capture group in order; a group that did not take part in the match is an empty string, and a repeated group holds its last repetition. No match outputs `[]`. Matching takes time linear in the input: the dialect has no backreferences or lookaround."
    ),
    PATTERN_PARAMS,
    TypeName::Seq,
  );

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Self::Compiled>> {
    Ok(Composed {
      compiled: compose_pattern(args, ctx, Self::DESC.name, compile_whole)?,
      output: Type::seq(Type::string()),
    })
  }

  fn instantiate(_: &Self::Compiled, _: &mut InstanceCtx) -> Result<MatchState> {
    Ok(MatchState {
      last: None,
      captures: None,
    })
  }

  fn activate(
    pattern: &Self::Compiled,
    state: &mut MatchState,
    ctx: &mut impl LeafCtx,
    input: &Var,
  ) -> Result<Flow> {
    let (regex, fresh) = resolve(
      pattern,
      &mut state.last,
      ctx,
      Self::DESC.name,
      compile_whole,
    )?;
    let captures = match &mut state.captures {
      Some(captures) if !fresh => captures,
      slot => slot.insert(regex.create_captures()),
    };
    let text = text(input);
    regex.captures(text, captures);
    if !captures.is_match() {
      return Ok(Flow::Next(Var::seq(Vec::new())));
    }
    let groups = (0..captures.group_len())
      .map(|i| Var::string(captures.get_group(i).map_or("", |span| &text[span])))
      .collect();
    Ok(Flow::Next(Var::seq(groups)))
  }
}

// --- Regex.Search ---

pub struct Search;
impl LeafShard for Search {
  type Compiled = Pattern<Regex>;
  type State = Option<LastPattern<Regex>>;
  const DESC: ShardDesc = desc(
    "Regex.Search",
    shard_doc!("Finds every match of a pattern in a string; outputs each match with its groups."),
    shard_doc!(
      "Matches are found left to right without overlapping; each is a sequence of the match followed by its capture groups (a group that did not take part is an empty string). No match outputs `[]`. An empty match is reported once at its position and the search moves on. `^` and `$` mean the start and end of the whole input (`(?m)` makes them match at lines). Searching takes time linear in the input."
    ),
    PATTERN_PARAMS,
    TypeName::Seq,
  );

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Self::Compiled>> {
    Ok(Composed {
      compiled: compose_pattern(args, ctx, Self::DESC.name, compile_search)?,
      output: Type::seq(Type::seq(Type::string())),
    })
  }

  fn instantiate(_: &Self::Compiled, _: &mut InstanceCtx) -> Result<Self::State> {
    Ok(None)
  }

  fn activate(
    pattern: &Self::Compiled,
    last: &mut Self::State,
    ctx: &mut impl LeafCtx,
    input: &Var,
  ) -> Result<Flow> {
    let (regex, _) = resolve(pattern, last, ctx, Self::DESC.name, compile_search)?;
    let matches = regex
      .captures_iter(text(input))
      .map(|captures| {
        Var::seq(
          captures
            .iter()
            .map(|group| Var::string(group.map_or("", |m| m.as_str())))
            .collect(),
        )
      })
      .collect();
    Ok(Flow::Next(Var::seq(matches)))
  }
}

// --- Regex.Replace ---

pub struct ReplaceCompiled {
  pattern: Pattern<Regex>,
  replacement: Operand,
}

pub struct Replace;
impl LeafShard for Replace {
  type Compiled = ReplaceCompiled;
  type State = Option<LastPattern<Regex>>;
  const DESC: ShardDesc = desc(
    "Regex.Replace",
    shard_doc!("Replaces every match of a pattern in a string."),
    shard_doc!(
      "Matches are found as `Regex.Search` finds them. In the replacement, `$0` is the whole match, `$1` a group by number and `$name` or `${name}` a group by name; a group that did not take part is empty. A reference takes every letter, digit and `_` after the `$`, so `$1x` names a group `1x` (empty): write `${1}x`. `$$` is a literal `$`. A string without a match is output unchanged."
    ),
    REPLACE_PARAMS,
    TypeName::String,
  );

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<ReplaceCompiled>> {
    let pattern = compose_pattern(args, ctx, Self::DESC.name, compile_search)?;
    let (replacement, _) = Operand::compose_arg(args, "replacement", Self::DESC.name, ctx)?;
    Ok(Composed {
      compiled: ReplaceCompiled {
        pattern,
        replacement,
      },
      output: Type::string(),
    })
  }

  fn instantiate(_: &ReplaceCompiled, _: &mut InstanceCtx) -> Result<Self::State> {
    Ok(None)
  }

  fn activate(
    c: &ReplaceCompiled,
    last: &mut Self::State,
    ctx: &mut impl LeafCtx,
    input: &Var,
  ) -> Result<Flow> {
    let (regex, _) = resolve(&c.pattern, last, ctx, Self::DESC.name, compile_search)?;
    let replacement = c.replacement.get(ctx);
    let replacement = replacement
      .as_str()
      .expect("replacement checked to be a String at compose");
    Ok(Flow::Next(
      match regex.replace_all(text(input), replacement) {
        // Nothing matched: the input itself, without copying it.
        Cow::Borrowed(_) => input.clone(),
        Cow::Owned(out) => Var::from_string(Arc::from(out)),
      },
    ))
  }
}
