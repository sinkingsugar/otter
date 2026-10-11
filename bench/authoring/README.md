# Authoring eval

Spec: [docs/golden-path.md §8](../../docs/golden-path.md). It measures how well a model writes Shards from a reference alone, so syntax and API disputes are settled with data (decision D12).

## Pieces

- `tasks/*.task`: tasks, each with a `prompt`, the `expected` log lines and a hidden `reference` solution. `cargo test -p authoring-eval` runs every reference in-process and requires exactly the expected log, so the tasks stay solvable as the language changes. Tasks 01–38 are the original baseline; 39 requires recursive tree folding (M7); 40 and 41 add batch parsing with resets and inventory validation with multiple rejection paths; 42 needs a compose-time value (M8, a literal-only parameter computed with `#( )`). 43 and 44 need a block parameter (M9: a `Retry` and a `Timed` written as functions).
- `reference/<variant>.md`: the language primer for a syntax variant. The model gets the primer followed by `otter catalog` and `otter describe` for every shard (`authoring-eval reference` prints it). `current` is the pre-M2 syntax; keep it after M2. `functions` is the M2 plus M5 syntax (assignment forms, lowercase labels, `Keep`, exhaustive `Match`, `@fn`). Results compare the complete reference and runtime snapshots; catalog and behavior changes mean they do not isolate syntax alone.
- `control/*.shs`: pre-M2 reference solutions for the harder shared tasks, using the same prompts and expected logs as the current tasks. They are validated with the control runtime, not the current syntax.
- `src/main.rs`: the runner. It shells out to a model command; there is no embedded API client.

## Running

Build `otter` first (`cargo build --release -p otter-cli`; the runner needs `run --json`), then:

```sh
cargo run -p authoring-eval -- verify    # reference solutions through the real CLI

cargo run -p authoring-eval -- run --model sonnet --format claude-json \
  --cmd 'claude -p --model sonnet --output-format json --tools "" --setting-sources "" --strict-mcp-config' \
  --out results.csv --transcripts transcripts/sonnet

cargo run -p authoring-eval -- run --model codex --format text \
  --cmd 'codex exec --skip-git-repo-check --sandbox read-only -' --out results.csv
```

The prompt goes to the command's stdin, from an empty temporary directory, so the model sees only the prompt; disable tools where the CLI allows it. The command runs in its own process group, killed when it exits or after 10 minutes. Per task: the model's last complete fenced code block is checked with `otter check --json`; diagnostics go back for at most `--rounds` repairs (default 5); a program that checks is run with `otter run --json`, which must succeed and log exactly the expected lines. `--only TEXT` selects tasks whose id contains the text, `--jobs N` runs tasks in parallel (default 4), `--trials N` repeats each task.

Pass a full model ID or use `claude-json`, which records the resolved model. To compare a new syntax variant with `current` on the same model and day, build both the runner and the binary from the tag `authoring-control-v1`, the last control before M2 changes the syntax (the project was still named shards2 there: the binary is `shards2`, passed with that runner's `--shards2`). Run the control runner with `--variant current` and the new runner with `--variant functions`; each runner reads tasks from its own source tree. Merely pointing the new runner at the old `shards2` would still send the new task 25 prompt and the unsupported recursion task.

To extend a separate control checkout with tasks 40 and 41, run this from the current checkout, replacing `/path/to/control` with its path:

```sh
python3 - /path/to/control <<'PY'
from pathlib import Path
import sys

control = Path(sys.argv[1]) / "bench/authoring/tasks"
for reference in sorted(Path("bench/authoring/control").glob("*.shs")):
    task = Path("bench/authoring/tasks", reference.stem + ".task")
    prompt_and_expected = task.read_text().split("=== reference\n", 1)[0]
    with (control / task.name).open("x") as output:
        output.write(prompt_and_expected + "=== reference\n" + reference.read_text())
PY
```

Build the control binaries and run its `authoring-eval verify` before spending model quota. It has 40 tasks; the current tree has 44. Report tasks 39 and 42 to 44 separately as new capabilities. Task 25's prompt also changed from a reusable routine to a function with parameters: exclude it from the matched-prompt aggregate, leaving 39 shared tasks (01–24, 26–38, 40–41). Keep the harder shared tasks visible separately from the nearly saturated original set. Record the runtime commits, task/reference hashes, model ID, CLI version, trials and command with real results. Reference validation does not count as a model eval run.

## Results

One CSV row per (task, model, variant, trial): `ModelId` (what the CLI reported, `claude-json` only), `SemanticSuccess` (the log matched), `FirstPassSuccess` (the first reply checked clean), `FirstFailure` (the class of that first failure), `RepairRounds`, `Tokens` and `OutputTokens` (summed over rounds, `claude-json` only; `Tokens` is dominated by the fixed prompt, about 16k tokens of reference plus about 7k of the CLI's system prompt a call, so compare `OutputTokens`), `LatencyMs` (model time only) and `Failure`. Failure classes: `syntax` (parse errors), `missing-shard` (an unknown shard), `compose` (other check errors), `format` (no complete code block in the reply), `runtime`, `wrong-output`, `timeout`, `model-error` and `harness-error` (the runner's own step failed; the transcript says why).

Each run lives in `results/<date>-<what>/` (`results.csv` and `transcripts/`). `results/2026-10-09-m8-task42` runs task 42 alone (M8). `results/2026-10-09-m9-tasks43-44` runs tasks 43 and 44 (M9). Without `--otter` (or `OTTER`), the runner uses `otter` from `CARGO_TARGET_DIR` when it is set, else from the checkout's `target`, release before debug. The pre-M2 baseline is `results/2026-10-06-baseline` (Sonnet and Haiku, 3 trials each). `results/2026-10-06-sonnet-primer-v1` is the first Sonnet run, superseded (first primer, alias only, one trial; its `FirstFailure` values were filled in from the transcripts, and `ModelId` and `OutputTokens` were not recorded). Commit rows from real runs only, never mock or placeholder data. Small samples are evidence, not proof.

The post-M5/M7 [Haiku comparison](results/2026-10-07-haiku-functions/README.md) records three trials on both snapshots, including the harder shared tasks and the new tree fold. Its report separates matched prompts from changed/new capabilities and includes failures, manual inspection, provenance, and a reproducible summary.
