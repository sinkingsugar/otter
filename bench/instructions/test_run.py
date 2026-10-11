"""The check must fail on any rise in a gated column, and only there."""
import contextlib
import io
from pathlib import Path
import tempfile
import unittest

from run import COLUMNS, GATED, check, read_baseline, write_baseline

ROW = {"ir": 1000.0, "allocs": 1.0, "bytes": 144.0, "d1_misses": 2.0, "ll_misses": 0.0, "mispredicts": 6.0}


def quiet_check(results, baseline, gated=GATED):
    with contextlib.redirect_stdout(io.StringIO()):
        return check(results, baseline, gated)


class Check(unittest.TestCase):
    def test_unchanged_passes(self):
        self.assertEqual(quiet_check({"w": dict(ROW)}, {"w": dict(ROW)}), [])

    def test_any_rise_in_a_gated_column_fails(self):
        for column, rise in (("ir", 0.01), ("allocs", 1), ("bytes", 8)):
            with self.subTest(column=column):
                new = dict(ROW, **{column: ROW[column] + rise})
                self.assertEqual(len(quiet_check({"w": new}, {"w": dict(ROW)})), 1)

    def test_improvements_and_secondary_columns_pass(self):
        new = dict(ROW, ir=900.0, allocs=0.0, d1_misses=50.0, mispredicts=60.0)
        self.assertEqual(quiet_check({"w": new}, {"w": dict(ROW)}), [])

    def test_from_zero(self):
        base = dict(ROW, allocs=0.0, bytes=0.0)
        self.assertEqual(len(quiet_check({"w": dict(ROW)}, {"w": base})), 2)

    def test_allocations_only_ignores_instructions(self):
        new = {"allocs": ROW["allocs"], "bytes": ROW["bytes"]}
        self.assertEqual(quiet_check({"w": new}, {"w": dict(ROW, ir=1.0)}, ("allocs", "bytes")), [])

    def test_workload_sets_must_match(self):
        self.assertEqual(len(quiet_check({"w": dict(ROW), "new": dict(ROW)}, {"w": dict(ROW)})), 1)
        self.assertEqual(len(quiet_check({"w": dict(ROW)}, {"w": dict(ROW), "gone": dict(ROW)})), 1)


class Baseline(unittest.TestCase):
    def test_round_trip(self):
        env = {"rustc": "rustc 1.98.1", "valgrind": "valgrind-3.22.0"}
        results = {"vm/add-int/8": dict(ROW), "reload": dict(ROW, ir=358589.1)}
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "baseline.txt"
            write_baseline(path, results, env)
            read_env, rows = read_baseline(path)
        self.assertEqual(read_env, env)
        self.assertEqual(rows, results)
        self.assertEqual(set(rows["reload"]), set(COLUMNS))


if __name__ == "__main__":
    unittest.main()
