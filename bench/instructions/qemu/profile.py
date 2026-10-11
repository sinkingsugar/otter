#!/usr/bin/env python3
"""Per-function instructions per iteration on another architecture, from
QEMU block counts (`insnblocks.c`), by N/2N differencing as run.py does:

    profile.py DRIVER N -- WORKLOAD ARGS...   (N and 2N are substituted for `{n}`)

Prints the functions whose cost per iteration is at least 0.5, most first.
Standard library only; needs llvm-nm, qemu-aarch64 and the aarch64 sysroot.
"""
import bisect
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
PLUGIN = ROOT / "target/insncount/libinsnblocks.so"


def symbols(binary):
    out = subprocess.run(["llvm-nm", "-n", "-C", "--defined-only", str(binary)], text=True,
                         stdout=subprocess.PIPE, check=True).stdout
    table = []
    for line in out.splitlines():
        parts = line.split(" ", 2)
        if len(parts) == 3 and parts[1] in "tTwW":
            table.append((int(parts[0], 16), parts[2]))
    return table


def counts(binary, argv):
    with tempfile.TemporaryDirectory() as tmp:
        out = Path(tmp) / "blocks.txt"
        subprocess.run(["setarch", "-R", "qemu-aarch64", "-L", "/usr/aarch64-linux-gnu", "-plugin",
                        f"{PLUGIN},out={out}", str(binary), *argv], stdout=subprocess.DEVNULL, check=True)
        result, entry = {}, None
        for line in out.read_text().splitlines():
            pc, n = line.split()
            if pc == "text":
                entry = int(n, 16)
                continue
            result[int(pc, 16)] = result.get(int(pc, 16), 0) + int(n)
        return result, entry


def by_function(blocks, table, base):
    """Costs by function of the binary; blocks outside it (the C library,
    the dynamic loader) are one entry."""
    addrs = [a for a, _ in table]
    end = addrs[-1] + 0x10000
    per = {}
    for pc, n in blocks.items():
        off = pc - base
        if off < addrs[0] or off > end:
            name = "(outside the binary: libc, loader)"
        else:
            name = table[bisect.bisect_right(addrs, off) - 1][1]
        per[name] = per.get(name, 0) + n
    return per


def build_plugin():
    PLUGIN.parent.mkdir(parents=True, exist_ok=True)
    cflags = subprocess.run(["pkg-config", "--cflags", "glib-2.0"], text=True,
                            stdout=subprocess.PIPE).stdout.split()
    subprocess.run(["cc", "-shared", "-fPIC", "-O2", *cflags, "-o", str(PLUGIN),
                    str(Path(__file__).with_name("insnblocks.c"))], check=True)


def main():
    build_plugin()
    binary, n = Path(sys.argv[1]), int(sys.argv[2])
    rest = sys.argv[sys.argv.index("--") + 1:]
    table = symbols(binary)
    one, entry = counts(binary, [a.replace("{n}", str(n)) for a in rest])
    two, _ = counts(binary, [a.replace("{n}", str(2 * n)) for a in rest])
    # The load base: where the text landed, less the address of the
    # executable segment.
    header = subprocess.run(["readelf", "-lW", str(binary)], text=True, stdout=subprocess.PIPE).stdout
    load = next(l for l in header.splitlines() if l.split()[:1] == ["LOAD"] and " R E " in l)
    base = entry - int(load.split()[2], 16)
    f1, f2 = by_function(one, table, base), by_function(two, table, base)
    rows = sorted(((f2.get(k, 0) - f1.get(k, 0)) / n, k) for k in set(f1) | set(f2))
    for cost, name in reversed(rows):
        if abs(cost) >= 0.5:
            print(f"{cost:10.1f}  {name[:110]}")


if __name__ == "__main__":
    main()
