//! otter-text acceptance: Shards scripts using the regex, hash and
//! identifier shards, loaded and run through the frontend.

use std::collections::{HashMap, HashSet};

use otter_core::diagnostic::Diagnostic;
use otter_core::{Catalog, Outcome, Var};
use otter_lang::{Program, Source};

fn catalog() -> Catalog {
  Catalog::new(&[otter_core::shards::CATALOG, otter_text::CATALOG]).unwrap()
}

fn report(text: &str) -> otter_lang::RunReport {
  let program = match Program::load(Source::new("t.shs", text), &catalog(), &HashMap::new()) {
    Ok(p) => p,
    Err((_, d)) => panic!("load failed: {d:?}"),
  };
  program
    .run()
    .unwrap_or_else(|d| panic!("run failed: {d:?}"))
}

/// The value a top-level program outputs.
fn value(text: &str) -> Var {
  match report(text).outcomes.into_iter().next() {
    Some((_, Some(Outcome::Completed(v)))) => v,
    other => panic!("expected a value, got {other:?}"),
  }
}

/// The activation error a top-level program fails with.
fn failure(text: &str) -> String {
  match report(text).outcomes.into_iter().next() {
    Some((_, Some(Outcome::Failed(e)))) => e.to_string(),
    other => panic!("expected a failure, got {other:?}"),
  }
}

/// The diagnostics of a program that must not compose.
fn errors(text: &str) -> Vec<Diagnostic> {
  let report = otter_lang::check(Source::new("t.shs", text), &catalog(), &HashMap::new());
  assert!(!report.ok(), "expected errors");
  report.diagnostics
}

// --- regex ---

#[test]
fn match_is_whole_string_with_groups() {
  let v = value(
    r#"
"This is a test string" | Regex.Match("(\\w+\\s*)+") = repeated
"baz.dat" | Regex.Match("([a-z]+)\\.([a-z]+)") = groups
"baz.dat!" | Regex.Match("([a-z]+)\\.([a-z]+)") = partial
"_@abc.cc" | Regex.Match("^[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\\.[A-Z|a-z]{2,}$") = email
"_@abc.c" | Regex.Match("^[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\\.[A-Z|a-z]{2,}$") = not-email
"ab" | Regex.Match("a|ab") = later-alternative
"b" | Regex.Match("(a)?b") = absent-group
"ab" | Regex.Match("(?x) a b # a comment") = verbose
"Straße" | Regex.Match("(?i)STRASSE|straße") = unicode
[repeated groups partial email not-email later-alternative absent-group verbose unicode]
"#,
  );
  assert_eq!(
    v.to_string(),
    r#"[["This is a test string" "string"] ["baz.dat" "baz" "dat"] [] ["_@abc.cc"] [] ["ab"] ["b" ""] ["ab"] ["Straße"]]"#
  );
}

#[test]
fn search_finds_every_match() {
  let v = value(
    r#"
"How many many do you see?" | Regex.Search("many") = words
"blah" | Regex.Search("Blah") = missing
"k1=v1; k2=v2" | Regex.Search("(\\w+)=(\\w+)") = pairs
"baa" | Regex.Search("a*") = empty-matches
"aaa" | Regex.Search("^a") = anchored
"a\nab" | Regex.Search("(?m)^a") = lines
"say \"one\" and \"two\"" | Regex.Search("\"([^\"]+)\"") = quoted
[words missing pairs empty-matches anchored lines quoted]
"#,
  );
  assert_eq!(
    v.to_string(),
    r#"[[["many"] ["many"]] [] [["k1=v1" "k1" "v1"] ["k2=v2" "k2" "v2"]] [[""] ["aa"]] [["a"]] [["a"] ["a"]] [["\"one\"" "one"] ["\"two\"" "two"]]]"#
  );
}

#[test]
fn replace_expands_groups() {
  let v = value(
    r#"
"Hello world, my name is John" | Regex.Replace("John" "Sarah") = plain
"[$0]" = brackets
"Quick brown fox" | Regex.Replace("a|e|i|o|u" brackets) = by-variable
"2026-10-16" | Regex.Replace("(\\d+)-(\\d+)-(\\d+)" "$3/$2/$1") = numbered
"2026-10-16" | Regex.Replace("(?P<y>\\d+)-(?P<m>\\d+)-(?P<d>\\d+)" "${d}.${m}.${y}") = named
"ab" | Regex.Replace("(a)" "${1}x") = braced
"cost" | Regex.Replace("cost" "$$5") = dollar
"a\\b\\c" | Regex.Replace("\\\\" "/") = slashes
"unchanged" | Regex.Replace("z+" "y") = no-match
"\"q\" rest" | Regex.Replace("\"[^\"]+\"" "") = removed
[plain by-variable numbered named braced dollar slashes no-match removed]
"#,
  );
  assert_eq!(
    v.to_string(),
    r#"["Hello world, my name is Sarah" "Q[u][i]ck br[o]wn f[o]x" "16/10/2026" "16.10.2026" "axb" "$5" "a/b/c" "unchanged" " rest"]"#
  );
}

#[test]
fn invalid_constant_patterns_fail_compose_at_the_argument() {
  for src in [
    r#""a" | Regex.Match("(a")"#,
    r#""a" | Regex.Search("a)(b")"#,
    // Lookaround and backreferences are not in the dialect.
    r#""a" | Regex.Replace("(?=a)" "b")"#,
    r#""aa" | Regex.Search("(a)\\1")"#,
  ] {
    let d = errors(src);
    assert_eq!(d.len(), 1, "{src}: {d:?}");
    let d = &d[0];
    assert_eq!(
      (d.code, d.param.as_deref(), d.param_index),
      ("invalid-argument-value", Some("regex"), Some(0)),
      "{src}: {d:?}"
    );
    assert!(d.message.starts_with("invalid regex:"), "{}", d.message);
    // Located at the shard; `param` names the argument.
    let column = src.find("Regex.").unwrap() as u32 + 1;
    assert_eq!((d.line, d.column), (Some(1), Some(column)), "{src}: {d:?}");
  }
  // Wrapping the text in anchors could have made `a)(b` valid; Match
  // anchors the parsed pattern instead.
  assert_eq!(
    errors(r#""ab" | Regex.Match("a)(b")"#)[0].code,
    "invalid-argument-value"
  );
}

#[test]
fn oversized_patterns_are_rejected() {
  for shard in ["Regex.Match", "Regex.Search"] {
    let d = errors(&format!(r#""a" | {shard}("\\w{{1000}}")"#));
    assert_eq!(d[0].code, "invalid-argument-value", "{d:?}");
    assert!(d[0].message.contains("limit"), "{}", d[0].message);
  }
  // A large repetition of a small pattern is fine.
  assert_eq!(
    value(r#""aaa" | Regex.Match("a{1,100}")"#).to_string(),
    r#"["aaa"]"#
  );
}

/// Patterns that make a backtracking engine (1.x's `std::regex`)
/// exponential on a near-miss input run in linear time here: these would
/// not finish with backtracking.
#[test]
fn catastrophic_patterns_stay_linear() {
  let input = format!("{}!", "a".repeat(20_000));
  let v = value(&format!(
    r#"
"{input}" = text
text | Regex.Match("(a+)+$") = m1
text | Regex.Match("(a|aa)*b") = m2
text | Regex.Search("(a*)*b") = s1
text | Regex.Search("(\\w+\\s?)*$") | Count = s2
[m1 m2 s1 s2]
"#
  ));
  assert_eq!(v.to_string(), r#"[[] [] [] 1]"#);
}

#[test]
fn variable_patterns_compile_at_activation_and_follow_changes() {
  let v = value(
    r#"
"\\d" | Var(pat)
[] | Var(counts)
Repeat({
  "a1b22c333" | Regex.Search(pat) | Count | Push(counts)
  "\\d+" | Update(pat)
} times: 3)
"x" = r
"a1b2" | Regex.Replace(pat r) = replaced
"[a-z]+" = word
"abc" | Regex.Match(word) = matched
[counts replaced matched]
"#,
  );
  assert_eq!(v.to_string(), r#"[[6 3 3] "axbx" ["abc"]]"#);
  // An invalid pattern from a variable fails the activation.
  let e = failure(r#""(" = p  "a" | Regex.Search(p)"#);
  assert!(e.contains("Regex.Search: invalid regex"), "{e}");
  // A variable must hold a String.
  let d = errors(r#"1 = p  "a" | Regex.Match(p)"#);
  assert_eq!(
    (d[0].code, d[0].param.as_deref()),
    ("wrong-variable-type", Some("regex"))
  );
}

#[test]
fn regex_input_must_be_a_string() {
  let d = errors(r#"1 | Regex.Search("a")"#);
  assert_eq!(d[0].code, "input-type-mismatch");
}

// --- hashes ---

#[test]
fn hashes_match_standard_vectors() {
  let v = value(
    r#"
"abc" | Hash.Sha2-256 | ToHex = sha256
"abc" | Hash.Sha1 | ToHex = sha1
"abc" | Hash.Md5 | ToHex = md5
"abc" | Hash.Blake3 | ToHex = blake3
"abc" | Hash.Blake2-128 | ToHex = blake2-128
"abc" | Hash.Blake2-256 | ToHex = blake2-256
"abc" | Hash.XXH3-64 = xxh64
xxh64 | ToHex = xxh64-hex
"abc" | Hash.XXH3-128 | ToHex = xxh128
[sha256 sha1 md5 blake3 blake2-128 blake2-256 xxh64 xxh64-hex xxh128]
"#,
  );
  let expected = [
    "\"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad\"",
    "\"a9993e364706816aba3e25717850c26c9cd0d89d\"",
    "\"900150983cd24fb0d6963f7d28e17f72\"",
    "\"6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85\"",
    "\"cf4ab791c62b8d2b2109c90275287816\"",
    "\"bddd813c634239723171ef3fee98579b94964e3bb1cb3e427262c8c068d52319\"",
    &0x78af5f94892f3950_i64.to_string(),
    "\"0x78af5f94892f3950\"",
    // Canonical (big-endian) order, as `xxhsum -H2` prints it.
    "\"06b05ab6733a618578af5f94892f3950\"",
  ];
  assert_eq!(v.to_string(), format!("[{}]", expected.join(" ")));
}

#[test]
fn hashes_take_bytes_and_empty_input() {
  let v = value(
    r#"
"abc" | StringToBytes | Hash.Sha2-256 = from-bytes
"abc" | Hash.Sha2-256 = from-string
"" | Hash.Sha2-256 | ToHex = empty
"" | Hash.Blake2-128 | ToHex = blake2-empty
"" | Hash.XXH3-64 | ToHex = xxh64-empty
"" | Hash.XXH3-128 | ToHex = xxh128-empty
"" | StringToBytes | Hash.Md5 | Count = md5-size
[(from-bytes | Is(from-string)) empty blake2-empty xxh64-empty xxh128-empty md5-size]
"#,
  );
  assert_eq!(
    v.to_string(),
    r#"[true "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855" "cae66941d9efbd404e4d88758ea67670" "0x2d06800538d394c2" "99aa06d3014798d86001c324468d497f" 16]"#
  );
  let d = errors(r#"1 | Hash.Blake3"#);
  assert_eq!(d[0].code, "input-type-mismatch");
}

// --- identifiers ---

fn strings(v: &Var) -> Vec<String> {
  v.as_seq()
    .unwrap()
    .iter()
    .map(|s| s.as_str().unwrap().to_string())
    .collect()
}

#[test]
fn uuids_are_version_4_and_unique() {
  let v = value(
    r#"
[] | Var(ids)
Repeat({ UUID | Push(ids) } times: 200)
ids
"#,
  );
  let ids = strings(&v);
  assert_eq!(ids.len(), 200);
  for id in &ids {
    let b = id.as_bytes();
    assert_eq!(b.len(), 36, "{id}");
    for (i, c) in b.iter().enumerate() {
      match i {
        8 | 13 | 18 | 23 => assert_eq!(*c, b'-', "{id}"),
        14 => assert_eq!(*c, b'4', "version: {id}"),
        19 => assert!(b"89ab".contains(c), "variant: {id}"),
        _ => assert!(c.is_ascii_hexdigit() && !c.is_ascii_uppercase(), "{id}"),
      }
    }
  }
  assert_eq!(ids.iter().collect::<HashSet<_>>().len(), ids.len());
}

#[test]
fn nanoids_have_their_size_and_alphabet_and_are_unique() {
  let v = value(
    r#"
[] | Var(ids)
Repeat({ NanoID | Push(ids) } times: 200)
NanoID(size: 8) = short
NanoID(1000) = long
[ids short long]
"#,
  );
  let parts = v.as_seq().unwrap();
  let ids = strings(&parts[0]);
  let url_safe = |s: &str| {
    s.bytes()
      .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
  };
  assert!(
    ids.iter().all(|id| id.len() == 21 && url_safe(id)),
    "{ids:?}"
  );
  assert_eq!(ids.iter().collect::<HashSet<_>>().len(), ids.len());
  let short = parts[1].as_str().unwrap();
  assert!(short.len() == 8 && url_safe(short), "{short}");
  let long = parts[2].as_str().unwrap();
  assert!(long.len() == 1000 && url_safe(long));
  // All 64 characters are reachable: over these 5200 draws, missing one
  // has a probability near 1e-34.
  let seen: HashSet<u8> = ids
    .iter()
    .flat_map(|id| id.bytes())
    .chain(long.bytes())
    .collect();
  assert_eq!(seen.len(), 64);
  for size in [0, 1025] {
    let d = errors(&format!("NanoID(size: {size})"));
    assert_eq!(
      (d[0].code, d[0].param.as_deref()),
      ("invalid-argument-value", Some("size"))
    );
  }
}
