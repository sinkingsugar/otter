//! Calendar dates: `Date.Parse` and `Date.Format`, between text and Unix
//! seconds, in UTC only (there is no time zone database on every target).
//!
//! Formats use a `strftime` subset: `%Y` (four or more digits; parsing
//! reads an optional sign and up to 12 digits, four when another number
//! follows directly, so every formatted date reads back), `%m`, `%d`, `%H`, `%M`, `%S` (two
//! digits), `%%`; every other character must match itself. 1.x's
//! `Date.Format` took any `strftime` format from the C library; formats
//! outside the subset are compose errors here.

use std::sync::Arc;

use super::leaf::{LeafShard, leaf_type};
use super::values::fail;
use super::*;
use crate::instance::{InstanceCtx, LeafCtx};
use crate::shard::Flow;

pub static DATE_PARSE: ShardType = leaf_type::<DateShard<ParseK>>();
pub static DATE_FORMAT: ShardType = leaf_type::<DateShard<FormatK>>();

/// One element of a format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
  Year,
  Month,
  Day,
  Hour,
  Minute,
  Second,
  Char(char),
}

fn parse_format(format: &str) -> std::result::Result<Vec<Part>, String> {
  let mut parts = Vec::new();
  let mut chars = format.chars();
  while let Some(c) = chars.next() {
    if c != '%' {
      parts.push(Part::Char(c));
      continue;
    }
    parts.push(match chars.next() {
      Some('Y') => Part::Year,
      Some('m') => Part::Month,
      Some('d') => Part::Day,
      Some('H') => Part::Hour,
      Some('M') => Part::Minute,
      Some('S') => Part::Second,
      Some('%') => Part::Char('%'),
      Some(other) => {
        return Err(format!(
          "`%{other}` is not supported (supported: %Y %m %d %H %M %S %%)"
        ));
      }
      None => return Err("the format ends with `%`".into()),
    });
  }
  Ok(parts)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
  let y = if m <= 2 { y - 1 } else { y };
  let era = y.div_euclid(400);
  let yoe = y - era * 400;
  let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
  let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
  era * 146_097 + doe - 719_468
}

/// The date of a day count since 1970-01-01 (`civil_from_days`).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
  let z = z + 719_468;
  let era = z.div_euclid(146_097);
  let doe = z - era * 146_097;
  let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
  let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
  let mp = (5 * doy + 2) / 153;
  let d = doy - (153 * mp + 2) / 5 + 1;
  let m = if mp < 10 { mp + 3 } else { mp - 9 };
  (
    if m <= 2 {
      yoe + era * 400 + 1
    } else {
      yoe + era * 400
    },
    m,
    d,
  )
}

fn days_in_month(y: i64, m: i64) -> i64 {
  match m {
    1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
    4 | 6 | 9 | 11 => 30,
    _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
    _ => 28,
  }
}

/// Text to Unix seconds, matching `parts` exactly.
fn parse_date(parts: &[Part], text: &str) -> std::result::Result<i64, String> {
  let bytes = text.as_bytes();
  let mut at = 0;
  let (mut y, mut mo, mut d, mut h, mut mi, mut s) = (1970i64, 1i64, 1i64, 0i64, 0i64, 0i64);
  let number = |at: &mut usize, min: usize, max: usize| -> std::result::Result<i64, String> {
    let start = *at;
    while *at < bytes.len() && *at - start < max && bytes[*at].is_ascii_digit() {
      *at += 1;
    }
    if *at - start < min {
      return Err(format!("expected digits at {start}"));
    }
    text[start..*at]
      .parse()
      .map_err(|_| format!("number out of range at {start}"))
  };
  for (k, part) in parts.iter().enumerate() {
    match part {
      Part::Year => {
        let negative = bytes.get(at) == Some(&b'-');
        if negative {
          at += 1;
        }
        // Up to 12 digits when text ends the year, every year an Int of
        // seconds reaches (about 292 billion either way), so whatever
        // `Date.Format` writes reads back; four when a number follows
        // directly (`%Y%m%d`).
        let max = match parts.get(k + 1) {
          None | Some(Part::Char(_)) => 12,
          Some(_) => 4,
        };
        let v = number(&mut at, 1, max)?;
        y = if negative { -v } else { v };
      }
      Part::Month => mo = number(&mut at, 2, 2)?,
      Part::Day => d = number(&mut at, 2, 2)?,
      Part::Hour => h = number(&mut at, 2, 2)?,
      Part::Minute => mi = number(&mut at, 2, 2)?,
      Part::Second => s = number(&mut at, 2, 2)?,
      Part::Char(c) => {
        let mut buf = [0u8; 4];
        let c = c.encode_utf8(&mut buf);
        if !text[at..].starts_with(&*c) {
          return Err(format!("expected `{c}` at {at}"));
        }
        at += c.len();
      }
    }
  }
  if at != bytes.len() {
    return Err(format!("unexpected text at {at}"));
  }
  if !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) {
    return Err(format!("{y}-{mo:02}-{d:02} is not a date"));
  }
  if h > 23 || mi > 59 || s > 60 {
    return Err(format!("{h:02}:{mi:02}:{s:02} is not a time"));
  }
  // In i128: the earliest second's day times 86,400 is below i64::MIN
  // before its time of day is added.
  let seconds = i128::from(days_from_civil(y, mo, d)) * 86_400 + i128::from(h * 3600 + mi * 60 + s);
  i64::try_from(seconds).map_err(|_| format!("{y}-{mo:02}-{d:02} is out of range"))
}

fn format_date(parts: &[Part], seconds: i64) -> String {
  use std::fmt::Write;
  let (y, mo, d) = civil_from_days(seconds.div_euclid(86_400));
  let rest = seconds.rem_euclid(86_400);
  let mut out = String::new();
  for part in parts {
    let _ = match part {
      Part::Year => write!(out, "{y:04}"),
      Part::Month => write!(out, "{mo:02}"),
      Part::Day => write!(out, "{d:02}"),
      Part::Hour => write!(out, "{:02}", rest / 3600),
      Part::Minute => write!(out, "{:02}", rest / 60 % 60),
      Part::Second => write!(out, "{:02}", rest % 60),
      Part::Char(c) => write!(out, "{c}"),
    };
  }
  out
}

static DATE_PARAMS: &[ParamDecl] = &[decl(
  "format",
  crate::shard_doc!(
    "The layout: `%Y` `%m` `%d` `%H` `%M` `%S` `%%`; other characters as they are."
  ),
  Forms::LITERAL,
  &[TypeName::String],
  Requirement::Default(DefaultValue::Str("%Y-%m-%d %H:%M:%S")),
)];

/// One direction.
pub trait DateKind: 'static {
  const DESC: ShardDesc;
  const INPUT: TypeName;
  fn output() -> Type;
  fn apply(parts: &[Part], input: &Var) -> Result<Var>;
}

const fn date_desc(
  name: &'static str,
  summary: &'static str,
  help: &'static str,
  input: &'static [TypeName],
  output: TypeName,
) -> ShardDesc {
  ShardDesc {
    name,
    version: 1,
    summary,
    help,
    params: Params::Declared(DATE_PARAMS),
    input: InputDesc::Types(input),
    output: OutputDesc::Fixed(output),
    targets: Targets::All,
    aliases: &[],
    effects: crate::signature::Effects::NONE,
    lifetime: crate::signature::Lifetime::Stateless,
  }
}

pub struct ParseK;
impl DateKind for ParseK {
  const DESC: ShardDesc = date_desc(
    "Date.Parse",
    crate::shard_doc!("Reads a UTC date and time into Unix seconds."),
    crate::shard_doc!(
      "The text must match the format exactly; fields it leaves out are 1970-01-01 00:00:00. A date that does not exist (`2026-02-30`) is an activation error."
    ),
    &[TypeName::String],
    TypeName::Int,
  );
  const INPUT: TypeName = TypeName::String;
  fn output() -> Type {
    Type::int()
  }
  fn apply(parts: &[Part], input: &Var) -> Result<Var> {
    parse_date(parts, input.as_str().unwrap_or(""))
      .map(Var::Int)
      .map_err(|e| fail("Date.Parse", &e))
  }
}

pub struct FormatK;
impl DateKind for FormatK {
  const DESC: ShardDesc = date_desc(
    "Date.Format",
    crate::shard_doc!("Writes Unix seconds as a UTC date and time."),
    "",
    &[TypeName::Int],
    TypeName::String,
  );
  const INPUT: TypeName = TypeName::Int;
  fn output() -> Type {
    Type::string()
  }
  fn apply(parts: &[Part], input: &Var) -> Result<Var> {
    let Var::Int(seconds) = *input else {
      return Err(fail("Date.Format", "input type mismatch"));
    };
    Ok(Var::from_string(Arc::from(format_date(parts, seconds))))
  }
}

pub struct DateShard<K>(std::marker::PhantomData<K>);
impl<K: DateKind> LeafShard for DateShard<K> {
  type Compiled = Vec<Part>;
  type State = ();
  const DESC: ShardDesc = K::DESC;
  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Vec<Part>>> {
    let input = ctx.input();
    if !K::INPUT.matches(input) || input == Type::any() {
      return Err(super::values::mismatch(
        K::DESC.name,
        input,
        std::slice::from_ref(&K::INPUT),
      ));
    }
    let format = args.string("format").unwrap_or("%Y-%m-%d %H:%M:%S");
    let parts = parse_format(format).map_err(|message| {
      param_error(
        args,
        K::DESC.name,
        "format",
        "compose-error",
        "invalid-argument-value",
        message,
      )
    })?;
    Ok(Composed {
      compiled: parts,
      output: K::output(),
    })
  }
  fn instantiate(_: &Vec<Part>, _: &mut InstanceCtx) -> Result<()> {
    Ok(())
  }
  fn activate(parts: &Vec<Part>, _: &mut (), _: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    K::apply(parts, input).map(Flow::Next)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn dates_round_trip_through_unix_seconds() {
    let f = parse_format("%Y-%m-%d %H:%M:%S").unwrap();
    for (text, seconds) in [
      ("1970-01-01 00:00:00", 0),
      ("2000-02-29 12:34:56", 951_827_696),
      ("2026-11-12 00:00:00", 1_794_441_600),
      ("1969-12-31 23:59:59", -1),
    ] {
      assert_eq!(parse_date(&f, text).unwrap(), seconds, "{text}");
      assert_eq!(format_date(&f, seconds), text);
    }
    let day = parse_format("%Y-%m-%d").unwrap();
    assert!(parse_date(&day, "2026-02-29").is_err());
    // Every Int of seconds formats to text that reads back; a year past
    // them is an error, not an overflow.
    for seconds in [i64::MIN, i64::MIN + 1, -1, i64::MAX - 1, i64::MAX] {
      assert_eq!(parse_date(&f, &format_date(&f, seconds)).unwrap(), seconds);
    }
    assert!(parse_date(&day, "999999999999-01-01").is_err());
    assert!(parse_date(&day, "2026-2-1").is_err());
    assert!(parse_date(&day, "2026-02-01x").is_err());
    assert!(parse_format("%Q").is_err());
  }
}
