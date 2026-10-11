//! Authoring eval runner (docs/golden-path.md §8).
//!
//! ```text
//! authoring-eval verify [--otter PATH]
//! authoring-eval reference [--variant NAME] [--otter PATH]
//! authoring-eval run --model NAME --cmd "<shell command>" --out FILE
//!     [--format text|claude-json] [--variant NAME] [--rounds N] [--jobs N]
//!     [--trials N] [--only TEXT] [--transcripts DIR] [--otter PATH]
//! ```
//!
//! `verify` checks every task's reference solution against its expected
//! output, without a model. `reference` prints what the model receives:
//! the variant's language primer (`reference/<variant>.md`) followed by
//! `otter catalog` and `otter describe` for every shard.
//!
//! `run` sends each task with the reference to `sh -c <cmd>` on stdin,
//! from an empty temporary directory, and reads the reply from stdout
//! (`text`), or from Claude Code's `--output-format json` envelope
//! (`claude-json`, which also reports tokens and the resolved model). The
//! last complete fenced code block of the reply is the program.
//! `otter check --json` diagnostics go back to the model for at most
//! `--rounds` repairs (default 5); a program that checks is then run with
//! `otter run --json` and its log compared with the task's expected
//! lines. One CSV row per task and trial is appended to `--out` as it
//! finishes.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const USAGE: &str = "usage:
  authoring-eval verify [--otter PATH]
  authoring-eval reference [--variant NAME] [--otter PATH]
  authoring-eval run --model NAME --cmd \"<shell command>\" --out FILE
      [--format text|claude-json] [--variant NAME] [--rounds N] [--jobs N]
      [--trials N] [--only TEXT] [--transcripts DIR] [--otter PATH]";

const CSV_HEADER: &str = "Task,Model,ModelId,Variant,Trial,SemanticSuccess,FirstPassSuccess,\
FirstFailure,RepairRounds,Tokens,OutputTokens,LatencyMs,Failure";

const MODEL_TIMEOUT: Duration = Duration::from_secs(600);
const SHARDS_TIMEOUT: Duration = Duration::from_secs(30);

fn manifest_dir() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

struct Task {
  id: String,
  prompt: String,
  expected: Vec<String>,
  reference: Option<String>,
}

/// A task file has `=== prompt`, `=== expected` and optionally
/// `=== reference` sections.
fn parse_task(id: &str, text: &str) -> Result<Task, String> {
  let mut sections: Vec<(String, String)> = Vec::new();
  for line in text.lines() {
    if let Some(name) = line.strip_prefix("=== ") {
      sections.push((name.trim().to_string(), String::new()));
    } else if let Some((_, body)) = sections.last_mut() {
      body.push_str(line);
      body.push('\n');
    } else if !line.trim().is_empty() {
      return Err(format!("{id}: text before the first section"));
    }
  }
  let get = |name: &str| {
    sections
      .iter()
      .find(|(n, _)| n == name)
      .map(|(_, b)| b.trim().to_string())
  };
  for (name, _) in &sections {
    if !["prompt", "expected", "reference"].contains(&name.as_str()) {
      return Err(format!("{id}: unknown section `{name}`"));
    }
  }
  Ok(Task {
    id: id.to_string(),
    prompt: get("prompt").ok_or(format!("{id}: missing prompt"))?,
    expected: get("expected")
      .ok_or(format!("{id}: missing expected"))?
      .lines()
      .map(str::to_string)
      .collect(),
    reference: get("reference"),
  })
}

fn load_tasks(only: Option<&str>) -> Result<Vec<Task>, String> {
  let dir = manifest_dir().join("tasks");
  let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
    .map_err(|e| format!("{}: {e}", dir.display()))?
    .filter_map(|e| e.ok().map(|e| e.path()))
    .filter(|p| p.extension().is_some_and(|e| e == "task"))
    .collect();
  paths.sort();
  let mut tasks = Vec::new();
  for path in paths {
    let id = path.file_stem().unwrap().to_string_lossy().to_string();
    if only.is_some_and(|o| !id.contains(o)) {
      continue;
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    tasks.push(parse_task(&id, &text)?);
  }
  Ok(tasks)
}

struct Output {
  status: Option<i32>,
  stdout: String,
  stderr: String,
  timed_out: bool,
}

/// Runs a command to completion or until `timeout`, feeding `stdin`. The
/// command runs in its own process group, which is killed when the command
/// exits or times out, so a descendant holding the pipes can neither keep
/// running nor stall the readers past the deadline.
fn run_with_timeout(mut cmd: Command, stdin: &str, timeout: Duration) -> Result<Output, String> {
  #[cfg(unix)]
  std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
  let mut child = cmd
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .map_err(|e| format!("spawn: {e}"))?;
  let mut input = child.stdin.take().unwrap();
  let stdin = stdin.to_string();
  // Detached: it ends when the pipe closes, at the latest when the group
  // is killed.
  std::thread::spawn(move || {
    let _ = input.write_all(stdin.as_bytes());
  });
  let (tx, rx) = std::sync::mpsc::channel();
  let read = |mut pipe: Box<dyn Read + Send>, which: usize| {
    let tx = tx.clone();
    std::thread::spawn(move || {
      let mut bytes = Vec::new();
      let _ = pipe.read_to_end(&mut bytes);
      let _ = tx.send((which, String::from_utf8_lossy(&bytes).into_owned()));
    });
  };
  read(Box::new(child.stdout.take().unwrap()), 0);
  read(Box::new(child.stderr.take().unwrap()), 1);
  let start = Instant::now();
  let (status, timed_out) = loop {
    if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
      break (status.code(), false);
    }
    if start.elapsed() > timeout {
      let _ = child.kill();
      let _ = child.wait();
      break (None, true);
    }
    std::thread::sleep(Duration::from_millis(20));
  };
  kill_group(child.id());
  let mut streams = [String::new(), String::new()];
  let grace = Instant::now() + Duration::from_secs(2);
  for _ in 0..2 {
    match rx.recv_timeout(grace.saturating_duration_since(Instant::now())) {
      Ok((which, text)) => streams[which] = text,
      Err(_) => break,
    }
  }
  let [stdout, stderr] = streams;
  Ok(Output {
    status,
    stdout,
    stderr,
    timed_out,
  })
}

/// Kills what is left of the process group led by `pid`.
fn kill_group(pid: u32) {
  #[cfg(unix)]
  if let Ok(pid) = libc::pid_t::try_from(pid) {
    // SAFETY: kill(2) takes plain integers and touches no memory of ours.
    // A negative pid addresses the group `run_with_timeout` created for
    // the child; failure (the group is already gone) is ignored.
    unsafe {
      libc::kill(-pid, libc::SIGKILL);
    }
  }
  #[cfg(not(unix))]
  let _ = pid;
}

/// An absolute path to `otter`: commands run from scratch directories.
fn otter_path(explicit: Option<&str>) -> Result<PathBuf, String> {
  let found = match explicit.map(str::to_string).or(std::env::var("OTTER").ok()) {
    Some(p) => PathBuf::from(p),
    None => {
      // Where cargo builds: `CARGO_TARGET_DIR` when set, so a stale
      // binary in the checkout's own `target` is never picked instead.
      let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir().join("../../target"));
      ["release", "debug"]
        .iter()
        .map(|profile| target.join(profile).join("otter"))
        .find(|p| p.exists())
        .ok_or("otter not found: build it (`cargo build --release -p otter-cli`) or pass --otter")?
    }
  };
  std::fs::canonicalize(&found).map_err(|e| format!("{}: {e}", found.display()))
}

fn otter(bin: &Path, args: &[&str], cwd: &Path) -> Result<Output, String> {
  let mut cmd = Command::new(bin);
  cmd.args(args).current_dir(cwd);
  run_with_timeout(cmd, "", SHARDS_TIMEOUT)
}

/// The primer for `variant` followed by the generated catalog.
fn reference(bin: &Path, variant: &str) -> Result<String, String> {
  let primer_path = manifest_dir()
    .join("reference")
    .join(format!("{variant}.md"));
  let primer =
    std::fs::read_to_string(&primer_path).map_err(|e| format!("{}: {e}", primer_path.display()))?;
  let cwd = std::env::temp_dir();
  let index = otter(bin, &["catalog"], &cwd)?;
  if index.status != Some(0) {
    return Err(format!("otter catalog failed: {}", index.stderr));
  }
  let json: serde_json::Value =
    serde_json::from_str(&index.stdout).map_err(|e| format!("catalog json: {e}"))?;
  let mut out = primer.trim_end().to_string();
  out.push_str("\n\n## Shard catalog\n\nOne `otter describe` record per shard:\n\n");
  for shard in json["shards"].as_array().ok_or("catalog: no shards")? {
    let name = shard["name"]
      .as_str()
      .ok_or("catalog: shard without name")?;
    let d = otter(bin, &["describe", name], &cwd)?;
    if d.status != Some(0) {
      return Err(format!("otter describe {name} failed: {}", d.stderr));
    }
    out.push_str(d.stdout.trim());
    out.push('\n');
  }
  Ok(out)
}

/// Why a program failed `check`: `syntax` (parse errors), `missing-shard`
/// (an unknown shard) or `compose` (anything else).
fn classify_check(report: &serde_json::Value) -> &'static str {
  let diagnostics = report["diagnostics"]
    .as_array()
    .cloned()
    .unwrap_or_default();
  let errors = diagnostics.iter().filter(|d| d["severity"] == "error");
  let mut kind = "compose";
  for d in errors {
    if d["phase"] == "parse" {
      return "syntax";
    }
    if d["code"] == "unknown-shard" {
      kind = "missing-shard";
    }
  }
  kind
}

struct Verdict {
  passed: bool,
  failure: &'static str,
  detail: String,
}

/// The verdict of a `otter run --json` report: the run must succeed and
/// log exactly the expected lines. Wire results are not part of the log.
fn judge(report: &serde_json::Value, expected: &[String]) -> (bool, &'static str) {
  let log: Vec<&str> = report["log"].as_array().map_or(Vec::new(), |l| {
    l.iter().filter_map(|v| v.as_str()).collect()
  });
  if report["ok"] != true {
    (false, "runtime")
  } else if log == expected {
    (true, "")
  } else {
    (false, "wrong-output")
  }
}

/// Runs a checked program and compares its log with the expected lines.
fn test(bin: &Path, dir: &Path, expected: &[String]) -> Result<Verdict, String> {
  let run = otter(bin, &["run", "--json", "solution.shs"], dir)?;
  let detail = format!(
    "exit {:?}\n{}\nstderr:\n{}",
    run.status,
    run.stdout.trim(),
    run.stderr
  );
  if run.timed_out {
    return Ok(Verdict {
      passed: false,
      failure: "timeout",
      detail,
    });
  }
  let report: serde_json::Value = serde_json::from_str(run.stdout.trim())
    .map_err(|e| format!("run --json: {e}: {}{}", run.stdout, run.stderr))?;
  let (passed, failure) = judge(&report, expected);
  Ok(Verdict {
    passed,
    failure,
    detail,
  })
}

struct Scratch(PathBuf);

impl Scratch {
  fn new(tag: &str) -> Result<Scratch, String> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("authoring-eval-{}-{n}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(Scratch(dir))
  }
}

impl Drop for Scratch {
  fn drop(&mut self) {
    let _ = std::fs::remove_dir_all(&self.0);
  }
}

/// Checks a program in `dir`; returns the report and whether it is ok.
fn check(bin: &Path, dir: &Path, program: &str) -> Result<(serde_json::Value, String), String> {
  std::fs::write(dir.join("solution.shs"), program).map_err(|e| e.to_string())?;
  let out = otter(bin, &["check", "--json", "solution.shs"], dir)?;
  let json: serde_json::Value = serde_json::from_str(out.stdout.trim())
    .map_err(|e| format!("check --json: {e}: {}{}", out.stdout, out.stderr))?;
  Ok((json, out.stdout.trim().to_string()))
}

fn verify(bin: &Path) -> Result<bool, String> {
  let tasks = load_tasks(None)?;
  let mut ok = true;
  for task in &tasks {
    let Some(program) = &task.reference else {
      println!("{}: no reference", task.id);
      continue;
    };
    let scratch = Scratch::new(&task.id)?;
    let (report, raw) = check(bin, &scratch.0, program)?;
    if report["ok"] != true {
      ok = false;
      println!("{}: check failed: {raw}", task.id);
      continue;
    }
    let v = test(bin, &scratch.0, &task.expected)?;
    if v.passed {
      println!("{}: ok", task.id);
    } else {
      ok = false;
      println!("{}: {}\n{}", task.id, v.failure, v.detail);
    }
  }
  println!("{} tasks", tasks.len());
  Ok(ok)
}

#[derive(Clone, Copy, PartialEq)]
enum Format {
  Text,
  ClaudeJson,
}

struct RunConfig {
  model: String,
  cmd: String,
  format: Format,
  variant: String,
  rounds: usize,
  reference: String,
  bin: PathBuf,
  transcripts: Option<PathBuf>,
}

struct Reply {
  text: String,
  tokens: Option<u64>,
  output_tokens: Option<u64>,
  /// Model identifiers the CLI reported (`claude-json` only).
  models: Vec<String>,
}

fn ask(config: &RunConfig, prompt: &str) -> Result<Reply, String> {
  let scratch = Scratch::new("model")?;
  let mut cmd = Command::new("sh");
  cmd.arg("-c").arg(&config.cmd).current_dir(&scratch.0);
  let out = run_with_timeout(cmd, prompt, MODEL_TIMEOUT)?;
  if out.timed_out {
    return Err("model command timed out".into());
  }
  if out.status != Some(0) {
    return Err(format!(
      "model command exit {:?}: {}{}",
      out.status, out.stderr, out.stdout
    ));
  }
  match config.format {
    Format::Text => Ok(Reply {
      text: out.stdout,
      tokens: None,
      output_tokens: None,
      models: Vec::new(),
    }),
    Format::ClaudeJson => {
      let json: serde_json::Value =
        serde_json::from_str(out.stdout.trim()).map_err(|e| format!("claude json: {e}"))?;
      if json["is_error"] == true {
        return Err(format!("model error: {}", json["result"]));
      }
      let usage = &json["usage"];
      let tokens = [
        "input_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
        "output_tokens",
      ]
      .iter()
      .map(|k| usage[k].as_u64().unwrap_or(0))
      .sum();
      let models = json["modelUsage"]
        .as_object()
        .map_or(Vec::new(), |m| m.keys().cloned().collect());
      Ok(Reply {
        text: json["result"]
          .as_str()
          .ok_or("claude json: no result")?
          .to_string(),
        tokens: Some(tokens),
        output_tokens: usage["output_tokens"].as_u64(),
        models,
      })
    }
  }
}

/// The last complete fenced code block of a reply. Fences count only at
/// the start of a line, so a backtick run inside prose cannot split it.
fn extract_program(reply: &str) -> Option<String> {
  let mut last = None;
  let mut open: Option<Vec<&str>> = None;
  for line in reply.lines() {
    let fence = line.trim_start().starts_with("```");
    match (&mut open, fence) {
      (None, true) => open = Some(Vec::new()),
      (Some(body), true) => {
        last = Some(body.join("\n") + "\n");
        open = None;
      }
      (Some(body), false) => body.push(line),
      (None, false) => {}
    }
  }
  last
}

const INSTRUCTIONS: &str = "The program is checked with `otter check` and then run with \
`otter run`; the lines it logs are compared exactly with the expected output, so log nothing \
else. Reply with the complete program in a single fenced code block and nothing else.";

fn first_prompt(config: &RunConfig, task: &Task) -> String {
  format!(
    "You are writing a program in the Shards language. Use only what the reference below \
     describes.\n\n<reference>\n{}\n</reference>\n\nTask:\n{}\n\n{INSTRUCTIONS}\n",
    config.reference, task.prompt
  )
}

fn repair_prompt(config: &RunConfig, task: &Task, program: &str, report: &str) -> String {
  format!(
    "{}\nYour previous program:\n```shards\n{}```\n\n`otter check --json` reported:\n{}\n\n\
     Fix the program. {INSTRUCTIONS}\n",
    first_prompt(config, task),
    program,
    report
  )
}

#[derive(Default)]
struct Row {
  semantic: bool,
  first_pass: bool,
  /// The class of the first failing check, when the first reply failed.
  first_failure: String,
  repairs: usize,
  tokens: Option<u64>,
  output_tokens: Option<u64>,
  models: Vec<String>,
  latency: Duration,
  failure: String,
}

fn add(total: Option<u64>, more: Option<u64>) -> Option<u64> {
  total.zip(more).map(|(a, b)| a + b)
}

/// Runs one task into `row`. An error is the harness's own failure (the
/// checker crashed or hung, a file could not be written).
fn run_task(
  config: &RunConfig,
  task: &Task,
  log: &mut String,
  row: &mut Row,
) -> Result<(), String> {
  let scratch = Scratch::new(&task.id)?;
  row.tokens = Some(0);
  row.output_tokens = Some(0);
  let mut prompt = first_prompt(config, task);
  log.push_str(&format!("# {}\n\n## Task\n\n{}\n", task.id, task.prompt));
  loop {
    let start = Instant::now();
    let reply = ask(config, &prompt);
    row.latency += start.elapsed();
    let reply = match reply {
      Ok(r) => r,
      Err(e) => {
        log.push_str(&format!("\n## Model error\n\n{e}\n"));
        row.failure = "model-error".into();
        return Ok(());
      }
    };
    row.tokens = add(row.tokens, reply.tokens);
    row.output_tokens = add(row.output_tokens, reply.output_tokens);
    for m in reply.models {
      if !row.models.contains(&m) {
        row.models.push(m);
      }
    }
    log.push_str(&format!(
      "\n## Reply {}\n\n{}\n",
      row.repairs,
      reply.text.trim()
    ));
    let Some(program) = extract_program(&reply.text) else {
      // Not a language failure: the reply broke the output format.
      row.failure = "format".into();
      if row.repairs == 0 {
        row.first_failure = "format".into();
      }
      return Ok(());
    };
    let (report, raw) = check(&config.bin, &scratch.0, &program)?;
    let ok = report["ok"] == true;
    if row.repairs == 0 {
      row.first_pass = ok;
      if !ok {
        row.first_failure = classify_check(&report).into();
      }
    }
    if ok {
      break;
    }
    log.push_str(&format!("\n## Check\n\n```json\n{raw}\n```\n"));
    if row.repairs == config.rounds {
      row.failure = classify_check(&report).into();
      return Ok(());
    }
    row.repairs += 1;
    prompt = repair_prompt(config, task, &program, &raw);
  }
  let v = test(&config.bin, &scratch.0, &task.expected)?;
  log.push_str(&format!(
    "\n## Run ({})\n\n```\n{}\n```\n",
    if v.passed { "pass" } else { v.failure },
    v.detail.trim_end()
  ));
  row.semantic = v.passed;
  row.failure = v.failure.into();
  Ok(())
}

fn csv_field(s: &str) -> String {
  if s.contains([',', '"', '\n']) {
    format!("\"{}\"", s.replace('"', "\"\""))
  } else {
    s.to_string()
  }
}

fn run(
  config: RunConfig,
  tasks: Vec<Task>,
  out: &Path,
  jobs: usize,
  trials: usize,
) -> Result<(), String> {
  let existing = std::fs::read_to_string(out).unwrap_or_default();
  if let Some(header) = existing.lines().next()
    && header != CSV_HEADER
  {
    return Err(format!(
      "{} has a different header; write to a new file",
      out.display()
    ));
  }
  let mut file = std::fs::OpenOptions::new()
    .create(true)
    .append(true)
    .open(out)
    .map_err(|e| format!("{}: {e}", out.display()))?;
  if existing.is_empty() {
    writeln!(file, "{CSV_HEADER}").map_err(|e| e.to_string())?;
  }
  if let Some(dir) = &config.transcripts {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
  }
  let mut runs: Vec<(Arc<Task>, usize)> = Vec::new();
  for task in tasks {
    let task = Arc::new(task);
    for trial in 1..=trials.max(1) {
      runs.push((task.clone(), trial));
    }
  }
  let total = runs.len();
  runs.reverse();
  let queue = Arc::new(Mutex::new(runs));
  let file = Arc::new(Mutex::new(file));
  let passed = Arc::new(AtomicUsize::new(0));
  let config = Arc::new(config);
  let workers: Vec<_> = (0..jobs.max(1))
    .map(|_| {
      let (queue, file, passed, config) =
        (queue.clone(), file.clone(), passed.clone(), config.clone());
      std::thread::spawn(move || -> Result<(), String> {
        loop {
          let Some((task, trial)) = queue.lock().unwrap().pop() else {
            return Ok(());
          };
          let mut log = String::new();
          let mut row = Row::default();
          if let Err(e) = run_task(&config, &task, &mut log, &mut row) {
            log.push_str(&format!("\n## Harness error\n\n{e}\n"));
            row.semantic = false;
            row.failure = "harness-error".into();
          }
          if let Some(dir) = &config.transcripts {
            let name = if trials > 1 {
              format!("{}.{trial}.md", task.id)
            } else {
              format!("{}.md", task.id)
            };
            std::fs::write(dir.join(name), &log).map_err(|e| e.to_string())?;
          }
          if row.semantic {
            passed.fetch_add(1, Ordering::Relaxed);
          }
          let number = |n: Option<u64>| n.map_or(String::new(), |t| t.to_string());
          let line = [
            csv_field(&task.id),
            csv_field(&config.model),
            csv_field(&row.models.join(" ")),
            csv_field(&config.variant),
            trial.to_string(),
            row.semantic.to_string(),
            row.first_pass.to_string(),
            row.first_failure.clone(),
            row.repairs.to_string(),
            number(row.tokens),
            number(row.output_tokens),
            row.latency.as_millis().to_string(),
            row.failure.clone(),
          ]
          .join(",");
          eprintln!("{line}");
          let mut f = file.lock().unwrap();
          writeln!(f, "{line}").map_err(|e| e.to_string())?;
          f.flush().map_err(|e| e.to_string())?;
        }
      })
    })
    .collect();
  for w in workers {
    w.join().map_err(|_| "worker panicked".to_string())??;
  }
  eprintln!("{} of {total} runs passed", passed.load(Ordering::Relaxed));
  Ok(())
}

struct Args {
  command: String,
  flags: std::collections::HashMap<String, String>,
}

fn parse_args() -> Result<Args, String> {
  let mut args = std::env::args().skip(1);
  let command = args.next().ok_or("missing command")?;
  let mut flags = std::collections::HashMap::new();
  while let Some(flag) = args.next() {
    let name = flag
      .strip_prefix("--")
      .ok_or(format!("unexpected argument {flag}"))?;
    let value = args.next().ok_or(format!("{flag} needs a value"))?;
    flags.insert(name.to_string(), value);
  }
  Ok(Args { command, flags })
}

fn main_inner() -> Result<bool, String> {
  let args = parse_args()?;
  let flag = |n: &str| args.flags.get(n).map(String::as_str);
  let known: &[&str] = match args.command.as_str() {
    "verify" => &["otter"],
    "reference" => &["otter", "variant"],
    "run" => &[
      "otter",
      "variant",
      "model",
      "cmd",
      "out",
      "format",
      "rounds",
      "jobs",
      "trials",
      "only",
      "transcripts",
    ],
    other => return Err(format!("unknown command {other}")),
  };
  if let Some(unknown) = args.flags.keys().find(|k| !known.contains(&k.as_str())) {
    return Err(format!("unknown option --{unknown}"));
  }
  let bin = otter_path(flag("otter"))?;
  let variant = flag("variant").unwrap_or("current").to_string();
  match args.command.as_str() {
    "verify" => verify(&bin),
    "reference" => {
      println!("{}", reference(&bin, &variant)?);
      Ok(true)
    }
    _ => {
      let number = |n: &str, default: usize| {
        flag(n).map_or(Ok(default), |v| {
          v.parse().map_err(|_| format!("--{n} takes a number"))
        })
      };
      let format = match flag("format").unwrap_or("text") {
        "text" => Format::Text,
        "claude-json" => Format::ClaudeJson,
        other => return Err(format!("unknown format {other}")),
      };
      let config = RunConfig {
        model: flag("model").ok_or("--model is required")?.to_string(),
        cmd: flag("cmd").ok_or("--cmd is required")?.to_string(),
        format,
        reference: reference(&bin, &variant)?,
        variant,
        rounds: number("rounds", 5)?,
        bin,
        transcripts: flag("transcripts").map(PathBuf::from),
      };
      let tasks = load_tasks(flag("only"))?;
      if tasks.is_empty() {
        return Err("no tasks selected".into());
      }
      let out = PathBuf::from(flag("out").ok_or("--out is required")?);
      run(
        config,
        tasks,
        &out,
        number("jobs", 4)?,
        number("trials", 1)?,
      )?;
      Ok(true)
    }
  }
}

fn main() -> ExitCode {
  match main_inner() {
    Ok(true) => ExitCode::SUCCESS,
    Ok(false) => ExitCode::FAILURE,
    Err(e) => {
      eprintln!("authoring-eval: {e}\n{USAGE}");
      ExitCode::from(2)
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_log_is_judged_apart_from_wire_results() {
    let expected = vec!["1".to_string(), "main: 2".to_string()];
    let report = |ok: bool, log: &[&str]| serde_json::json!({"ok": ok, "log": log, "outcomes": [{"wire": "main", "outcome": "completed", "value": "2"}]});
    assert_eq!(
      judge(&report(true, &["1", "main: 2"]), &expected),
      (true, "")
    );
    assert_eq!(
      judge(&report(true, &["1"]), &expected),
      (false, "wrong-output")
    );
    assert_eq!(
      judge(&report(true, &["1", "main: 2", "3"]), &expected),
      (false, "wrong-output")
    );
    assert_eq!(
      judge(&report(false, &["1", "main: 2"]), &expected),
      (false, "runtime")
    );
  }

  #[test]
  fn program_is_the_last_complete_fenced_block() {
    assert_eq!(
      extract_program("x\n```shards\n1 | Log\n```\n").as_deref(),
      Some("1 | Log\n")
    );
    assert_eq!(
      extract_program("```\na\n```\n```shards\nb\n```").as_deref(),
      Some("b\n")
    );
    // A backtick run inside prose is not a fence.
    let prose = "```shards\n1 | Log\n```\nNo ``` needed elsewhere.";
    assert_eq!(extract_program(prose).as_deref(), Some("1 | Log\n"));
    // An unclosed block does not count.
    assert_eq!(
      extract_program("```shards\na\n```\n```\nb").as_deref(),
      Some("a\n")
    );
    assert_eq!(extract_program("1 | Log"), None);
  }

  #[cfg(unix)]
  #[test]
  fn timeouts_and_exits_kill_descendants_holding_the_pipes() {
    // Bounds stay under the 2 s reader grace, so a failed kill fails here.
    // The shell waits on a descendant: the timeout must not wait for it.
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "sleep 30 & wait"]);
    let start = Instant::now();
    let out = run_with_timeout(cmd, "", Duration::from_millis(100)).unwrap();
    assert!(out.timed_out);
    assert!(
      start.elapsed() < Duration::from_millis(1500),
      "{:?}",
      start.elapsed()
    );
    // The shell exits first and leaves a descendant holding stdout.
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "echo hi; sleep 30 &"]);
    let start = Instant::now();
    let out = run_with_timeout(cmd, "", Duration::from_secs(20)).unwrap();
    assert!(!out.timed_out);
    assert_eq!((out.status, out.stdout.as_str()), (Some(0), "hi\n"));
    assert!(
      start.elapsed() < Duration::from_millis(1500),
      "{:?}",
      start.elapsed()
    );
  }

  /// Every reference solution runs and logs exactly the expected lines,
  /// so the tasks stay solvable as the language changes.
  #[test]
  fn reference_solutions_pass() {
    let catalog = otter_core::Catalog::new(&[otter_core::shards::CATALOG]).unwrap();
    for task in load_tasks(None).unwrap() {
      let source = otter_lang::Source::new(&task.id, task.reference.clone().unwrap());
      let program = otter_lang::Program::load(source, &catalog, &Default::default())
        .unwrap_or_else(|(_, d)| panic!("{}: {d:?}", task.id));
      let (report, lines) = otter_core::log::capture(|| program.run().unwrap());
      assert!(report.succeeded(), "{}", task.id);
      assert_eq!(lines, task.expected, "{}", task.id);
    }
  }

  #[test]
  fn every_task_parses() {
    let tasks = load_tasks(None).unwrap();
    assert!(tasks.len() >= 30);
    assert!(
      tasks
        .iter()
        .all(|t| !t.expected.is_empty() && t.reference.is_some())
    );
  }
}
