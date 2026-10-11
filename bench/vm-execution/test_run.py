"""Result validation must reject a fast run that did the wrong work."""
import unittest

from workloads import dialect, parse_output, script


class OutputValidation(unittest.TestCase):
    GOOD = """[log prefix] VM_SECONDS: 0.01
[log prefix] VM_VALID: true
[log prefix] VM_DONE: 100
VM_SECONDS: 2e-2
VM_VALID: true
VM_DONE: 100
main: 0
"""

    def test_both_log_formats(self):
        self.assertEqual(parse_output(self.GOOD, 100, 2), [0.01, 0.02])

    def test_wrong_value(self):
        with self.assertRaises(RuntimeError):
            parse_output(self.GOOD.replace("true", "false", 1), 100, 2)

    def test_wrong_iteration_count(self):
        with self.assertRaises(RuntimeError):
            parse_output(self.GOOD.replace("VM_DONE: 100", "VM_DONE: 99", 1), 100, 2)

    def test_missing_sample(self):
        with self.assertRaises(RuntimeError):
            parse_output(self.GOOD, 100, 3)

    def test_extra_sample(self):
        with self.assertRaises(RuntimeError):
            parse_output(self.GOOD + self.GOOD, 100, 2)

    def test_invalid_time(self):
        for bad in ("0", "-0.01", "1e999", "nan", "inf", "0.01junk"):
            with self.subTest(bad=bad), self.assertRaises(RuntimeError):
                parse_output(self.GOOD.replace("0.01", bad), 100, 2)


if __name__ == "__main__":
    unittest.main()


class Dialects(unittest.TestCase):
    def test_2_uses_its_own_syntax(self):
        for case in ("push-shared-seq", "add-float4", "do-int"):
            text = dialect(script(case, 2, 10, 2), "2-stackless")
            for old in ("Set(", "Times:", "Clear:"):
                self.assertNotIn(old, text, case)
            self.assertIn(" | Var(", text)
            self.assertEqual(script(case, 2, 10, 2), dialect(script(case, 2, 10, 2), "1x"))


class Untimed(unittest.TestCase):
    def test_reads_no_clock(self):
        for case in ("add-int", "push-shared-seq", "do-int"):
            text = script(case, 8, 10, 1, timed=False)
            self.assertNotIn("Time.Now", text, case)
            self.assertNotIn("VM_SECONDS", text, case)

    def test_output_is_checked_without_durations(self):
        good = "VM_VALID: true\nVM_DONE: 100\n"
        self.assertEqual(parse_output(good, 100, 1, timed=False), [])
        with self.assertRaises(RuntimeError):
            parse_output(good.replace("100", "99"), 100, 1, timed=False)
        with self.assertRaises(RuntimeError):
            parse_output("VM_SECONDS: 0.01\n" + good, 100, 1, timed=False)
