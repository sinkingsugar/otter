#!/usr/bin/env python3
"""The instruction gate (docs/performance-gates.md): instructions and
allocations per iteration of each workload, against a committed baseline.

Each workload runs at N and at 2N iterations through the driver
(`crates/otter-lang/examples/bench_gate.rs`) under Cachegrind, and the gate
uses (count(2N) - count(N)) / N: process start, parsing, compose and teardown
cancel, leaving the steady-state cost of one iteration. The driver counts
allocations itself, so the same run gives both.

  run.py --check            fail on any regression against this target's baseline
  run.py --write            regenerate it (say why in the commit)
  run.py --check FILE       against another baseline (e.g. one written at the base)
  run.py --jitter 5                                spread across repeated runs
  run.py --allocations-only --check ...            natively, without Valgrind
  run.py --qemu aarch64 --write /tmp/before.txt    aarch64 counts on an x86-64 machine (QEMU)

Instruction counts depend on the exact Valgrind and C library build, so a
baseline is only comparable in the environment it was written in (its header
records it, and --check refuses a mismatch). The committed baselines are CI's,
one per target (bench/instructions/TARGET.txt, from ubuntu-24.04 and
ubuntu-24.04-arm); locally, write one at the base revision and check
against it after the change. Python 3 standard library only.
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

# The gate's builds: one codegen unit, in their own target directory. With
# rustc's default sixteen, editing one function moved the counts of
# unedited engine functions by 50 to 500 instructions per iteration (how
# the crate is cut into units changes what is inlined); one unit measures
# the source. Its own directory, so gate and ordinary release builds do
# not rebuild each other.
GATE_TARGET = ROOT / "target/gate"
GATE_ENV = {"CARGO_PROFILE_RELEASE_CODEGEN_UNITS": "1", "CARGO_TARGET_DIR": str(GATE_TARGET)}
DRIVER = GATE_TARGET / "release/examples/bench_gate"
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
# `--check`/`--write` without a file: this host's target's baseline.
HOST = Path("<host target>")


# The snippets the gate runs, by name (`common/snippets.rs`).
SNIPPETS = ("path-static", "path-dynamic", "path-lenient", "repeat-take", "foreach-sum", "foreach-named",
            "repeat-push", "map-256", "filter-256", "reduce-256", "table-foreach")


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
    for case in ("assign", "seq", "table", "lookup"):
        name = f"service/{case}" if case == "lookup" else f"object/{case}"
        found.append(Workload(name, 1000, lambda n, c=case: ["object", c, str(n)]))
    # Script snippets (crates/otter-lang/examples/common/snippets.rs): paths, and
    # the iteration shards beside the hand-written loops they replace.
    for case in SNIPPETS:
        found.append(Workload(f"snippet/{case}", 1000, lambda n, c=case: ["snippet", c, str(n)]))
    return found


def capture(command):
    try:
        return subprocess.run(command, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT).stdout.strip()
    except FileNotFoundError:
        return "not available"


# `--qemu ARCH`: the driver cross-built for ARCH and run under QEMU's
# user-mode emulator, counting executed instructions with a plugin
# (`qemu/insncount.c`). Exact and deterministic like Cachegrind, but it
# counts the target's own C library and simulates no caches: a local
# stand-in for CI's job on that architecture, to compare two revisions on
# one machine, not to check CI's baselines.
QEMU = {
    "aarch64": {"target": "aarch64-unknown-linux-gnu", "sysroot": "/usr/aarch64-linux-gnu",
                "linker": "aarch64-linux-gnu-gcc", "emulator": "qemu-aarch64"},
}
PLUGIN = ROOT / "target/insncount/libinsncount.so"


def environment(counter, qemu):
    """What instruction counts depend on, besides the source: recorded in the
    baseline and compared before a check."""
    target = re.search(r"host: (\S+)", capture(["rustc", "-vV"]))
    # ldd names the distribution's package revision, which libc_ver does not.
    libc = capture(["ldd", "--version"]).splitlines()[0] or " ".join(platform.libc_ver())
    env = {"rustc": capture(["rustc", "-V"]), "target": target.group(1) if target else "unknown", "libc": libc,
           "codegen-units": GATE_ENV["CARGO_PROFILE_RELEASE_CODEGEN_UNITS"]}
    if counter == "cachegrind":
        env["valgrind"] = capture(["valgrind", "--version"])
    elif counter == "qemu":
        env["target"] = qemu["target"] + "-qemu"
        env["libc"] = "glibc " + capture([qemu["emulator"], "-L", qemu["sysroot"],
                                          qemu["sysroot"] + "/lib/libc.so.6"]).splitlines()[0]
        env["qemu"] = capture([qemu["emulator"], "--version"]).splitlines()[0]
    return env


def measure(driver, workload, n, scratch, counter, qemu):
    """Totals of one run at `n` iterations, keyed by COLUMNS (those the
    counter measures)."""
    command = [str(driver), *workload.argv(n)]
    out = scratch / f"{workload.name.replace('/', '_')}-{n}.cg"
    if counter == "cachegrind":
        # `--vgdb=no`: Valgrind otherwise maps a file named after the
        # process id, and the C library reads the process's maps at start
        # (the main thread's stack bounds), so a pid one digit longer cost
        # 13 more instructions in some runs.
        command = ["valgrind", "--tool=cachegrind", "--vgdb=no", "--cache-sim=yes", "--branch-sim=yes", *CACHES,
                   f"--cachegrind-out-file={out}", *command]
    elif counter == "qemu":
        # Without address randomization: the target's malloc, counted here
        # (Cachegrind substitutes its own), takes paths that depend on
        # addresses, and the counts varied between runs.
        command = ["setarch", "-R", qemu["emulator"], "-L", qemu["sysroot"], "-plugin",
                   f"{PLUGIN},out={out}", *command]
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
    if counter == "qemu":
        totals["ir"] = int(out.read_text())
    elif counter == "cachegrind":
        text = out.read_text()
        events = re.search(r"^events: (.+)$", text, re.M).group(1).split()
        counts = dict(zip(events, map(int, re.search(r"^summary: (.+)$", text, re.M).group(1).split())))
        totals["ir"] = counts["Ir"]
        totals["d1_misses"] = counts["D1mr"] + counts["D1mw"]
        totals["ll_misses"] = counts["ILmr"] + counts["DLmr"] + counts["DLmw"]
        totals["mispredicts"] = counts["Bcm"] + counts["Bim"]
    return totals


def per_iteration(driver, workload, scratch, counter, qemu):
    n = workload.n
    # Equal digit counts keep the scripts and arguments the same length.
    assert len(str(n)) == len(str(2 * n)), workload.name
    one, two = (measure(driver, workload, k, scratch, counter, qemu) for k in (n, 2 * n))
    return {column: (two[column] - one[column]) / n for column in one}


def run_all(driver, selected, scratch, counter, qemu, jobs):
    with ThreadPoolExecutor(max_workers=jobs) as pool:
        futures = {w.name: pool.submit(per_iteration, driver, w, scratch, counter, qemu) for w in selected}
        return {name: future.result() for name, future in futures.items()}


def write_baseline(path, results, env):
    lines = ["# Instructions and allocations per iteration (bench/instructions/run.py --write).",
             "# Regenerate after an intended change and say why in the commit.",
             *(f"# {key}: {value}" for key, value in env.items()),
             "# workload " + " ".join(COLUMNS)]
    for name, row in results.items():
        # A column the counter does not measure is `-` (QEMU: the caches).
        lines.append(" ".join([name, *(f"{row[c]:.2f}" if c in row else "-" for c in COLUMNS)]))
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("\n".join(lines) + "\n")


def read_baseline(path):
    env, rows = {}, {}
    for line in path.read_text().splitlines():
        if m := re.match(r"# ([\w-]+): (.*)$", line):
            env[m.group(1)] = m.group(2)
        elif line and not line.startswith("#"):
            name, *values = line.split()
            rows[name] = {c: float(v) for c, v in zip(COLUMNS, values) if v != "-"}
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
    mode.add_argument("--check", type=Path, nargs="?", const=HOST, metavar="BASELINE",
                      help="default: bench/instructions/TARGET.txt for this host")
    mode.add_argument("--write", type=Path, nargs="?", const=HOST, metavar="BASELINE",
                      help="default: bench/instructions/TARGET.txt for this host")
    mode.add_argument("--jitter", type=int, metavar="RUNS", help="run every workload RUNS times and report the spread")
    p.add_argument("--allocations-only", action="store_true", help="run natively, without Valgrind: allocations and bytes only")
    p.add_argument("--qemu", choices=sorted(QEMU), help="count instructions for this architecture under QEMU instead of Cachegrind")
    p.add_argument("--ignore-environment", action="store_true", help="check against a baseline from another environment")
    p.add_argument("--filter", default="", help="only workloads whose name matches this regular expression")
    p.add_argument("--jobs", type=int, default=os.cpu_count(), help="parallel runs (counts do not depend on it)")
    p.add_argument("--driver", type=Path, default=DRIVER, help="a prebuilt driver; builds the default one otherwise")
    p.add_argument("--fresh", type=Path, help="with --check, also write the measured baseline here (CI's artifact)")
    args = p.parse_args()
    qemu = QEMU.get(args.qemu)
    counter = None if args.allocations_only else ("qemu" if qemu else "cachegrind")
    if args.write and not counter:
        p.error("--write needs the instruction counts; drop --allocations-only")
    if qemu:
        if args.driver == DRIVER:
            env_vars = dict(os.environ, **GATE_ENV,
                            **{f"CARGO_TARGET_{qemu['target'].upper().replace('-', '_')}_LINKER": qemu["linker"]})
            subprocess.run(["cargo", "build", "--release", "-p", "otter-lang", "--example", "bench_gate",
                            "--target", qemu["target"]], cwd=ROOT, check=True, env=env_vars)
            args.driver = GATE_TARGET / qemu["target"] / "release/examples/bench_gate"
        PLUGIN.parent.mkdir(parents=True, exist_ok=True)
        cflags = capture(["pkg-config", "--cflags", "glib-2.0"]).split()
        subprocess.run(["cc", "-shared", "-fPIC", "-O2", *cflags, "-o", str(PLUGIN),
                        str(Path(__file__).with_name("qemu") / "insncount.c")], check=True)
    elif args.driver == DRIVER:
        subprocess.run(["cargo", "build", "--release", "-p", "otter-lang", "--example", "bench_gate"],
                       cwd=ROOT, check=True, env=dict(os.environ, **GATE_ENV))
    env = environment(counter, qemu)
    for mode in ("check", "write"):
        if getattr(args, mode) is HOST:
            setattr(args, mode, Path(__file__).with_name(f"{env['target']}.txt"))
    with tempfile.TemporaryDirectory() as tmp:
        scratch = Path(tmp)
        selected = [w for w in workloads(scratch) if re.search(args.filter, w.name)]
        if not selected:
            p.error("no workload matches --filter")
        if args.jitter:
            runs = [run_all(args.driver, selected, scratch, counter, qemu, args.jobs) for _ in range(args.jitter)]
            steady = True
            for w in selected:
                for column in runs[0][w.name]:
                    values = [r[w.name][column] for r in runs]
                    if max(values) != min(values):
                        steady = False
                        print(f"{w.name} {column}: {min(values):.3f} .. {max(values):.3f}")
            print("no jitter" if steady else "jitter found")
            return 0 if steady else 1
        results = run_all(args.driver, selected, scratch, counter, qemu, args.jobs)
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
    gated = GATED if counter else ("allocs", "bytes")
    if mismatch and counter and not args.ignore_environment:
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
