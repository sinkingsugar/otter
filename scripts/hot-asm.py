#!/usr/bin/env python3
"""Disassembles the runtime's hot functions and summarizes them, to check a
change to the interpreter loop or the engine step before benchmarking it
(docs/runtime-performance-overview.md, lesson 6).

    python3 scripts/hot-asm.py                      # this checkout
    python3 scripts/hot-asm.py --compare a965e32    # and a revision, side by side
    python3 scripts/hot-asm.py --check bench/hot-asm/aarch64-apple-darwin.txt
    python3 scripts/hot-asm.py --check ... --fresh fresh.txt   # also write the summary (CI's artifact)

Builds `otter` in release (in CARGO_TARGET_DIR, or the checkout's target),
finds each function below by its mangled name, and reports its instruction
count, stack loads and stores (spills and reloads), indirect branches, and
the functions it calls rather than inlines. The full disassembly, normalized
(addresses, immediates and symbol names stripped, so only code changes show),
goes to --out. With --compare, the revision is built in a temporary worktree
and the normalized `run` of both is diffed.

Standard library only; needs an LLVM objdump (macOS's `objdump`, or
`llvm-objdump` elsewhere) and `nm`.
"""
import argparse
import difflib
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# (label, regular expression on the mangled symbol)
FUNCTIONS = [
    ("run (runtime)", r"inline3runNtNtNtB4_9stackless6engine11EngineCallsEB4_$"),
    ("run (in-step child flows)", r"inline3runNtB2_7NoCallsEB4_$"),
    ("run (metered)", r"inline3runINtB2_7MeteredNtNtNtB4_9stackless6engine11EngineCallsEEB4_$"),
    ("construct", r"inline9construct"),
    ("vm_call_op (runtime)", r"inline10vm_call_opNtNtNtB4_9stackless6engine11EngineCallsEB4_$"),
    ("Engine::activate", r"6Engine8activate"),
    # The runtime's step loop (`steps::<false, false>`: not metered, not
    # locating); the evaluation's two instantiations are separate.
    ("engine steps (runtime)", r"6Engine5stepsKb0_K"),
]
RUN = "run (runtime)"


def objdump():
    for tool in ("llvm-objdump", "objdump"):
        path = shutil.which(tool)
        if path and "LLVM" in subprocess.run([path, "--version"], capture_output=True, text=True).stdout:
            return path
    sys.exit("hot-asm: needs an LLVM objdump (llvm-objdump, or macOS's objdump)")


def build(root, target_dir):
    env = dict(os.environ, CARGO_TARGET_DIR=str(target_dir))
    subprocess.run(["cargo", "build", "--release", "-q", "-p", "otter-cli"], cwd=root, env=env, check=True)
    return target_dir / "release" / "otter"


def symbols(binary):
    out = subprocess.run(["nm", str(binary)], capture_output=True, text=True, check=True).stdout
    return [line.split()[-1] for line in out.splitlines() if line.strip()]


def demangler():
    for tool in ("llvm-cxxfilt", "c++filt"):
        path = shutil.which(tool)
        if path:
            return path
    return None


def demangle_all(names):
    """Demangled Rust names (v0), when a demangler is present; else as is."""
    tool = demangler()
    plain = [n[1:] if n.startswith("__R") else n for n in names]
    if not tool or not plain:
        return dict(zip(names, plain))
    out = subprocess.run([tool], input="\n".join(plain) + "\n", capture_output=True, text=True).stdout
    return dict(zip(names, out.splitlines()))


def short(name):
    """A crate path without the crate name and its hash."""
    name = re.sub(r"\[[0-9a-f]+\]", "", name)
    return name.replace("otter_core::", "")


def disassemble(tool, binary, symbol):
    out = subprocess.run([tool, "-d", "--no-show-raw-insn", f"--disassemble-symbols={symbol}", str(binary)],
                         capture_output=True, text=True).stdout
    lines = []
    for line in out.splitlines():
        m = re.match(r"\s*[0-9a-f]+:\s+(.*)", line)
        # Alignment padding (.cargo/config.toml aligns branch targets) is not code.
        if m and not re.match(r"nop\b", m.group(1).strip()):
            lines.append(m.group(1).strip())
    return lines


def normalize(lines):
    out = []
    for line in lines:
        line = re.sub(r"<[^>]*>", "<sym>", line)
        line = re.sub(r"0x[0-9a-f]+", "N", line)
        line = re.sub(r"#-?\d+", "#N", line)
        out.append(line)
    return out


def summarize(lines):
    targets = []
    for line in lines:
        m = re.match(r"(bl|call[q]?)\s+\S+\s+<([^>+]*)", line)
        if m:
            targets.append(m.group(2))
    names = demangle_all(sorted(set(targets)))
    calls, other = set(), 0
    for target in targets:
        name = names.get(target, target)
        if "otter_core::" in name:
            calls.add(short(name))
        else:
            other += 1
    # aarch64: ldr/ldp/ldur from [sp...]; x86-64 (AT&T): a move from or to (%rsp).
    loads = r"^(ldr|ldp|ldur)\S*\s.*\[sp|^mov\S*\s+-?\w*\(%rsp\),"
    stores = r"^(str|stp|stur)\S*\s.*\[sp|^mov\S*\s+%\w+,\s*-?\w*\(%rsp\)"
    return {
        "instructions": len(lines),
        "stack loads": sum(1 for line in lines if re.search(loads, line)),
        "stack stores": sum(1 for line in lines if re.search(stores, line)),
        "indirect branches": sum(1 for l in lines if re.match(r"(br\s|jmp[q]?\s+\*)", l)),
        "calls": sorted(calls),
        "other calls": other,
    }


def analyze(binary, out_dir, tool):
    syms = symbols(binary)
    report = {}
    for label, pattern in FUNCTIONS:
        found = [s for s in syms if re.search(pattern, s)]
        if not found:
            report[label] = None
            continue
        lines = disassemble(tool, binary, found[0])
        if out_dir:
            safe = re.sub(r"[^a-z0-9]+", "-", label.lower()).strip("-")
            (out_dir / f"{safe}.s").write_text("\n".join(normalize(lines)) + "\n")
        report[label] = (summarize(lines), normalize(lines))
    return report


def render(report):
    out = []
    for label, _ in FUNCTIONS:
        entry = report.get(label)
        if entry is None:
            out.append(f"{label}: not found (inlined, or renamed)")
            continue
        s = entry[0]
        out.append(f"{label}: {s['instructions']} instructions, {s['stack loads']} stack loads, "
                   f"{s['stack stores']} stack stores, {s['indirect branches']} indirect branches")
        out.append(f"  calls {', '.join(s['calls']) or 'nothing in otter_core'}; "
                   f"{s['other calls']} call sites outside it (allocation, drops, panics)")
    return "\n".join(out) + "\n"


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--compare", help="a revision to build and compare with this checkout")
    p.add_argument("--check", type=Path, help="a stored summary this checkout must match")
    p.add_argument("--out", type=Path, help="directory for the normalized disassembly")
    p.add_argument("--fresh", type=Path, help="also write this checkout's summary here (a baseline to commit)")
    args = p.parse_args()
    tool = objdump()
    target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
    if args.out:
        args.out.mkdir(parents=True, exist_ok=True)
    current = analyze(build(ROOT, target), args.out, tool)
    text = render(current)
    print(text, end="")
    if args.fresh:
        args.fresh.parent.mkdir(parents=True, exist_ok=True)
        args.fresh.write_text(text)
    if args.compare:
        with tempfile.TemporaryDirectory(prefix="hot-asm-") as tmp:
            tree = Path(tmp) / "tree"
            subprocess.run(["git", "worktree", "add", "-q", "--detach", str(tree), args.compare], cwd=ROOT, check=True)
            try:
                other_out = args.out / args.compare if args.out else None
                if other_out:
                    other_out.mkdir(parents=True, exist_ok=True)
                other = analyze(build(tree, Path(tmp) / "target"), other_out, tool)
            finally:
                subprocess.run(["git", "worktree", "remove", "--force", str(tree)], cwd=ROOT, check=True)
        print(f"\n--- {args.compare}\n" + render(other), end="")
        if current.get(RUN) and other.get(RUN):
            diff = list(difflib.unified_diff(other[RUN][1], current[RUN][1], args.compare, "this checkout",
                                             lineterm="", n=1))
            changed = sum(1 for d in diff if d[:1] in "+-" and not d.startswith(("+++", "---")))
            print(f"\nrun (runtime), normalized: {changed} lines differ")
            for line in diff[:200]:
                print(line)
    if args.check:
        stored = args.check.read_text()
        if stored != text:
            print(f"\nhot-asm: differs from {args.check}:")
            for line in difflib.unified_diff(stored.splitlines(), text.splitlines(), str(args.check), "now",
                                             lineterm=""):
                print(line)
            sys.exit(1)
        print(f"hot-asm: matches {args.check}")


if __name__ == "__main__":
    main()
