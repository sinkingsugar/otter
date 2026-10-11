#!/usr/bin/env python3
"""Matched, non-suspending VM workloads; Python 3 standard library only."""

import argparse
import csv
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import random
import re
import statistics
import shutil
import subprocess
import sys
import time

from workloads import CASES, dialect, parse_output, script

ROOT = Path(__file__).resolve().parents[2]


def run(command, path, iterations, batches, timeout):
    p = subprocess.run([*command, str(path)], text=True, stdout=subprocess.PIPE,
                       stderr=subprocess.STDOUT, timeout=timeout)
    if p.returncode:
        raise RuntimeError(f"command failed ({p.returncode}): {command}\n{p.stdout[-6000:]}")
    return parse_output(p.stdout, iterations, batches)


def capture(command, cwd=None):
    try:
        p = subprocess.run(command, cwd=cwd, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    except FileNotFoundError:
        return f"{command[0]}: not available on this platform"
    return p.stdout.strip()


def sha256(path):
    h = hashlib.sha256()
    with path.open("rb") as f:
        for block in iter(lambda: f.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


def provenance(args, engines):
    info = {
        "started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "platform": platform.platform(), "python": sys.version,
        "arguments": {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
        "commands": engines, "binaries": {}, "repositories": {},
        "rustc": capture(["rustc", "-Vv"], ROOT),
        "cpu": capture(["lscpu"]) if shutil.which("lscpu") else capture(["sysctl", "-n", "machdep.cpu.brand_string"]),
        "affinity": sorted(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else None,
        "script_generator_sha256": sha256(Path(__file__).with_name("workloads.py")),
        "harness_sha256": sha256(Path(__file__)),
    }
    for label, binary in [("1x", args.shards1), ("2", args.otter)]:
        info["binaries"][label] = {"path": str(binary), "sha256": sha256(binary), "bytes": binary.stat().st_size}
    for label, repo in [("2", ROOT), ("1x", args.source1)]:
        if repo:
            info["repositories"][label] = {
                "head": capture(["git", "rev-parse", "HEAD"], repo),
                "status": capture(["git", "status", "--short", "--untracked-files=no"], repo),
                "tracked_diff_sha256": hashlib.sha256(capture(["git", "diff", "HEAD"], repo).encode()).hexdigest(),
            }
    cache = args.shards1.parent / "CMakeCache.txt"
    if cache.exists():
        info["cmake_settings"] = [line for line in cache.read_text().splitlines()
                                  if re.match(r"(CMAKE_(BUILD_TYPE|CXX_(FLAGS|COMPILER))|SHARDS_(INLINE|DEBUGGER)|TRACY_ENABLE)", line)]
    for name in ("scaling_governor", "scaling_driver", "cpuinfo_max_freq"):
        path = Path(f"/sys/devices/system/cpu/cpu{args.cpu or 0}/cpufreq/{name}")
        if path.exists():
            info[name] = path.read_text().strip()
    return info


def summarize(rows, out):
    grouped = {}
    for row in rows:
        if not row["warmup"]:
            grouped.setdefault((row["case"], row["width"], row["engine"]), []).append(row["ns_per_iteration"])
    summary = []
    for (case, width, engine), values in sorted(grouped.items()):
        med = statistics.median(values)
        summary.append(dict(case=case, width=width, engine=engine, samples=len(values),
                            median_ns=med, min_ns=min(values), max_ns=max(values),
                            mad_ns=statistics.median(abs(v - med) for v in values)))
    with (out / "summary.csv").open("w") as f:
        writer = csv.DictWriter(f, fieldnames=list(summary[0]), lineterminator="\n")
        writer.writeheader()
        writer.writerows(summary)
    # A fitted slope removes constant loop/sink overhead. These are costs per
    # repeated source motif, NOT per machine instruction or necessarily per shard.
    slopes = []
    for case, engine in sorted({(s["case"], s["engine"]) for s in summary}):
        points = [(s["width"], s["median_ns"]) for s in summary if s["case"] == case and s["engine"] == engine]
        if len(points) < 2:
            continue
        xbar, ybar = statistics.mean(x for x, _ in points), statistics.mean(y for _, y in points)
        xx = sum((x - xbar) ** 2 for x, _ in points)
        slope = sum((x - xbar) * (y - ybar) for x, y in points) / xx
        intercept = ybar - slope * xbar
        residual = sum((y - (intercept + slope * x)) ** 2 for x, y in points)
        total = sum((y - ybar) ** 2 for _, y in points)
        slopes.append(dict(case=case, engine=engine, ns_per_motif=slope, intercept_ns=intercept,
                           r_squared=1 - residual / total if total else 1.0))
    if slopes:
        with (out / "slopes.csv").open("w") as f:
            writer = csv.DictWriter(f, fieldnames=list(slopes[0]), lineterminator="\n")
            writer.writeheader()
            writer.writerows(slopes)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("shards1", type=Path, help="fresh Release 1.x executable (inline dispatch enabled)")
    p.add_argument("--otter", type=Path, default=ROOT / "target/release/otter")
    p.add_argument("--source1", type=Path, help="1.x checkout, for provenance")
    p.add_argument("--out", type=Path, required=True, help="new output directory, including generated scripts")
    p.add_argument("--cpu", type=int, help="pin this process and children to one allowed CPU (Linux)")
    p.add_argument("--cases", nargs="+", choices=CASES, default=CASES)
    p.add_argument("--widths", nargs="+", type=int, default=[0, 8, 64, 256])
    p.add_argument("--rounds", type=int, default=3, help="independent processes per engine/case/width")
    p.add_argument("--samples", type=int, default=3, help="timed samples per process after warmup")
    p.add_argument("--warmup", type=int, default=2, help="discarded batches through the same instantiated body")
    p.add_argument("--target-ms", type=float, default=20, help="target duration for fastest engine")
    p.add_argument("--max-ms", type=float, default=250, help="cap estimated batch duration for slowest engine")
    p.add_argument("--iterations", type=int, help="fixed iterations, skipping calibration (e.g. smoke checks)")
    p.add_argument("--timeout", type=float, default=120)
    p.add_argument("--seed", type=int, default=7529)
    args = p.parse_args()
    if (min(args.rounds, args.samples) < 1 or args.warmup < 0 or min(args.widths) < 0
            or len(set(args.widths)) != len(args.widths) or (args.iterations is not None and args.iterations < 1)
            or not 0 < args.target_ms <= args.max_ms):
        p.error("invalid counts, widths or timing targets")
    args.shards1, args.otter = args.shards1.resolve(), args.otter.resolve()
    if args.cpu is not None:
        os.sched_setaffinity(0, {args.cpu})
    args.out.mkdir(parents=True, exist_ok=False)
    scripts = args.out / "scripts"
    scripts.mkdir()
    engines = {"1x": [str(args.shards1)], "2-stackless": [str(args.otter), "run"]}
    (args.out / "metadata.json").write_text(json.dumps(provenance(args, engines), indent=2) + "\n")
    rng = random.Random(args.seed)
    jobs = [(case, width) for case in args.cases for width in args.widths]
    rng.shuffle(jobs)
    rows, calibration = [], []
    fields = ["case", "width", "engine", "round", "batch", "warmup", "iterations", "seconds", "ns_per_iteration"]
    with (args.out / "samples.csv").open("w") as f:
        writer = csv.DictWriter(f, fieldnames=fields, lineterminator="\n")
        writer.writeheader()
        for job, (case, width) in enumerate(jobs, 1):
            paths = {engine: scripts / f"{case}-{width}-{engine.split('-')[0]}.shs" for engine in engines}

            def write(iterations, batches):
                for engine, path in paths.items():
                    path.write_text(dialect(script(case, width, iterations, batches), engine))

            iterations = args.iterations
            if iterations is None:
                probe = 10000
                write(probe, 2)
                rates = []
                for engine, command in engines.items():
                    times = run(command, paths[engine], probe, 2, args.timeout)
                    rates.append(times[-1] / probe)
                    calibration.append(dict(case=case, width=width, engine=engine, iterations=probe, seconds=times))
                iterations = max(100, min(100_000_000, math.ceil(min(args.target_ms / 1000 / min(rates),
                                                                          args.max_ms / 1000 / max(rates)))))
            batches = args.warmup + args.samples
            write(iterations, batches)
            for round_index in range(args.rounds):
                order = list(engines)
                rng.shuffle(order)
                for engine in order:
                    times = run(engines[engine], paths[engine], iterations, batches, args.timeout)
                    for batch, seconds in enumerate(times):
                        row = dict(case=case, width=width, engine=engine, round=round_index, batch=batch,
                                   warmup=batch < args.warmup, iterations=iterations, seconds=seconds,
                                   ns_per_iteration=seconds * 1e9 / iterations)
                        rows.append(row)
                        writer.writerow(row)
                    f.flush()
            print(f"[{job}/{len(jobs)}] {case} width={width} iterations={iterations}", flush=True)
    (args.out / "calibration.json").write_text(json.dumps(calibration, indent=2) + "\n")
    summarize(rows, args.out)
    print(f"Verified all results and iteration counts. Results: {args.out}")


if __name__ == "__main__":
    main()
