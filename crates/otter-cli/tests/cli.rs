//! The `otter` command end to end.

use std::process::Command;

fn script(name: &str, text: &str) -> String {
  let dir = std::env::temp_dir().join(format!("otter-cli-{}", std::process::id()));
  std::fs::create_dir_all(&dir).unwrap();
  let path = dir.join(name);
  std::fs::write(&path, text).unwrap();
  path.to_string_lossy().into_owned()
}

fn otter(args: &[&str]) -> (i32, String, String) {
  let out = Command::new(env!("CARGO_BIN_EXE_otter"))
    .args(args)
    .output()
    .unwrap();
  (
    out.status.code().unwrap_or(-1),
    String::from_utf8_lossy(&out.stdout).into_owned(),
    String::from_utf8_lossy(&out.stderr).into_owned(),
  )
}

#[test]
fn check_json_reports_located_diagnostics() {
  let file = script("bad.shs", "0 | Var(n)\n\"a\" | Add(2)\n");
  let (code, out, _) = otter(&["check", "--json", &file]);
  assert_eq!(code, 1);
  assert!(out.starts_with("{\"ok\":false,"), "{out}");
  assert!(out.contains("\"code\":\"input-type-mismatch\""), "{out}");
  assert!(out.contains("\"line\":2,\"column\":7"), "{out}");

  let (code, _, err) = otter(&["check", &file]);
  assert_eq!(code, 1);
  assert!(
    err.contains("bad.shs:2:7: compose error: Math.Add needs Int, Float, Float2, Float3 or Float4 input, got String"),
    "{err}"
  );
  assert!(err.contains("  2 | \"a\" | Add(2)\n    |       ^"), "{err}");
}

#[test]
fn check_ok_and_run_with_script_arguments() {
  let file = script("ok.shs", "@n = text\n40 | Add(2)\n");
  let (code, out, _) = otter(&["check", "--json", &file, "n:hello"]);
  assert_eq!(code, 0, "{out}");
  assert!(
    out.starts_with(&format!(
      "{{\"ok\":true,\"file\":\"{file}\",\"diagnostics\":[],\"wires\":["
    )),
    "{out}"
  );
  assert!(
    out.contains("\"input\":\"Int\",\"output\":\"Int\""),
    "{out}"
  );
  // Script arguments may come before the file.
  let (code, _, err) = otter(&["check", "n:hello", &file]);
  assert_eq!(code, 0, "{err}");
  let (code, out, err) = otter(&["run", &file, "n:hello"]);
  assert_eq!((code, out.as_str()), (0, "root: 42\n"), "{err}");
}

#[test]
fn run_json_separates_the_log_from_wire_results() {
  // A log line that looks like a result line stays in `log`.
  let file = script(
    "json.shs",
    "@wire(main { 1 | Log(\"main\") Maybe({[1] | Take(3)}) \"a\\\"b\" | Log })\n@mesh(m) @schedule(m,main) @run(m)\n",
  );
  let (code, out, err) = otter(&["run", "--json", &file]);
  assert_eq!((code, err.as_str()), (0, ""), "{out}");
  let expected = format!(
    r#"{{"ok":true,"file":"{file}","diagnostics":[],"log":["main: 1","Maybe: activation error: Take: index 3 is out of range (length 1)","a\"b"],"outcomes":[{{"wire":"main","outcome":"completed","value":"\"a\\\"b\""}}],"spawned_failures":[]}}"#
  );
  assert_eq!(out.trim(), expected);
  let failing = script("fail.shs", "[1] | Take(3)\n");
  let (code, out, _) = otter(&["run", "--json", &failing]);
  assert_eq!(code, 1);
  assert!(
    out.contains(r#""ok":false"#)
      && out.contains(r#""outcome":"failed","error":"activation error: Take"#),
    "{out}"
  );
  let unknown = script("unknown.shs", "Nope\n");
  let (code, out, _) = otter(&["run", "--json", &unknown]);
  assert_eq!(code, 1);
  assert!(
    out.contains(r#""code":"unknown-shard""#) && out.contains(r#""log":[],"outcomes":[]"#),
    "{out}"
  );
}

#[test]
fn syntax_errors_are_rendered_with_the_source_line() {
  let file = script("syntax.shs", "\"Hello\" | Log(\n");
  let (code, _, err) = otter(&["check", &file]);
  assert_eq!(code, 1);
  assert!(err.contains("syntax.shs:1:14: parse error: missing `)` for the parameters of `Log` opened at 1:14 [unclosed]"), "{err}");
}

#[test]
fn catalog_commands() {
  let (code, out, _) = otter(&["describe", "Add"]);
  assert_eq!(code, 0);
  assert!(
    out.contains("\"name\":\"Math.Add\",\"aliases\":[\"Add\"]"),
    "{out}"
  );
  let (code, _, err) = otter(&["describe", "Ad"]);
  assert_eq!(code, 2);
  assert!(err.contains("did you mean: Add"), "{err}");
  let (code, out, _) = otter(&["search", "http"]);
  assert_eq!(code, 0);
  assert!(out.contains("Http.Get: "), "{out}");
  let (code, _, err) = otter(&[]);
  assert_eq!(code, 2);
  assert!(err.contains("usage:"));
}

#[test]
fn watch_reloads_atomic_saves_and_keeps_running_after_rejected_edits() {
  use std::io::{BufRead, Write};
  use std::process::{Child, Stdio};
  use std::sync::mpsc;
  use std::time::Duration;

  struct KillOnDrop(Child);
  impl Drop for KillOnDrop {
    fn drop(&mut self) {
      let _ = self.0.kill();
      let _ = self.0.wait();
    }
  }

  {
    let file = script("watch.shs", "41");
    let mut child = KillOnDrop(
      Command::new(env!("CARGO_BIN_EXE_otter"))
        .arg("watch")
        .arg(&file)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap(),
    );
    let (send, lines) = mpsc::channel();
    let stdout = child.0.stdout.take().unwrap();
    let stderr = child.0.stderr.take().unwrap();
    let errors = send.clone();
    std::thread::spawn(move || {
      for line in std::io::BufReader::new(stdout).lines() {
        if send.send(line.unwrap()).is_err() {
          break;
        }
      }
    });
    std::thread::spawn(move || {
      for line in std::io::BufReader::new(stderr).lines() {
        if errors.send(line.unwrap()).is_err() {
          break;
        }
      }
    });
    let wait_for = |expected: &str| {
      let deadline = std::time::Instant::now() + Duration::from_secs(10);
      loop {
        let line = lines
          .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
          .unwrap_or_else(|e| panic!("waiting for {expected}: {e}"));
        if line.contains(expected) {
          break line;
        }
      }
    };
    wait_for("root: 41");
    std::fs::write(&file, "unknown").unwrap();
    wait_for("edit rejected; previous execution retained");
    let replacement = format!("{file}.new");
    std::fs::write(&replacement, "42").unwrap();
    std::fs::rename(&replacement, &file).unwrap();
    wait_for("root: 42");
    let live = |output, body| {
      format!(
        r#"@fn(Inner input: None output: {output} params: {{n: Int}} {{{body}}})
@wire(main {{Keep(n 0) Inc(n) Inner(n: n)}} looped: true)
@mesh(m) @schedule(m main) @run(m fps: 10)"#
      )
    };
    std::fs::write(&file, live("String", r#"f"old {n}" Log"#)).unwrap();
    wait_for("old 1");
    std::fs::write(&file, live("String", r#"f"new {n}" Log"#)).unwrap();
    let line = wait_for("new ");
    assert!(line.strip_prefix("new ").unwrap().parse::<i64>().unwrap() > 1);
    // A changed interface is rejected; explicit restart accepts it and
    // resets script locals without restarting the watching process.
    std::fs::write(&file, live("Int", r#"f"restart {n}" Log 123"#)).unwrap();
    wait_for("edit rejected; previous execution retained");
    child.0.stdin.as_mut().unwrap().write_all(b"r\n").unwrap();
    wait_for("restart 1");
    std::fs::write(&file, r#""pending" Log Pause(1000.0)"#).unwrap();
    wait_for("pending");
    child.0.stdin.as_mut().unwrap().write_all(b"q\n").unwrap();
    wait_for("root: cancelled");
    assert!(child.0.wait().unwrap().success());
  }
}

const GREET_TOOL: &str = r#"
@fn(Greet input: {input: {} env: {} working-dir: String} output: String params: {} {
  input.input.name | If({IsString} {ExpectString} {"" | Fail}) = name
  name | When({Is("boom")} {"no booms" | Fail})
  f"greeting {name}" | Log
  f"Hello, {name}!"
})
{
  definition: {
    name: "greet"
    description: "Greets someone."
    parameters: {type: "object" properties: {name: {type: "string"}} required: ["name"]}
  }
  use: "Greet"
}
"#;

#[test]
fn tool_prints_its_definition_and_runs_with_json_arguments() {
  let file = script("greet.shs", GREET_TOOL);
  let (code, out, err) = otter(&["tool", &file]);
  assert_eq!(code, 0, "{err}");
  assert_eq!(
    out.trim_end(),
    r#"{"name":"greet","description":"Greets someone.","parameters":{"properties":{"name":{"type":"string"}},"required":["name"],"type":"object"}}"#
  );

  // The output on stdout, the log on stderr.
  let (code, out, err) = otter(&["tool", &file, r#"{"name": "Ann"}"#]);
  assert_eq!(code, 0, "{err}");
  assert_eq!(out, "Hello, Ann!\n");
  assert!(err.contains("greeting Ann"), "{err}");

  let (code, out, err) = otter(&["tool", &file, r#"{"name": "boom"}"#]);
  assert_eq!(code, 1);
  assert_eq!(out, "");
  assert!(err.contains("greet: failed: no booms"), "{err}");

  let (code, _, err) = otter(&["tool", &file, "[1]"]);
  assert_eq!(code, 2);
  assert!(err.contains("must be a JSON object"), "{err}");
  let (code, _, err) = otter(&["tool", &file, "{"]);
  assert_eq!(code, 2);
  assert!(err.contains("invalid JSON"), "{err}");

  // A script that is not a tool is reported at load, with its location.
  let bad = script("not-a-tool.shs", "\"a\" | Add(1)\n");
  let (code, _, err) = otter(&["tool", &bad]);
  assert_eq!(code, 1);
  assert!(err.contains("not-a-tool.shs:1:"), "{err}");
}

/// The Google Flights example tools (examples/tools/flights).
mod flights {
  use super::otter;

  fn tool(name: &str) -> String {
    format!(
      "{}/../../examples/tools/flights/{name}",
      env!("CARGO_MANIFEST_DIR")
    )
  }

  #[test]
  fn the_flights_tools_load_and_describe_themselves() {
    for (file, name, required) in [
      (
        "search-flights.shs",
        "google_flights_search",
        r#""required":["from","to","date"]"#,
      ),
      (
        "cheap-dates.shs",
        "google_flights_cheap_dates",
        r#""required":["from","to","from-date","to-date"]"#,
      ),
    ] {
      let (code, out, err) = otter(&["tool", &tool(file)]);
      assert_eq!(code, 0, "{file}: {err}");
      assert!(
        out.starts_with(&format!("{{\"name\":\"{name}\",\"description\":\"")),
        "{out}"
      );
      assert!(out.contains(required), "{out}");
    }
    // Missing arguments fail before any request.
    let (code, _, err) = otter(&["tool", &tool("search-flights.shs"), r#"{"from": "KIX"}"#]);
    assert_eq!(code, 1);
    assert!(err.contains("from, to and date are required"), "{err}");
    let (code, _, err) = otter(&["tool", &tool("cheap-dates.shs"), "{}"]);
    assert_eq!(code, 1);
    assert!(
      err.contains("from, to, from-date and to-date are required"),
      "{err}"
    );
  }

  /// A date `days` from today, as `YYYY-MM-DD` (UTC).
  fn date_in(days: u64) -> String {
    let secs = std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap()
      .as_secs();
    // Howard Hinnant's civil_from_days.
    let z = (secs / 86_400 + days) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
  }

  /// Searches Google Flights for real: needs the network, and Google may
  /// rate-limit. `cargo test -p otter-cli --test cli -- --ignored flights`.
  #[test]
  #[ignore = "queries the live Google Flights website"]
  fn live_search() {
    let args = format!(
      r#"{{"from": "KIX", "to": "NRT", "date": "{}", "max-stops": 0}}"#,
      date_in(30)
    );
    let (code, out, err) = otter(&["tool", &tool("search-flights.shs"), &args]);
    assert_eq!(code, 0, "{err}");
    assert!(out.starts_with("One-way KIX -> NRT on "), "{out}");
    assert!(out.contains(" - nonstop\n"), "{out}");
    assert!(
      out.contains("Book / see more: https://www.google.com/travel/flights/search?tfs="),
      "{out}"
    );
  }

  #[test]
  #[ignore = "queries the live Google Flights website"]
  fn live_cheap_dates() {
    let args = format!(
      r#"{{"from": "KIX", "to": "NRT", "from-date": "{}", "to-date": "{}"}}"#,
      date_in(30),
      date_in(33)
    );
    let (code, out, err) = otter(&["tool", &tool("cheap-dates.shs"), &args]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("Cheapest: "), "{out}");
  }
}
