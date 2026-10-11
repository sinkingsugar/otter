#!/usr/bin/env python3
"""Wall-clock A/B of two revisions on one machine (docs/performance-gates.md §3):
what the instruction gate cannot see (code layout, branch prediction on a real
core). CI runs it on every pull request, on Apple Silicon.

    python3 scripts/perf-ab.py BASE              # BASE against this checkout
    python3 scripts/perf-ab.py BASE --head REV   # against another revision
    python3 scripts/perf-ab.py HEAD --head HEAD  # A/A: the noise floor

Builds each side in release with the repository's flags (BASE in a temporary
worktree, each side in its own target directory under target/perf-ab, which
CI's cache keeps), then times both, interleaved
in mirrored order (ABBA) round by round so drift in the machine hits both
sides alike: the VM suite (every case at widths 8, 64 and 256, scripts from
bench/vm-execution/workloads.py, through `otter run`), the entity tick
(`bench_instances`), suspension depth (`bench_depth --progress`) and table
storage (`bench_tables`). Reports each case's median per side and the change,
as a markdown table (also to $GITHUB_STEP_SUMMARY). With --fail-above PCT, it
exits non-zero when a case is slower by more than PCT percent.

Python 3 standard library only.
"""

import argparse
import math
import os
from pathlib import Path
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "bench/vm-execution"))
from workloads import CASES, dialect, parse_output, script  # noqa: E402

WIDTHS = (8, 64, 256)
EXAMPLES = ("bench_instances", "bench_depth", "bench_tables")
# A VM batch's target duration, and the batches per process (the first is
# warm-up and discarded).
BATCH_SECONDS = 0.01
BATCHES = 4


def build(tree, target_dir):
    """Builds `otter` and the core benchmarks in `tree`; returns their paths."""
    env = dict(os.environ, CARGO_TARGET_DIR=str(target_dir))
    command = ["cargo", "build", "--release", "-q", "-p", "otter-cli", "--bin", "otter", "-p", "otter-core"]
    for example in EXAMPLES:
        command += ["--example", example]
    subprocess.run(command, cwd=tree, env=env, check=True)
    release = target_dir / "release"
    return {"otter": release / "otter", **{e: release / "examples" / e for e in EXAMPLES}}


def output(command):
    p = subprocess.run(command, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=600)
    if p.returncode:
        raise RuntimeError(f"failed ({p.returncode}): {' '.join(map(str, command))}\n{p.stdout[-3000:]}")
    return p.stdout


class Vm:
    """A VM suite case: nanoseconds per iteration, the median of a process's
    timed batches."""

    def __init__(self, case, width, scratch):
        self.name, self.case, self.width, self.scratch = f"vm/{case}/{width}", case, width, scratch
        self.iterations = None

    def write(self, iterations):
        self.iterations = iterations
        self.path = self.scratch / f"{self.case}-{self.width}.shs"
        self.path.write_text(dialect(script(self.case, self.width, iterations, BATCHES), "2"))

    def calibrate(self, bins):
        self.write(1000)
        seconds = min(parse_output(output([bins["otter"], "run", self.path]), 1000, BATCHES)[1:]) / 1000
        self.write(max(100, min(10_000_000, math.ceil(BATCH_SECONDS / max(seconds, 1e-10)))))

    def measure(self, bins):
        text = output([bins["otter"], "run", self.path])
        times = parse_output(text, self.iterations, BATCHES)[1:]
        return {self.name: statistics.median(times) * 1e9 / self.iterations}


def instances(bins):
    text = output([bins["bench_instances"], "1000"])
    return {"entity/1000": float(re.search(r"ns_per_instance_tick=([\d.]+)", text).group(1))}


def depth(bins):
    text = output([bins["bench_depth"], "--progress"])
    return {f"depth/{d}": float(ns) for d, ns in re.findall(r"depth=(\d+) .*ns_per_instance_tick=([\d.]+)", text)}


def tables(bins):
    text = output([bins["bench_tables"], "200000"])
    return {f"table/{name}": float(ns) for name, ns in re.findall(r"^(\S+)\s+ns_per_iter=\s*([\d.]+)", text, re.M)}


def table(samples, names):
    """Per case: base and head medians (ns) and the change; with the
    spread of the per-round head/base ratios, as a noise estimate."""
    rows = []
    for name in names:
        base, head = samples["base"][name], samples["head"][name]
        ratios = sorted(h / b for b, h in zip(base, head))
        rows.append(dict(name=name, base=statistics.median(base), head=statistics.median(head),
                         change=statistics.median(head) / statistics.median(base) - 1,
                         spread=(ratios[-1] - ratios[0]) if len(ratios) > 1 else 0.0))
    return rows


def render(rows, base, head, rounds, fail_above):
    lines = [f"### Wall-clock A/B: {head} against {base}", "",
             f"{rounds} interleaved rounds per case; medians in ns; spread is max - min of the per-round ratios.", "",
             "| case | base | head | change | spread |", "|---|---:|---:|---:|---:|"]
    for r in rows:
        mark = " ⚠" if fail_above is not None and r["change"] * 100 > fail_above else ""
        lines.append(f"| {r['name']} | {r['base']:.1f} | {r['head']:.1f} | {r['change']:+.1%}{mark} | {r['spread']:.1%} |")
    changes = [r["change"] for r in rows]
    lines += ["", f"Geometric mean change: {math.exp(statistics.mean(math.log1p(c) for c in changes)) - 1:+.2%}; "
              f"slowest {max(changes):+.1%}, fastest {min(changes):+.1%}."]
    return "\n".join(lines) + "\n"


def revision(rev):
    return subprocess.run(["git", "rev-parse", "--short", rev], cwd=ROOT, text=True,
                          stdout=subprocess.PIPE, check=True).stdout.strip()


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("base", help="the base revision")
    p.add_argument("--head", help="the head revision (default: this checkout, as it is)")
    p.add_argument("--rounds", type=int, default=5, help="interleaved rounds per case")
    p.add_argument("--filter", default="", help="only cases whose name matches this regular expression")
    p.add_argument("--fail-above", type=float, metavar="PCT", help="fail when a case is slower by more than PCT percent")
    p.add_argument("--out", type=Path, help="also write the table here")
    args = p.parse_args()
    if args.rounds < 2:
        p.error("--rounds must be at least 2")
    base_name = revision(args.base)
    head_name = revision(args.head) if args.head else "this checkout"
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        trees = []
        try:
            sides = {}
            for side, rev in (("base", args.base), ("head", args.head)):
                tree = ROOT
                if rev is not None:
                    tree = tmp / side
                    subprocess.run(["git", "worktree", "add", "-q", "--detach", str(tree), rev], cwd=ROOT, check=True)
                    trees.append(tree)
                print(f"building {side} ({rev or 'this checkout'})", flush=True)
                sides[side] = build(tree, ROOT / "target/perf-ab" / side)
        finally:
            for tree in trees:
                subprocess.run(["git", "worktree", "remove", "--force", str(tree)], cwd=ROOT, check=False)
        scratch = tmp / "scripts"
        scratch.mkdir()
        vms = [Vm(c, w, scratch) for c in CASES for w in WIDTHS if re.search(args.filter, f"vm/{c}/{w}")]
        samples = {"base": {}, "head": {}}
        started = time.monotonic()
        for vm in vms:
            vm.calibrate(sides["base"])
        for round_index in range(args.rounds):
            order = ("base", "head") if round_index % 2 == 0 else ("head", "base")
            for job in [*(vm.measure for vm in vms), instances, depth, tables]:
                for side in order:
                    for name, value in job(sides[side]).items():
                        if re.search(args.filter, name):
                            samples[side].setdefault(name, []).append(value)
            print(f"round {round_index + 1}/{args.rounds} ({time.monotonic() - started:.0f}s)", flush=True)
    names = list(samples["base"])
    rows = table(samples, names)
    text = render(rows, base_name, head_name, args.rounds, args.fail_above)
    print(text)
    for path in filter(None, [args.out, os.environ.get("GITHUB_STEP_SUMMARY")]):
        with open(path, "a") as f:
            f.write(text)
    if args.fail_above is not None:
        slower = [r for r in rows if r["change"] * 100 > args.fail_above]
        if slower:
            print(f"{len(slower)} case(s) slower than {args.fail_above}%: " + ", ".join(r["name"] for r in slower),
                  file=sys.stderr)
            return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
