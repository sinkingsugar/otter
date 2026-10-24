//! Strings, byte strings and encodings (docs/host-requirements.md §3, the
//! text-tool subset first): `String.Join`, `String.Split`,
//! `String.Replace`, `String.Contains`, `String.Starts`, `String.Ends`,
//! `String.Trim`, `String.ToUpper`, `String.ToLower`, `String.EncodeURI`,
//! `String.DecodeURI`, `Slice`, `Bytes.Join`, `ToBase64`, `FromBase64`,
//! `ToLEB128`, `FromLEB128`, `ToJson`, `FromJson`, the type tests
//! (`IsInt`, `IsFloat`, `IsBool`, `IsString`, `IsBytes`, `IsSeq`,
//! `IsTable`), `Fail` and `Sort`, then `String.CodePoints`,
//! `String.FromCodePoints`, `String.CleanUTF8`, `HexToBytes`,
//! `BytesToInts`, `IntsToBytes`, `String.PadStart` and `String.PadEnd`.
//! All leaf shards. The first group does not run at compose time (not on
//! the audited list in `compose_time.rs`, its work not charged to the
//! meter); the second does, charged by size.
//!
//! Strings are measured in characters, as `Count` does: `Slice` on a
//! string takes characters, never a part of one.

use std::sync::Arc;

use super::leaf::{LeafShard, leaf_type};
use super::values::{Pure, PureOp, fail, meter_bytes, mismatch, one_of, pure_desc};
use super::*;
use crate::compose_time::Meter;
use crate::instance::{InstanceCtx, LeafCtx};
use crate::shard::Flow;

pub static STRING_JOIN: ShardType = leaf_type::<Join>();
pub static STRING_SPLIT: ShardType = leaf_type::<Split>();
pub static STRING_REPLACE: ShardType = leaf_type::<Replace>();
pub static STRING_CONTAINS: ShardType = leaf_type::<TextProbe<ContainsK>>();
pub static STRING_STARTS: ShardType = leaf_type::<TextProbe<StartsK>>();
pub static STRING_ENDS: ShardType = leaf_type::<TextProbe<EndsK>>();
pub static STRING_TRIM: ShardType = leaf_type::<Pure<TrimOp>>();
pub static STRING_TO_UPPER: ShardType = leaf_type::<Pure<ToUpperOp>>();
pub static STRING_TO_LOWER: ShardType = leaf_type::<Pure<ToLowerOp>>();
pub static STRING_ENCODE_URI: ShardType = leaf_type::<Pure<EncodeUriOp>>();
pub static STRING_DECODE_URI: ShardType = leaf_type::<Pure<DecodeUriOp>>();
pub static SLICE: ShardType = leaf_type::<Slice>();
pub static BYTES_JOIN: ShardType = leaf_type::<Pure<BytesJoinOp>>();
pub static TO_BASE64: ShardType = leaf_type::<Pure<ToBase64Op>>();
pub static FROM_BASE64: ShardType = leaf_type::<Pure<FromBase64Op>>();
pub static TO_LEB128: ShardType = leaf_type::<Leb128<ToLeb>>();
pub static FROM_LEB128: ShardType = leaf_type::<Leb128<FromLeb>>();
pub static TO_JSON: ShardType = leaf_type::<ToJson>();
pub static FROM_JSON: ShardType = leaf_type::<Pure<FromJsonOp>>();
pub static IS_INT: ShardType = leaf_type::<Pure<IsType<IsIntK>>>();
pub static IS_FLOAT: ShardType = leaf_type::<Pure<IsType<IsFloatK>>>();
pub static IS_BOOL: ShardType = leaf_type::<Pure<IsType<IsBoolK>>>();
pub static IS_STRING: ShardType = leaf_type::<Pure<IsType<IsStringK>>>();
pub static IS_BYTES: ShardType = leaf_type::<Pure<IsType<IsBytesK>>>();
pub static IS_SEQ: ShardType = leaf_type::<Pure<IsType<IsSeqK>>>();
pub static IS_TABLE: ShardType = leaf_type::<Pure<IsType<IsTableK>>>();
pub static FAIL: ShardType = leaf_type::<Fail>();
pub static SORT: ShardType = leaf_type::<Sort>();
pub static STRING_CODE_POINTS: ShardType = leaf_type::<Pure<CodePointsOp>>();
pub static STRING_FROM_CODE_POINTS: ShardType = leaf_type::<Pure<FromCodePointsOp>>();
pub static STRING_CLEAN_UTF8: ShardType = leaf_type::<Pure<CleanUtf8Op>>();
pub static HEX_TO_BYTES: ShardType = leaf_type::<Pure<HexToBytesOp>>();
pub static BYTES_TO_INTS: ShardType = leaf_type::<Pure<BytesToIntsOp>>();
pub static INTS_TO_BYTES: ShardType = leaf_type::<Pure<IntsToBytesOp>>();
pub static STRING_PAD_START: ShardType = leaf_type::<Pad<false>>();
pub static STRING_PAD_END: ShardType = leaf_type::<Pad<true>>();

const STRING: &[TypeName] = &[TypeName::String];
const SEQ: &[TypeName] = &[TypeName::Seq];
const TEXT: Forms = Forms::LITERAL.or(Forms::VARIABLE);

const fn desc(
  name: &'static str,
  summary: &'static str,
  help: &'static str,
  params: &'static [ParamDecl],
  input: InputDesc,
  output: OutputDesc,
) -> ShardDesc {
  ShardDesc {
    name,
    version: 1,
    summary,
    help,
    params: Params::Declared(params),
    input,
    output,
    targets: Targets::All,
    aliases: &[],
    effects: crate::signature::Effects::NONE,
    lifetime: crate::signature::Lifetime::Stateless,
  }
}

/// The input type, when it is one of `accepted` (not `Any`: narrow it with
/// an `Expect` first).
fn input_one_of(
  ctx: &ComposeCtx<'_>,
  name: &'static str,
  accepted: &'static [TypeName],
) -> Result<Type> {
  let input = ctx.input();
  one_of(input, accepted, input).map_err(|expected| mismatch(name, input, expected))
}

/// The string an operand holds at activation (declared String, so a
/// variable was checked at compose).
fn text_of(op: &Operand, ctx: &impl LeafCtx) -> Arc<str> {
  match &op.get(ctx) {
    Var::String(s) => Arc::clone(s),
    other => Arc::from(other.text()),
  }
}

macro_rules! no_state {
  ($compiled:ty) => {
    type State = ();
    fn instantiate(_: &$compiled, _: &mut InstanceCtx) -> Result<()> {
      Ok(())
    }
  };
}

// --- String.Join ---

static JOIN_PARAMS: &[ParamDecl] = &[decl(
  "separator",
  crate::shard_doc!("Text put between the elements: a String literal or variable."),
  TEXT,
  STRING,
  Requirement::Default(DefaultValue::Str("")),
)];

pub struct Join;
impl LeafShard for Join {
  type Compiled = Operand;
  no_state!(Operand);
  const DESC: ShardDesc = desc(
    "String.Join",
    crate::shard_doc!("Joins a sequence of strings into one, with a separator between them."),
    crate::shard_doc!(
      "Every element must be a String (an activation error otherwise); `String.Format` writes any value as text instead."
    ),
    JOIN_PARAMS,
    InputDesc::Types(SEQ),
    OutputDesc::Fixed(TypeName::String),
  );
  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Operand>> {
    input_one_of(ctx, Self::DESC.name, SEQ)?;
    let (separator, _) = Operand::compose_arg(args, "separator", Self::DESC.name, ctx)?;
    Ok(Composed {
      compiled: separator,
      output: Type::string(),
    })
  }
  fn activate(sep: &Operand, _: &mut (), ctx: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    let sep = text_of(sep, ctx);
    let items = input.as_seq().unwrap_or(&[]);
    let mut out = String::new();
    for (i, item) in items.iter().enumerate() {
      let Some(s) = item.as_str() else {
        return Err(fail(
          "String.Join",
          &format!("element {i} is {}, not a String", item.type_of()),
        ));
      };
      if i > 0 {
        out.push_str(&sep);
      }
      out.push_str(s);
    }
    Ok(Flow::Next(Var::from_string(Arc::from(out))))
  }
}

// --- String.Split ---

static SPLIT_PARAMS: &[ParamDecl] = &[
  decl(
    "separator",
    crate::shard_doc!("The text to split at: a non-empty String literal or variable."),
    TEXT,
    STRING,
    Requirement::Required,
  ),
  decl(
    "keep-separator",
    crate::shard_doc!("Whether each part but the last keeps the separator at its end."),
    Forms::LITERAL,
    &[TypeName::Bool],
    Requirement::Default(DefaultValue::Bool(false)),
  ),
];

pub struct SplitCompiled {
  separator: Operand,
  keep: bool,
}

pub struct Split;
impl LeafShard for Split {
  type Compiled = SplitCompiled;
  no_state!(SplitCompiled);
  const DESC: ShardDesc = desc(
    "String.Split",
    crate::shard_doc!("Splits a string at every occurrence of a separator."),
    crate::shard_doc!(
      "Outputs the parts in order, empty ones included (`\"a,,b\"` split at `,` is `[\"a\" \"\" \"b\"]`); a string without the separator is one part. An empty separator is an error."
    ),
    SPLIT_PARAMS,
    InputDesc::Types(STRING),
    OutputDesc::Fixed(TypeName::Seq),
  );
  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<SplitCompiled>> {
    input_one_of(ctx, Self::DESC.name, STRING)?;
    let (separator, _) = Operand::compose_arg(args, "separator", Self::DESC.name, ctx)?;
    if let Operand::Const(Var::String(s)) = &separator
      && s.is_empty()
    {
      return Err(param_error(
        args,
        Self::DESC.name,
        "separator",
        "compose-error",
        "invalid-argument-value",
        "the separator must not be empty".into(),
      ));
    }
    Ok(Composed {
      compiled: SplitCompiled {
        separator,
        keep: args.bool("keep-separator").unwrap_or(false),
      },
      output: Type::seq(Type::string()),
    })
  }
  fn activate(c: &SplitCompiled, _: &mut (), ctx: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    let sep = text_of(&c.separator, ctx);
    if sep.is_empty() {
      return Err(fail("String.Split", "the separator is empty"));
    }
    let text = input.as_str().unwrap_or("");
    let parts: Vec<Var> = if c.keep {
      text.split_inclusive(&*sep).map(Var::string).collect()
    } else {
      text.split(&*sep).map(Var::string).collect()
    };
    // `split_inclusive` yields nothing for an empty string; `split` yields
    // one empty part. Keep them the same.
    let parts = if parts.is_empty() {
      vec![Var::string("")]
    } else {
      parts
    };
    Ok(Flow::Next(Var::seq(parts)))
  }
}

// --- String.Replace ---

static REPLACE_PARAMS: &[ParamDecl] = &[
  decl(
    "find",
    crate::shard_doc!("The text to replace: a non-empty String literal or variable."),
    TEXT,
    STRING,
    Requirement::Required,
  ),
  decl(
    "replacement",
    crate::shard_doc!("What replaces each occurrence: a String literal or variable."),
    TEXT,
    STRING,
    Requirement::Required,
  ),
];

pub struct ReplaceCompiled {
  find: Operand,
  replacement: Operand,
}

pub struct Replace;
impl LeafShard for Replace {
  type Compiled = ReplaceCompiled;
  no_state!(ReplaceCompiled);
  const DESC: ShardDesc = desc(
    "String.Replace",
    crate::shard_doc!("Replaces every occurrence of a text in a string."),
    crate::shard_doc!(
      "Plain text, not a pattern; occurrences are found left to right without overlapping. An empty `find` is an error."
    ),
    REPLACE_PARAMS,
    InputDesc::Types(STRING),
    OutputDesc::Fixed(TypeName::String),
  );
  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<ReplaceCompiled>> {
    input_one_of(ctx, Self::DESC.name, STRING)?;
    let (find, _) = Operand::compose_arg(args, "find", Self::DESC.name, ctx)?;
    let (replacement, _) = Operand::compose_arg(args, "replacement", Self::DESC.name, ctx)?;
    if let Operand::Const(Var::String(s)) = &find
      && s.is_empty()
    {
      return Err(param_error(
        args,
        Self::DESC.name,
        "find",
        "compose-error",
        "invalid-argument-value",
        "the text to find must not be empty".into(),
      ));
    }
    Ok(Composed {
      compiled: ReplaceCompiled { find, replacement },
      output: Type::string(),
    })
  }
  fn activate(
    c: &ReplaceCompiled,
    _: &mut (),
    ctx: &mut impl LeafCtx,
    input: &Var,
  ) -> Result<Flow> {
    let find = text_of(&c.find, ctx);
    if find.is_empty() {
      return Err(fail("String.Replace", "the text to find is empty"));
    }
    let replacement = text_of(&c.replacement, ctx);
    let text = input.as_str().unwrap_or("");
    Ok(Flow::Next(Var::from_string(Arc::from(
      text.replace(&*find, &replacement),
    ))))
  }
}

// --- String.Contains, String.Starts, String.Ends ---

/// One substring test.
pub trait ProbeKind: 'static {
  const DESC: ShardDesc;
  fn test(text: &str, part: &str) -> bool;
}

static PROBE_PARAMS: &[ParamDecl] = &[decl(
  "text",
  crate::shard_doc!("The text to look for: a String literal or variable."),
  TEXT,
  STRING,
  Requirement::Required,
)];

pub struct ContainsK;
impl ProbeKind for ContainsK {
  const DESC: ShardDesc = desc(
    "String.Contains",
    crate::shard_doc!("Outputs whether the input string contains a text."),
    "",
    PROBE_PARAMS,
    InputDesc::Types(STRING),
    OutputDesc::Fixed(TypeName::Bool),
  );
  fn test(text: &str, part: &str) -> bool {
    text.contains(part)
  }
}

pub struct StartsK;
impl ProbeKind for StartsK {
  const DESC: ShardDesc = desc(
    "String.Starts",
    crate::shard_doc!("Outputs whether the input string starts with a text."),
    "",
    PROBE_PARAMS,
    InputDesc::Types(STRING),
    OutputDesc::Fixed(TypeName::Bool),
  );
  fn test(text: &str, part: &str) -> bool {
    text.starts_with(part)
  }
}

pub struct EndsK;
impl ProbeKind for EndsK {
  const DESC: ShardDesc = desc(
    "String.Ends",
    crate::shard_doc!("Outputs whether the input string ends with a text."),
    "",
    PROBE_PARAMS,
    InputDesc::Types(STRING),
    OutputDesc::Fixed(TypeName::Bool),
  );
  fn test(text: &str, part: &str) -> bool {
    text.ends_with(part)
  }
}

pub struct TextProbe<K>(std::marker::PhantomData<K>);
impl<K: ProbeKind> LeafShard for TextProbe<K> {
  type Compiled = Operand;
  no_state!(Operand);
  const DESC: ShardDesc = K::DESC;
  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Operand>> {
    input_one_of(ctx, K::DESC.name, STRING)?;
    let (part, _) = Operand::compose_arg(args, "text", K::DESC.name, ctx)?;
    Ok(Composed {
      compiled: part,
      output: Type::bool(),
    })
  }
  fn activate(part: &Operand, _: &mut (), ctx: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    let part = text_of(part, ctx);
    Ok(Flow::Next(Var::Bool(K::test(
      input.as_str().unwrap_or(""),
      &part,
    ))))
  }
}

// --- String.Trim, String.ToUpper, String.ToLower ---

macro_rules! string_op {
  ($op:ident, $name:literal, $summary:literal, $help:literal, |$s:ident| $body:expr) => {
    pub struct $op;
    impl PureOp for $op {
      const DESC: ShardDesc = pure_desc(
        $name,
        crate::shard_doc!($summary),
        crate::shard_doc!($help),
        InputDesc::Types(STRING),
        OutputDesc::Fixed(TypeName::String),
      );
      fn output(input: Type) -> std::result::Result<Type, &'static [TypeName]> {
        one_of(input, STRING, Type::string())
      }
      fn apply(input: &Var) -> Result<Var> {
        let $s: &str = input
          .as_str()
          .ok_or_else(|| fail($name, "input is not a string"))?;
        $body
      }
    }
  };
}

string_op!(
  TrimOp,
  "String.Trim",
  "Removes whitespace from both ends of a string.",
  "Whitespace as Unicode defines it.",
  |s| Ok(Var::string(s.trim()))
);
string_op!(
  ToUpperOp,
  "String.ToUpper",
  "Converts a string to upper case.",
  "Unicode case mapping (`ß` becomes `SS`).",
  |s| Ok(Var::from_string(Arc::from(s.to_uppercase())))
);
string_op!(
  ToLowerOp,
  "String.ToLower",
  "Converts a string to lower case.",
  "Unicode case mapping.",
  |s| Ok(Var::from_string(Arc::from(s.to_lowercase())))
);
string_op!(
  EncodeUriOp,
  "String.EncodeURI",
  "Percent-encodes a string for use in a URL component.",
  "Every byte of the UTF-8 text except the unreserved characters (`A`-`Z`, `a`-`z`, `0`-`9`, `-`, `_`, `.`, `~`) becomes `%XX`, as 1.x does: a query value or a path segment.",
  |s| Ok(Var::from_string(Arc::from(encode_uri(s))))
);
string_op!(
  DecodeUriOp,
  "String.DecodeURI",
  "Decodes a percent-encoded string.",
  "`%XX` becomes the byte it names and `+` stays `+`; the result must be UTF-8, and a `%` not followed by two hex digits is an error.",
  |s| decode_uri(s)
    .map(|d| Var::from_string(Arc::from(d)))
    .map_err(|e| fail("String.DecodeURI", &e))
);

/// Percent-encodes everything but the unreserved characters (RFC 3986).
pub fn encode_uri(s: &str) -> String {
  const HEX: &[u8; 16] = b"0123456789ABCDEF";
  let mut out = String::with_capacity(s.len());
  for &b in s.as_bytes() {
    if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
      out.push(b as char);
    } else {
      out.push('%');
      out.push(HEX[usize::from(b >> 4)] as char);
      out.push(HEX[usize::from(b & 15)] as char);
    }
  }
  out
}

fn decode_uri(s: &str) -> std::result::Result<String, String> {
  let bytes = s.as_bytes();
  let mut out = Vec::with_capacity(bytes.len());
  let mut i = 0;
  while i < bytes.len() {
    if bytes[i] == b'%' {
      let hex = bytes
        .get(i + 1..i + 3)
        .filter(|h| h.iter().all(u8::is_ascii_hexdigit))
        .and_then(|h| std::str::from_utf8(h).ok())
        .and_then(|h| u8::from_str_radix(h, 16).ok());
      match hex {
        Some(b) => out.push(b),
        None => return Err(format!("invalid escape at byte {i}")),
      }
      i += 3;
    } else {
      out.push(bytes[i]);
      i += 1;
    }
  }
  String::from_utf8(out).map_err(|_| "the decoded bytes are not UTF-8".to_string())
}

// --- Slice ---

const SLICED: &[TypeName] = &[TypeName::String, TypeName::Seq, TypeName::Bytes];

static SLICE_PARAMS: &[ParamDecl] = &[
  decl(
    "from",
    crate::shard_doc!(
      "The first index taken; negative counts from the end (`-1` is the last element)."
    ),
    Forms::LITERAL.or(Forms::VARIABLE),
    &[TypeName::Int],
    Requirement::Default(DefaultValue::Int(0)),
  ),
  decl(
    "to",
    crate::shard_doc!(
      "The index after the last one taken; negative counts from the end. Omitted: to the end."
    ),
    Forms::LITERAL.or(Forms::VARIABLE),
    &[TypeName::Int],
    Requirement::Optional,
  ),
];

pub struct SliceCompiled {
  from: Operand,
  to: Option<Operand>,
}

pub struct Slice;
impl LeafShard for Slice {
  type Compiled = SliceCompiled;
  no_state!(SliceCompiled);
  const DESC: ShardDesc = desc(
    "Slice",
    crate::shard_doc!("Outputs part of a string, sequence or byte string."),
    crate::shard_doc!(
      "Takes the elements (characters of a string, bytes of a byte string) from index `from` up to but not including `to`. Negative indices count from the end; indices beyond either end are clamped, so a slice past the end is empty, never an error."
    ),
    SLICE_PARAMS,
    InputDesc::Types(SLICED),
    OutputDesc::SameAsInput,
  );
  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<SliceCompiled>> {
    let input = input_one_of(ctx, Self::DESC.name, SLICED)?;
    let (from, _) = Operand::compose_arg(args, "from", Self::DESC.name, ctx)?;
    let to = Operand::compose_optional_arg(args, "to", Self::DESC.name, ctx)?.map(|(op, _)| op);
    Ok(Composed {
      compiled: SliceCompiled { from, to },
      output: input,
    })
  }
  fn activate(c: &SliceCompiled, _: &mut (), ctx: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    let int = |op: &Operand| match op.get(ctx) {
      Var::Int(i) => Ok(i),
      other => Err(fail(
        "Slice",
        &format!("index is {}, not an Int", other.type_of()),
      )),
    };
    let from = int(&c.from)?;
    let to = match &c.to {
      Some(op) => Some(int(op)?),
      None => None,
    };
    let range = |len: usize| {
      let at = |i: i64| -> usize {
        if i < 0 {
          len.saturating_sub(i.unsigned_abs().try_into().unwrap_or(usize::MAX))
        } else {
          usize::try_from(i).unwrap_or(usize::MAX).min(len)
        }
      };
      let start = at(from);
      let end = to.map_or(len, at).max(start);
      start..end
    };
    Ok(Flow::Next(match input {
      Var::String(s) => {
        let chars = s.chars().count();
        let r = range(chars);
        Var::from_string(Arc::from(
          s.chars()
            .skip(r.start)
            .take(r.end - r.start)
            .collect::<String>(),
        ))
      }
      Var::Seq(items) => Var::seq(items[range(items.len())].to_vec()),
      Var::Bytes(b) => Var::bytes(&b[range(b.len())]),
      _ => return Err(fail("Slice", "input type mismatch")),
    }))
  }
}

// --- Bytes.Join ---

pub struct BytesJoinOp;
impl PureOp for BytesJoinOp {
  const DESC: ShardDesc = pure_desc(
    "Bytes.Join",
    crate::shard_doc!("Concatenates a sequence of byte strings into one."),
    crate::shard_doc!(
      "Every element must be a byte string (an activation error otherwise); convert text with `StringToBytes`."
    ),
    InputDesc::Types(SEQ),
    OutputDesc::Fixed(TypeName::Bytes),
  );
  fn output(input: Type) -> std::result::Result<Type, &'static [TypeName]> {
    one_of(input, SEQ, Type::bytes())
  }
  fn apply(input: &Var) -> Result<Var> {
    let items = input.as_seq().unwrap_or(&[]);
    let mut out = Vec::new();
    for (i, item) in items.iter().enumerate() {
      let Some(b) = item.as_bytes() else {
        return Err(fail(
          "Bytes.Join",
          &format!("element {i} is {}, not Bytes", item.type_of()),
        ));
      };
      out.extend_from_slice(b);
    }
    Ok(Var::from_bytes(Arc::from(out)))
  }
}

// --- ToBase64, FromBase64 ---

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const ENCODED: &[TypeName] = &[TypeName::Bytes, TypeName::String];

/// Standard base64 (RFC 4648 §4) with padding.
pub fn base64_encode(bytes: &[u8]) -> String {
  let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
  for chunk in bytes.chunks(3) {
    let n = match chunk {
      [a, b, c] => u32::from(*a) << 16 | u32::from(*b) << 8 | u32::from(*c),
      [a, b] => u32::from(*a) << 16 | u32::from(*b) << 8,
      [a] => u32::from(*a) << 16,
      _ => unreachable!(),
    };
    for i in 0..4 {
      if i <= chunk.len() {
        out.push(BASE64[(n >> (18 - 6 * i) & 63) as usize] as char);
      } else {
        out.push('=');
      }
    }
  }
  out
}

/// Decodes standard or URL-safe base64; padding is optional but, when
/// present, completes the last group of four, and the unused bits of the
/// last digit are zero (RFC 4648 §3.5). Whitespace is not allowed.
pub fn base64_decode(text: &str) -> std::result::Result<Vec<u8>, String> {
  let digits = text.trim_end_matches('=');
  let padding = text.len() - digits.len();
  if padding > 2 || (padding > 0 && !text.len().is_multiple_of(4)) {
    return Err("wrong padding".into());
  }
  let mut out = Vec::with_capacity(digits.len() * 3 / 4);
  let mut acc = 0u32;
  let mut bits = 0;
  for (i, c) in digits.bytes().enumerate() {
    let v = match c {
      b'A'..=b'Z' => c - b'A',
      b'a'..=b'z' => c - b'a' + 26,
      b'0'..=b'9' => c - b'0' + 52,
      b'+' | b'-' => 62,
      b'/' | b'_' => 63,
      _ => return Err(format!("invalid character at {i}")),
    };
    acc = acc << 6 | u32::from(v);
    bits += 6;
    if bits >= 8 {
      bits -= 8;
      out.push((acc >> bits) as u8);
      acc &= (1 << bits) - 1;
    }
  }
  if digits.len() % 4 == 1 {
    return Err("truncated input".into());
  }
  if acc != 0 {
    return Err("the last digit has bits set past the data".into());
  }
  Ok(out)
}

pub struct ToBase64Op;
impl PureOp for ToBase64Op {
  const DESC: ShardDesc = pure_desc(
    "ToBase64",
    crate::shard_doc!("Encodes a byte string (or a string's UTF-8 bytes) as base64."),
    crate::shard_doc!("The standard alphabet with `+`, `/` and `=` padding (RFC 4648 §4)."),
    InputDesc::Types(ENCODED),
    OutputDesc::Fixed(TypeName::String),
  );
  fn output(input: Type) -> std::result::Result<Type, &'static [TypeName]> {
    one_of(input, ENCODED, Type::string())
  }
  fn apply(input: &Var) -> Result<Var> {
    let bytes = match input {
      Var::Bytes(b) => &b[..],
      Var::String(s) => s.as_bytes(),
      _ => return Err(fail("ToBase64", "input type mismatch")),
    };
    Ok(Var::from_string(Arc::from(base64_encode(bytes))))
  }
}

pub struct FromBase64Op;
impl PureOp for FromBase64Op {
  const DESC: ShardDesc = pure_desc(
    "FromBase64",
    crate::shard_doc!("Decodes base64 text into a byte string."),
    crate::shard_doc!(
      "Reads the standard and the URL-safe alphabets; padding is optional. Other characters are an activation error."
    ),
    InputDesc::Types(STRING),
    OutputDesc::Fixed(TypeName::Bytes),
  );
  fn output(input: Type) -> std::result::Result<Type, &'static [TypeName]> {
    one_of(input, STRING, Type::bytes())
  }
  fn apply(input: &Var) -> Result<Var> {
    let text = input.as_str().unwrap_or("");
    base64_decode(text)
      .map(|b| Var::from_bytes(Arc::from(b)))
      .map_err(|e| fail("FromBase64", &e))
  }
}

// --- ToLEB128, FromLEB128 ---

static LEB_PARAMS: &[ParamDecl] = &[decl(
  "signed",
  crate::shard_doc!(
    "Signed LEB128 (negative numbers allowed) instead of unsigned (protobuf varints)."
  ),
  Forms::LITERAL,
  &[TypeName::Bool],
  Requirement::Default(DefaultValue::Bool(false)),
)];

/// One direction of LEB128.
pub trait LebDirection: 'static {
  const DESC: ShardDesc;
  const INPUT: &'static [TypeName];
  fn output() -> Type;
  fn apply(signed: bool, input: &Var) -> Result<Var>;
}

pub struct ToLeb;
impl LebDirection for ToLeb {
  const DESC: ShardDesc = desc(
    "ToLEB128",
    crate::shard_doc!("Encodes an Int as LEB128 bytes (a protobuf varint by default)."),
    crate::shard_doc!(
      "Unsigned by default, as in 1.x: a negative Int is then an activation error. With `signed: true`, signed LEB128."
    ),
    LEB_PARAMS,
    InputDesc::Types(&[TypeName::Int]),
    OutputDesc::Fixed(TypeName::Bytes),
  );
  const INPUT: &'static [TypeName] = &[TypeName::Int];
  fn output() -> Type {
    Type::bytes()
  }
  fn apply(signed: bool, input: &Var) -> Result<Var> {
    let Var::Int(i) = *input else {
      return Err(fail("ToLEB128", "input type mismatch"));
    };
    let mut out = Vec::with_capacity(10);
    if signed {
      let mut v = i;
      loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        let done = (v == 0 && byte & 0x40 == 0) || (v == -1 && byte & 0x40 != 0);
        out.push(if done { byte } else { byte | 0x80 });
        if done {
          break;
        }
      }
    } else {
      let Ok(mut v) = u64::try_from(i) else {
        return Err(fail(
          "ToLEB128",
          &format!("{i} is negative; use `signed: true`"),
        ));
      };
      loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        out.push(if v == 0 { byte } else { byte | 0x80 });
        if v == 0 {
          break;
        }
      }
    }
    Ok(Var::from_bytes(Arc::from(out)))
  }
}

pub struct FromLeb;
impl LebDirection for FromLeb {
  const DESC: ShardDesc = desc(
    "FromLEB128",
    crate::shard_doc!("Decodes LEB128 bytes into an Int."),
    crate::shard_doc!(
      "The byte string must hold exactly one value; one that does not fit in an Int, or trailing bytes, are an activation error."
    ),
    LEB_PARAMS,
    InputDesc::Types(&[TypeName::Bytes]),
    OutputDesc::Fixed(TypeName::Int),
  );
  const INPUT: &'static [TypeName] = &[TypeName::Bytes];
  fn output() -> Type {
    Type::int()
  }
  fn apply(signed: bool, input: &Var) -> Result<Var> {
    let bytes = input.as_bytes().unwrap_or(&[]);
    let mut value: u64 = 0;
    let mut shift = 0u32;
    for (i, &b) in bytes.iter().enumerate() {
      // The tenth byte holds bit 63 alone: 0 or 1 unsigned (1 fails the
      // Int check below), and for signed only the sign extension of bit
      // 63, `0x00` or `0x7f`.
      let last_ok = if signed {
        b == 0 || b == 0x7f
      } else {
        b & 0x7e == 0
      };
      if shift >= 64 || (shift == 63 && !last_ok) {
        return Err(fail("FromLEB128", "the value does not fit in an Int"));
      }
      value |= u64::from(b & 0x7f) << shift;
      shift += 7;
      if b & 0x80 == 0 {
        if i + 1 != bytes.len() {
          return Err(fail("FromLEB128", "trailing bytes after the value"));
        }
        if signed {
          if shift < 64 && b & 0x40 != 0 {
            value |= u64::MAX << shift;
          }
          return Ok(Var::Int(value as i64));
        }
        return i64::try_from(value)
          .map(Var::Int)
          .map_err(|_| fail("FromLEB128", "the value does not fit in an Int"));
      }
    }
    Err(fail("FromLEB128", "truncated value"))
  }
}

pub struct Leb128<D>(std::marker::PhantomData<D>);
impl<D: LebDirection> LeafShard for Leb128<D> {
  type Compiled = bool;
  no_state!(bool);
  const DESC: ShardDesc = D::DESC;
  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<bool>> {
    input_one_of(ctx, D::DESC.name, D::INPUT)?;
    Ok(Composed {
      compiled: args.bool("signed").unwrap_or(false),
      output: D::output(),
    })
  }
  fn activate(signed: &bool, _: &mut (), _: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    D::apply(*signed, input).map(Flow::Next)
  }
}

// --- ToJson, FromJson ---

static TO_JSON_PARAMS: &[ParamDecl] = &[decl(
  "indent",
  crate::shard_doc!("Spaces per nesting level; 0 writes compact JSON on one line."),
  Forms::LITERAL,
  &[TypeName::Int],
  Requirement::Default(DefaultValue::Int(0)),
)];

pub struct ToJson;
impl LeafShard for ToJson {
  type Compiled = usize;
  no_state!(usize);
  const DESC: ShardDesc = desc(
    "ToJson",
    crate::shard_doc!("Writes a value as JSON text."),
    crate::shard_doc!(
      "none is `null`, tables are objects (keys in sorted order), sequences and float vectors are arrays; an Int is written as an integer and a Float always with a fraction or exponent (`2.0`), so the text reads back as the same value. NaN, infinities, byte strings and objects have no JSON form (an activation error)."
    ),
    TO_JSON_PARAMS,
    InputDesc::Any,
    OutputDesc::Fixed(TypeName::String),
  );
  fn compose(args: &Args, _: &mut ComposeCtx<'_>) -> Result<Composed<usize>> {
    let indent = args.int("indent").unwrap_or(0);
    let Ok(indent) = usize::try_from(indent) else {
      return Err(param_error(
        args,
        Self::DESC.name,
        "indent",
        "compose-error",
        "invalid-argument-value",
        format!("indent must not be negative, got {indent}"),
      ));
    };
    Ok(Composed {
      compiled: indent.min(16),
      output: Type::string(),
    })
  }
  fn activate(indent: &usize, _: &mut (), _: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    let text = if *indent == 0 {
      crate::json::write(input)
    } else {
      crate::json::write_pretty(input, *indent)
    };
    text
      .map(|t| Flow::Next(Var::from_string(Arc::from(t))))
      .map_err(|e| fail("ToJson", &e))
  }
}

pub struct FromJsonOp;
impl PureOp for FromJsonOp {
  const DESC: ShardDesc = pure_desc(
    "FromJson",
    crate::shard_doc!("Parses JSON text into a value."),
    crate::shard_doc!(
      "`null` is none, objects are tables, arrays are sequences; a number without a fraction or exponent that fits is an Int, any other a Float. The output is Any: narrow it with `ExpectTable`, `ExpectSeq` and the like. Malformed text is an activation error naming the byte offset."
    ),
    InputDesc::Types(STRING),
    OutputDesc::Fixed(TypeName::Any),
  );
  fn output(input: Type) -> std::result::Result<Type, &'static [TypeName]> {
    one_of(input, STRING, Type::any())
  }
  fn apply(input: &Var) -> Result<Var> {
    crate::json::parse(input.as_str().unwrap_or("")).map_err(|e| fail("FromJson", &e))
  }
}

// --- IsInt, IsFloat, ... ---

/// The type an `Is*` test checks.
pub trait IsKind: 'static {
  const NAME: &'static str;
  const TARGET: TypeName;
}

macro_rules! is_kind {
  ($kind:ident, $name:literal, $target:ident) => {
    pub struct $kind;
    impl IsKind for $kind {
      const NAME: &'static str = $name;
      const TARGET: TypeName = TypeName::$target;
    }
  };
}

is_kind!(IsIntK, "IsInt", Int);
is_kind!(IsFloatK, "IsFloat", Float);
is_kind!(IsBoolK, "IsBool", Bool);
is_kind!(IsStringK, "IsString", String);
is_kind!(IsBytesK, "IsBytes", Bytes);
is_kind!(IsSeqK, "IsSeq", Seq);
is_kind!(IsTableK, "IsTable", Table);

pub struct IsType<K>(std::marker::PhantomData<K>);
impl<K: IsKind> PureOp for IsType<K> {
  const DESC: ShardDesc = pure_desc(
    K::NAME,
    crate::shard_doc!("Outputs whether the input has one type."),
    crate::shard_doc!(
      "Checks the value, so it works on Any and unions (what `FromJson` and open tables give); pair it with the matching `Expect` shard to narrow: `If({IsInt} {ExpectInt} {0})`."
    ),
    InputDesc::Any,
    OutputDesc::Fixed(TypeName::Bool),
  );
  fn output(_: Type) -> std::result::Result<Type, &'static [TypeName]> {
    Ok(Type::bool())
  }
  fn apply(input: &Var) -> Result<Var> {
    Ok(Var::Bool(K::TARGET.to_type().admits(input)))
  }
}

// --- Fail ---

pub struct Fail;
impl LeafShard for Fail {
  type Compiled = ();
  no_state!(());
  const DESC: ShardDesc = desc(
    "Fail",
    crate::shard_doc!("Fails with the input string as the error message."),
    crate::shard_doc!(
      "An activation error: `Maybe` catches it, otherwise it ends the instance (a host sees the message)."
    ),
    &[],
    InputDesc::Types(STRING),
    OutputDesc::Dynamic(crate::shard_doc!("none: it never produces a value")),
  );
  fn compose(_: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<()>> {
    input_one_of(ctx, Self::DESC.name, STRING)?;
    Ok(Composed {
      compiled: (),
      output: Type::never(),
    })
  }
  fn activate(_: &(), _: &mut (), _: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    Err(Error::Activation(input.text()))
  }
}

// --- Sort ---

static SORT_PARAMS: &[ParamDecl] = &[decl(
  "desc",
  crate::shard_doc!("Largest first."),
  Forms::LITERAL,
  &[TypeName::Bool],
  Requirement::Default(DefaultValue::Bool(false)),
)];

/// The order `Sort` uses: numbers by value (Int and Float mixed), strings
/// by their bytes, Bools false first, sequences element by element
/// (shorter first on a tie). Other kinds, NaN, and mixing kinds have no
/// order.
pub fn sort_order(a: &Var, b: &Var) -> Result<std::cmp::Ordering> {
  match (a, b) {
    (Var::Int(_) | Var::Float(_), Var::Int(_) | Var::Float(_)) => compare(a, b.clone()),
    (Var::String(x), Var::String(y)) => Ok(x.cmp(y)),
    (Var::Bool(x), Var::Bool(y)) => Ok(x.cmp(y)),
    (Var::Seq(x), Var::Seq(y)) => {
      for (p, q) in x.iter().zip(y.iter()) {
        let o = sort_order(p, q)?;
        if o.is_ne() {
          return Ok(o);
        }
      }
      Ok(x.len().cmp(&y.len()))
    }
    _ => Err(fail(
      "Sort",
      &format!("cannot order {} against {}", a.type_of(), b.type_of()),
    )),
  }
}

/// `sort_order` made total, so the sort's comparator is consistent even
/// over values with no order (Rust's sort may panic otherwise): kinds rank
/// numbers, strings, Bools, sequences, then the rest, all equal; NaN sorts
/// after every number. The first unordered pair compared fills `error`.
fn total_order(a: &Var, b: &Var, error: &mut Option<Error>) -> std::cmp::Ordering {
  fn rank(v: &Var) -> u8 {
    match v {
      Var::Int(_) | Var::Float(_) => 0,
      Var::String(_) => 1,
      Var::Bool(_) => 2,
      Var::Seq(_) => 3,
      _ => 4,
    }
  }
  fn is_nan(v: &Var) -> bool {
    matches!(v, Var::Float(f) if f.is_nan())
  }
  let o = match (a, b) {
    (Var::Int(_) | Var::Float(_), Var::Int(_) | Var::Float(_)) => match compare(a, b.clone()) {
      Ok(o) => return o,
      Err(_) => is_nan(a).cmp(&is_nan(b)),
    },
    (Var::String(x), Var::String(y)) => return x.cmp(y),
    (Var::Bool(x), Var::Bool(y)) => return x.cmp(y),
    (Var::Seq(x), Var::Seq(y)) => {
      for (p, q) in x.iter().zip(y.iter()) {
        let o = total_order(p, q, error);
        if o.is_ne() {
          return o;
        }
      }
      return x.len().cmp(&y.len());
    }
    _ => rank(a).cmp(&rank(b)),
  };
  if error.is_none() {
    *error = sort_order(a, b).err();
  }
  o
}

pub struct Sort;
impl LeafShard for Sort {
  type Compiled = bool;
  no_state!(bool);
  const DESC: ShardDesc = desc(
    "Sort",
    crate::shard_doc!("Sorts a sequence."),
    crate::shard_doc!(
      "Numbers by value, strings by their bytes, Bools false first, sequences element by element, so `[key value]` pairs sort by key and then value. The sort is stable. Elements of kinds that do not order against each other are an activation error."
    ),
    SORT_PARAMS,
    InputDesc::Types(SEQ),
    OutputDesc::SameAsInput,
  );
  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<bool>> {
    let input = input_one_of(ctx, Self::DESC.name, SEQ)?;
    Ok(Composed {
      compiled: args.bool("desc").unwrap_or(false),
      output: input,
    })
  }
  fn activate(desc: &bool, _: &mut (), _: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    let mut items = input.as_seq().unwrap_or(&[]).to_vec();
    let mut error = None;
    items.sort_by(|a, b| {
      let o = total_order(a, b, &mut error);
      if *desc { o.reverse() } else { o }
    });
    match error {
      Some(e) => Err(e),
      None => Ok(Flow::Next(Var::seq(items))),
    }
  }
}

// --- String.CodePoints, String.FromCodePoints, String.CleanUTF8 ---
//
// From here on, the shards are on the compose-time list (`compose_time.rs`):
// each charges its meter by the size it allocates.

const INT_SEQ_INPUT: &[TypeName] = &[TypeName::Seq];

/// Whether `input` is a sequence of Ints, the input `FromCodePoints` and
/// `IntsToBytes` take (`[Any]` must be narrowed with `ExpectIntSeq`).
fn int_seq(input: Type) -> std::result::Result<Type, &'static [TypeName]> {
  if Type::seq(Type::int()).accepts(input) {
    Ok(input)
  } else {
    Err(INT_SEQ_INPUT)
  }
}

/// A sequence of `len` elements is about to be allocated.
fn allocate_seq(meter: &Meter, len: usize) -> Result<()> {
  meter.allocate(len.saturating_mul(std::mem::size_of::<Var>()))
}

pub struct CodePointsOp;
impl PureOp for CodePointsOp {
  const DESC: ShardDesc = pure_desc(
    "String.CodePoints",
    crate::shard_doc!("Outputs a string's characters as their Unicode code points (Ints)."),
    crate::shard_doc!("One Int per character, as `Count` counts them: `\"hé\"` is `[104 233]`."),
    InputDesc::Types(STRING),
    OutputDesc::Dynamic(crate::shard_doc!("a sequence of Ints")),
  );
  fn output(input: Type) -> std::result::Result<Type, &'static [TypeName]> {
    one_of(input, STRING, Type::seq(Type::int()))
  }
  fn meter(input: &Var, meter: &Meter) -> Result<()> {
    let text = input.as_str().unwrap_or("");
    meter_bytes(input, meter)?;
    allocate_seq(meter, text.chars().count())
  }
  fn apply(input: &Var) -> Result<Var> {
    let text = input.as_str().unwrap_or("");
    Ok(Var::seq(
      text
        .chars()
        .map(|c| Var::Int(i64::from(u32::from(c))))
        .collect(),
    ))
  }
}

pub struct FromCodePointsOp;
impl PureOp for FromCodePointsOp {
  const DESC: ShardDesc = pure_desc(
    "String.FromCodePoints",
    crate::shard_doc!("Builds a string from Unicode code points (a sequence of Ints)."),
    crate::shard_doc!(
      "Each Int must be a Unicode scalar value: 0 to 0x10FFFF, surrogates (0xD800 to 0xDFFF) excluded. Any other is an activation error naming its index."
    ),
    InputDesc::Types(INT_SEQ_INPUT),
    OutputDesc::Fixed(TypeName::String),
  );
  fn output(input: Type) -> std::result::Result<Type, &'static [TypeName]> {
    int_seq(input).map(|_| Type::string())
  }
  fn meter(input: &Var, meter: &Meter) -> Result<()> {
    meter.allocate(4 * input.as_seq().map_or(0, <[Var]>::len))
  }
  fn apply(input: &Var) -> Result<Var> {
    let items = input.as_seq().unwrap_or(&[]);
    let mut out = String::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
      let c = match item {
        Var::Int(n) => u32::try_from(*n).ok().and_then(char::from_u32),
        _ => None,
      };
      match c {
        Some(c) => out.push(c),
        None => {
          return Err(fail(
            "String.FromCodePoints",
            &format!("element {i} ({item}) is not a Unicode scalar value"),
          ));
        }
      }
    }
    Ok(Var::from_string(Arc::from(out)))
  }
}

const TEXT_OR_BYTES: &[TypeName] = &[TypeName::Bytes, TypeName::String];

pub struct CleanUtf8Op;
impl PureOp for CleanUtf8Op {
  const DESC: ShardDesc = pure_desc(
    "String.CleanUTF8",
    crate::shard_doc!("Reads a byte string as UTF-8 text, replacing what is not UTF-8."),
    crate::shard_doc!(
      "Each invalid sequence becomes U+FFFD (`�`), so the output is always a string; `BytesToString` fails instead. A string is valid UTF-8 already and passes through."
    ),
    InputDesc::Types(TEXT_OR_BYTES),
    OutputDesc::Fixed(TypeName::String),
  );
  fn output(input: Type) -> std::result::Result<Type, &'static [TypeName]> {
    one_of(input, TEXT_OR_BYTES, Type::string())
  }
  fn meter(input: &Var, meter: &Meter) -> Result<()> {
    match input {
      // At most three bytes for each invalid one.
      Var::Bytes(b) => meter.allocate(b.len().saturating_mul(3)),
      _ => Ok(()),
    }
  }
  fn apply(input: &Var) -> Result<Var> {
    match input {
      Var::String(_) => Ok(input.clone()),
      Var::Bytes(b) => Ok(Var::from_string(Arc::from(String::from_utf8_lossy(b)))),
      _ => Err(fail("String.CleanUTF8", "input type mismatch")),
    }
  }
}

// --- HexToBytes, BytesToInts, IntsToBytes ---

pub struct HexToBytesOp;
impl PureOp for HexToBytesOp {
  const DESC: ShardDesc = ShardDesc {
    aliases: &["FromHex"],
    ..pure_desc(
      "HexToBytes",
      crate::shard_doc!("Decodes hexadecimal text into a byte string."),
      crate::shard_doc!(
        "Two digits a byte, either case, with an optional `0x` prefix; an odd number of digits reads as if a `0` led them, so `ToHex`'s Ints read back (`\"0xabc\"` is `@bytes(0abc)`). Any other character is an activation error."
      ),
      InputDesc::Types(STRING),
      OutputDesc::Fixed(TypeName::Bytes),
    )
  };
  fn output(input: Type) -> std::result::Result<Type, &'static [TypeName]> {
    one_of(input, STRING, Type::bytes())
  }
  fn meter(input: &Var, meter: &Meter) -> Result<()> {
    meter.allocate(input.as_str().map_or(0, str::len) / 2 + 1)
  }
  fn apply(input: &Var) -> Result<Var> {
    let text = input.as_str().unwrap_or("");
    let digits = text
      .strip_prefix("0x")
      .or_else(|| text.strip_prefix("0X"))
      .unwrap_or(text)
      .as_bytes();
    let value = |i: usize| -> Result<u8> {
      match (digits[i] as char).to_digit(16) {
        Some(d) => Ok(d as u8),
        None => Err(fail(
          "HexToBytes",
          &format!("{:?} at {i} is not a hex digit", digits[i] as char),
        )),
      }
    };
    let mut out = Vec::with_capacity(digits.len().div_ceil(2));
    let mut i = 0;
    if digits.len() % 2 == 1 {
      out.push(value(0)?);
      i = 1;
    }
    while i < digits.len() {
      out.push(value(i)? << 4 | value(i + 1)?);
      i += 2;
    }
    Ok(Var::from_bytes(Arc::from(out)))
  }
}

pub struct BytesToIntsOp;
impl PureOp for BytesToIntsOp {
  const DESC: ShardDesc = pure_desc(
    "BytesToInts",
    crate::shard_doc!("Outputs a byte string's bytes as Ints (0 to 255)."),
    "",
    InputDesc::Types(&[TypeName::Bytes]),
    OutputDesc::Dynamic(crate::shard_doc!("a sequence of Ints")),
  );
  fn output(input: Type) -> std::result::Result<Type, &'static [TypeName]> {
    one_of(input, &[TypeName::Bytes], Type::seq(Type::int()))
  }
  fn meter(input: &Var, meter: &Meter) -> Result<()> {
    allocate_seq(meter, input.as_bytes().map_or(0, <[u8]>::len))
  }
  fn apply(input: &Var) -> Result<Var> {
    let bytes = input.as_bytes().unwrap_or(&[]);
    Ok(Var::seq(
      bytes.iter().map(|b| Var::Int(i64::from(*b))).collect(),
    ))
  }
}

pub struct IntsToBytesOp;
impl PureOp for IntsToBytesOp {
  const DESC: ShardDesc = pure_desc(
    "IntsToBytes",
    crate::shard_doc!("Builds a byte string from Ints, one byte each."),
    crate::shard_doc!(
      "Every Int must be 0 to 255; any other is an activation error naming its index."
    ),
    InputDesc::Types(INT_SEQ_INPUT),
    OutputDesc::Fixed(TypeName::Bytes),
  );
  fn output(input: Type) -> std::result::Result<Type, &'static [TypeName]> {
    int_seq(input).map(|_| Type::bytes())
  }
  fn meter(input: &Var, meter: &Meter) -> Result<()> {
    meter.allocate(input.as_seq().map_or(0, <[Var]>::len))
  }
  fn apply(input: &Var) -> Result<Var> {
    let items = input.as_seq().unwrap_or(&[]);
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
      match item {
        Var::Int(n) if (0..=255).contains(n) => out.push(*n as u8),
        _ => {
          return Err(fail(
            "IntsToBytes",
            &format!("element {i} ({item}) is not a byte (0 to 255)"),
          ));
        }
      }
    }
    Ok(Var::from_bytes(Arc::from(out)))
  }
}

// --- String.PadStart, String.PadEnd ---

static PAD_PARAMS: &[ParamDecl] = &[
  decl(
    "width",
    crate::shard_doc!(
      "The length to pad to, in characters: an Int literal or variable. A string already this long or longer is unchanged."
    ),
    Forms::LITERAL.or(Forms::VARIABLE),
    &[TypeName::Int],
    Requirement::Required,
  ),
  decl(
    "with",
    crate::shard_doc!(
      "The character to pad with: a String of exactly one character, literal or variable."
    ),
    TEXT,
    STRING,
    Requirement::Default(DefaultValue::Str(" ")),
  ),
];

pub struct PadCompiled {
  width: Operand,
  with: Operand,
}

/// `String.PadStart` (`END` false) and `String.PadEnd`.
pub struct Pad<const END: bool>;
impl<const END: bool> Pad<END> {
  const NAME: &'static str = if END {
    "String.PadEnd"
  } else {
    "String.PadStart"
  };
}

/// The one character of a padding string, if it is one.
fn pad_char(text: &str) -> Option<char> {
  let mut chars = text.chars();
  match (chars.next(), chars.next()) {
    (Some(c), None) => Some(c),
    _ => None,
  }
}

impl<const END: bool> LeafShard for Pad<END> {
  type Compiled = PadCompiled;
  no_state!(PadCompiled);
  const DESC: ShardDesc = desc(
    Self::NAME,
    if END {
      crate::shard_doc!("Pads a string at the end to a width in characters.")
    } else {
      crate::shard_doc!("Pads a string at the start to a width in characters.")
    },
    crate::shard_doc!(
      "Characters are Unicode scalar values, as `Count` counts them. `with` must be exactly one character: a literal of another length is a compose error, a variable's an activation error. A string already `width` characters long or longer is output unchanged, never cut. `5 | ToString | String.PadStart(width: 2 with: \"0\")` is `\"05\"`."
    ),
    PAD_PARAMS,
    InputDesc::Types(STRING),
    OutputDesc::Fixed(TypeName::String),
  );
  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<PadCompiled>> {
    input_one_of(ctx, Self::NAME, STRING)?;
    let (width, _) = Operand::compose_arg(args, "width", Self::NAME, ctx)?;
    let (with, _) = Operand::compose_arg(args, "with", Self::NAME, ctx)?;
    if let Operand::Const(Var::String(s)) = &with
      && pad_char(s).is_none()
    {
      return Err(param_error(
        args,
        Self::NAME,
        "with",
        "compose-error",
        "invalid-argument-value",
        format!(
          "`with` must be exactly one character, got {} (\"{}\")",
          s.chars().count(),
          **s
        ),
      ));
    }
    Ok(Composed {
      compiled: PadCompiled { width, with },
      output: Type::string(),
    })
  }
  fn activate(c: &PadCompiled, _: &mut (), ctx: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    let width = match c.width.get(ctx) {
      Var::Int(i) => i,
      other => {
        return Err(fail(
          Self::NAME,
          &format!("width is {}, not an Int", other.type_of()),
        ));
      }
    };
    let with = text_of(&c.with, ctx);
    let Some(fill) = pad_char(&with) else {
      return Err(fail(
        Self::NAME,
        &format!(
          "`with` must be exactly one character, got {}",
          with.chars().count()
        ),
      ));
    };
    // A width of zero or less pads nothing, without reading the string.
    if width <= 0 {
      return Ok(Flow::Next(input.clone()));
    }
    let Ok(width) = usize::try_from(width) else {
      return Err(fail(
        Self::NAME,
        &format!("width {width} is too large for this target"),
      ));
    };
    let text = input.as_str().unwrap_or("");
    // Counting the characters reads the string: charged before it, on
    // every path after it.
    if let Some(meter) = ctx.meter() {
      meter_bytes(input, meter)?;
    }
    let len = text.chars().count();
    let Some(missing) = width.checked_sub(len).filter(|m| *m > 0) else {
      return Ok(Flow::Next(input.clone()));
    };
    let bytes = missing
      .checked_mul(fill.len_utf8())
      .and_then(|b| b.checked_add(text.len()));
    if let Some(meter) = ctx.meter() {
      meter.allocate(bytes.unwrap_or(usize::MAX))?;
    }
    let mut out = String::new();
    if bytes.is_none_or(|b| out.try_reserve_exact(b).is_err()) {
      return Err(fail(
        Self::NAME,
        &format!("padding to {width} characters needs more memory than there is"),
      ));
    }
    if END {
      out.push_str(text);
    }
    out.extend(std::iter::repeat_n(fill, missing));
    if !END {
      out.push_str(text);
    }
    Ok(Flow::Next(Var::from_string(Arc::from(out))))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn base64_round_trips_and_matches_the_standard() {
    for (plain, encoded) in [
      ("", ""),
      ("f", "Zg=="),
      ("fo", "Zm8="),
      ("foo", "Zm9v"),
      ("foob", "Zm9vYg=="),
      ("fooba", "Zm9vYmE="),
      ("foobar", "Zm9vYmFy"),
    ] {
      assert_eq!(base64_encode(plain.as_bytes()), encoded);
      assert_eq!(base64_decode(encoded).unwrap(), plain.as_bytes());
    }
    assert_eq!(base64_decode("Zm9vYg").unwrap(), b"foob");
    assert_eq!(base64_decode("-_8").unwrap(), [0xfb, 0xff]);
    assert!(base64_decode("Zm9v!").is_err());
    assert!(base64_decode("Z").is_err());
  }

  #[test]
  fn leb128_both_ways() {
    let to = |signed, i| ToLeb::apply(signed, &Var::Int(i)).unwrap();
    let from = |signed, v: &Var| FromLeb::apply(signed, v).unwrap();
    assert_eq!(to(false, 0).as_bytes(), Some(&[0u8][..]));
    assert_eq!(to(false, 300).as_bytes(), Some(&[0xac, 0x02][..]));
    assert_eq!(to(true, -1).as_bytes(), Some(&[0x7f][..]));
    assert_eq!(to(true, 63).as_bytes(), Some(&[0x3f][..]));
    assert_eq!(to(true, 64).as_bytes(), Some(&[0xc0, 0x00][..]));
    assert!(ToLeb::apply(false, &Var::Int(-1)).is_err());
    for i in [0, 1, 127, 128, 300, i64::MAX] {
      assert_eq!(from(false, &to(false, i)), Var::Int(i));
    }
    for i in [0, -1, 63, -64, 64, -65, i64::MIN, i64::MAX] {
      assert_eq!(from(true, &to(true, i)), Var::Int(i));
    }
    assert!(FromLeb::apply(false, &Var::bytes(&[0x80])).is_err());
    assert!(FromLeb::apply(false, &Var::bytes(&[0x01, 0x01])).is_err());
    assert!(FromLeb::apply(false, &Var::bytes(&[0xff; 10])).is_err());
  }

  #[test]
  fn uri_encoding_keeps_only_unreserved_characters() {
    assert_eq!(encode_uri("a b+c/é~"), "a%20b%2Bc%2F%C3%A9~");
    assert_eq!(decode_uri("a%20b%2Bc%2F%C3%A9~").unwrap(), "a b+c/é~");
    assert!(decode_uri("%zz").is_err());
    assert!(decode_uri("%ff").is_err());
  }
}
