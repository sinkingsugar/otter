#!/usr/bin/env python3
"""The instruction gate (docs/performance-gates.md): instructions and
allocations per iteration of each workload, against a committed baseline.

Each workload runs at N and at 2N iterations through the driver
(`crates/otter-lang/examples/bench_gate.rs`) under Cachegrind, and the gate
uses (count(2N) - count(N)) / N: process start, parsing, compose and teardown
cancel, leaving the steady-state cost of one iteration. The driver counts
allocations itself, so the same run gives both.

  run.py --check bench/instructions/baseline.txt   fail on any regression
  run.py --write bench/instructions/baseline.txt   regenerate (say why in the commit)
  run.py --jitter 5                                spread across repeated runs
  run.py --allocations-only --check ...            natively, without Valgrind

Instruction counts depend on the exact Valgrind and C library build, so a
baseline is only comparable in the environment it was written in (its header
records it, and --check refuses a mismatch). The committed baseline is CI's
(ubuntu-24.04); locally, write one at the base revision and check against it
after the change. Python 3 standard library only.
"""

import argparse
from concurrent.futures import ThreadPoolExecutor
import os
from pathlib import Path
import platform
import re
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "bench/vm-execution"))
from workloads import CASES, dialect, parse_output, script  # noqa: E402

DRIVER = ROOT / "target/release/examples/bench_gate"
WIDTHS = (8, 64, 256)
TABLE_CASES = ("const-take-16", "var-key-take-16", "make-take-4", "make-take-16", "rebuild-16", "chain-16")
# Instructions per iteration may rise by this fraction before the check fails;
# allocations and bytes may not rise at all.
TOLERANCE = 0.0
# A fixed cache geometry for Cachegrind's simulators, rather than one probed
# from the CPU the run starts on: probing differs between machines, and on
# hybrid CPUs between a performance and an efficiency core of the same one.
CACHES = ["--I1=32768,8,64", "--D1=32768,8,64", "--LL=8388608,16,64"]
# Measured per iteration. `ir` is gated with TOLERANCE, `allocs` and `bytes`
# exactly; the cache and branch columns model a generic machine (Cachegrind's
# simulators), so they are recorded and reported, not gated.
COLUMNS = ("ir", "allocs", "bytes", "d1_misses", "ll_misses", "mispredicts")
GATED = ("ir", "allocs", "bytes")


class Workload:
    """A named run of the driver: `argv(n)` runs `n` iterations; a VM script
    also has its output checked."""

    def __init__(self, name, n, argv, vm=None):
        self.name, self.n, self.argv, self.vm = name, n, argv, vm


def workloads(scratch):
    found = []
    for case in CASES:
        for width in WIDTHS:
            def argv(n, case=case, width=width):
                path = scratch / f"{case}-{width}-{n}.shs"
                path.write_text(dialect(script(case, width, n, 1, timed=False), "2"))
                return ["script", str(path)]
            found.append(Workload(f"vm/{case}/{width}", 1000, argv, vm=True))
    found.append(Workload("entity/100", 100, lambda n: ["entity", "100", str(n)]))
    for depth in (1, 16):
        found.append(Workload(f"depth/{depth}", 100, lambda n, d=depth: ["depth", str(d), "100", str(n)]))
    for case in TABLE_CASES:
        found.append(Workload(f"table/{case}", 1000, lambda n, c=case: ["table", c, str(n)]))
    found.append(Workload("reload", 10, lambda n: ["reload", str(n)]))
    return found


def capture(command):
    try:
        return subprocess.run(command, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT).stdout.strip()
    except FileNotFoundError:
        return "not available"


def environment(valgrind):
    """What instruction counts depend on, besides the source: recorded in the
    baseline and compared before a check."""
    target = re.search(r"host: (\S+)", capture(["rustc", "-vV"]))
    # ldd names the distribution's package revision, which libc_ver does not.
    libc = capture(["ldd", "--version"]).splitlines()[0] or " ".join(platform.libc_ver())
    env = {"rustc": capture(["rustc", "-V"]), "target": target.group(1) if target else "unknown", "libc": libc}
    if valgrind:
        env["valgrind"] = capture(["valgrind", "--version"])
    return env


def measure(driver, workload, n, scratch, valgrind):
    """Totals of one run at `n` iterations, keyed by COLUMNS."""
    command = [str(driver), *workload.argv(n)]
    out = scratch / f"{workload.name.replace('/', '_')}-{n}.cg"
    if valgrind:
        command = ["valgrind", "--tool=cachegrind", "--cache-sim=yes", "--branch-sim=yes", *CACHES,
                   f"--cachegrind-out-file={out}", *command]
    p = subprocess.run(command, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if p.returncode:
        raise RuntimeError(f"{workload.name} at {n} failed ({p.returncode}): {' '.join(command)}\n"
                           f"{p.stdout[-3000:]}\n{p.stderr[-3000:]}")
    if workload.vm:
        parse_output(p.stdout, n, 1, timed=False)
    totals = {
        "allocs": int(re.search(r"GATE_ALLOCS: (\d+)", p.stdout).group(1)),
        "bytes": int(re.search(r"GATE_BYTES: (\d+)", p.stdout).group(1)),
    }
    if valgrind:
        text = out.read_text()
        events = re.search(r"^events: (.+)$", text, re.M).group(1).split()
        counts = dict(zip(events, map(int, re.search(r"^summary: (.+)$", text, re.M).group(1).split())))
        totals["ir"] = counts["Ir"]
        totals["d1_misses"] = counts["D1mr"] + counts["D1mw"]
        totals["ll_misses"] = counts["ILmr"] + counts["DLmr"] + counts["DLmw"]
        totals["mispredicts"] = counts["Bcm"] + counts["Bim"]
    return totals


def per_iteration(driver, workload, scratch, valgrind):
    n = workload.n
    # Equal digit counts keep the scripts and arguments the same length.
    assert len(str(n)) == len(str(2 * n)), workload.name
    one, two = (measure(driver, workload, k, scratch, valgrind) for k in (n, 2 * n))
    return {column: (two[column] - one[column]) / n for column in one}


def run_all(driver, selected, scratch, valgrind, jobs):
    with ThreadPoolExecutor(max_workers=jobs) as pool:
        futures = {w.name: pool.submit(per_iteration, driver, w, scratch, valgrind) for w in selected}
        return {name: future.result() for name, future in futures.items()}


def write_baseline(path, results, env):
    lines = ["# Instructions and allocations per iteration (bench/instructions/run.py --write).",
             "# Regenerate after an intended change and say why in the commit.",
             *(f"# {key}: {value}" for key, value in env.items()),
             "# workload " + " ".join(COLUMNS)]
    for name, row in results.items():
        lines.append(" ".join([name, *(f"{row[c]:.2f}" for c in COLUMNS)]))
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("\n".join(lines) + "\n")


def read_baseline(path):
    env, rows = {}, {}
    for line in path.read_text().splitlines():
        if m := re.match(r"# (\w+): (.*)$", line):
            env[m.group(1)] = m.group(2)
        elif line and not line.startswith("#"):
            name, *values = line.split()
            rows[name] = dict(zip(COLUMNS, map(float, values)))
    return env, rows


def check(results, baseline, gated):
    """Prints every change; returns the regressions."""
    regressions = []
    for name, row in results.items():
        base = baseline.get(name)
        if base is None:
            regressions.append(f"{name}: not in the baseline (regenerate it)")
            continue
        changes = []
        for column in COLUMNS:
            if column not in row:
                continue
            old, new = base[column], round(row[column], 2)
            if new == old:
                continue
            delta = f"{column} {old:.2f} -> {new:.2f}" + (f" ({(new - old) / old:+.2%})" if old else "")
            limit = old * (1 + TOLERANCE) if column == "ir" else old
            if column in gated and new > limit:
                regressions.append(f"{name}: {delta}")
                delta += "  REGRESSION"
            changes.append(delta)
        if changes:
            print(f"{name}: " + "; ".join(changes))
    for name in baseline.keys() - results.keys():
        regressions.append(f"{name}: in the baseline but not measured (regenerate it)")
    return regressions


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    mode = p.add_mutually_exclusive_group(required=True)
    mode.add_argument("--check", type=Path, metavar="BASELINE")
    mode.add_argument("--write", type=Path, metavar="BASELINE")
    mode.add_argument("--jitter", type=int, metavar="RUNS", help="run every workload RUNS times and report the spread")
    p.add_argument("--allocations-only", action="store_true", help="run natively, without Valgrind: allocations and bytes only")
    p.add_argument("--ignore-environment", action="store_true", help="check against a baseline from another environment")
    p.add_argument("--filter", default="", help="only workloads whose name matches this regular expression")
    p.add_argument("--jobs", type=int, default=os.cpu_count(), help="parallel runs (counts do not depend on it)")
    p.add_argument("--driver", type=Path, default=DRIVER, help="a prebuilt driver; builds the default one otherwise")
    p.add_argument("--fresh", type=Path, help="with --check, also write the measured baseline here (CI's artifact)")
    args = p.parse_args()
    valgrind = not args.allocations_only
    if args.write and not valgrind:
        p.error("--write needs the instruction counts; drop --allocations-only")
    if args.driver == DRIVER:
        subprocess.run(["cargo", "build", "--release", "-p", "otter-lang", "--example", "bench_gate"],
                       cwd=ROOT, check=True)
    env = environment(valgrind)
    with tempfile.TemporaryDirectory() as tmp:
        scratch = Path(tmp)
        selected = [w for w in workloads(scratch) if re.search(args.filter, w.name)]
        if not selected:
            p.error("no workload matches --filter")
        if args.jitter:
            runs = [run_all(args.driver, selected, scratch, valgrind, args.jobs) for _ in range(args.jitter)]
            steady = True
            for w in selected:
                for column in runs[0][w.name]:
                    values = [r[w.name][column] for r in runs]
                    if max(values) != min(values):
                        steady = False
                        print(f"{w.name} {column}: {min(values):.3f} .. {max(values):.3f}")
            print("no jitter" if steady else "jitter found")
            return 0 if steady else 1
        results = run_all(args.driver, selected, scratch, valgrind, args.jobs)
    if args.write:
        if args.filter:
            p.error("--write records every workload; drop --filter")
        write_baseline(args.write, results, env)
        print(f"wrote {len(results)} workloads to {args.write}")
        return 0
    if args.fresh:
        write_baseline(args.fresh, results, env)
    if not args.check.exists():
        print(f"{args.check} does not exist; write it with --write" +
              (f", or commit {args.fresh}" if args.fresh else ""), file=sys.stderr)
        return 1
    base_env, baseline = read_baseline(args.check)
    if args.filter:
        baseline = {k: v for k, v in baseline.items() if re.search(args.filter, k)}
    mismatch = [f"{k}: baseline {base_env.get(k)!r}, here {v!r}" for k, v in env.items() if base_env.get(k) != v]
    gated = GATED if valgrind else ("allocs", "bytes")
    if mismatch and valgrind and not args.ignore_environment:
        print("The baseline was written in another environment; instruction counts are not comparable:\n  "
              + "\n  ".join(mismatch)
              + "\nUse --allocations-only, --ignore-environment, or a baseline written here.", file=sys.stderr)
        return 2
    regressions = check(results, baseline, gated)
    if regressions:
        print(f"\n{len(regressions)} regression(s) against {args.check}:\n  " + "\n  ".join(regressions), file=sys.stderr)
        return 1
    print(f"{len(results)} workloads: no regression against {args.check}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
