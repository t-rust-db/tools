"""Tests for gen_events.py (tools#36): python3 -m unittest scripts/test_gen_events.py"""

import io
import json
import re
import sys
import time
import unittest
from collections import Counter
from contextlib import redirect_stderr
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import gen_events  # noqa: E402


def run(*argv):
    out, err = io.StringIO(), io.StringIO()
    with redirect_stderr(err):
        gen_events.main(list(argv), out=out)
    return out.getvalue(), err.getvalue()


def records(*argv):
    return [json.loads(line) for line in run(*argv)[0].splitlines()]


class GenEvents(unittest.TestCase):
    def test_the_same_seed_gives_the_same_bytes(self):
        a, _ = run("--sessions", "50", "--seed", "7")
        b, _ = run("--sessions", "50", "--seed", "7")
        c, _ = run("--sessions", "50", "--seed", "8")
        self.assertEqual(a, b)
        self.assertNotEqual(a, c)

    def test_records_are_in_time_order(self):
        rows = records("--sessions", "200", "--sps", "20", "--seed", "3")
        stamps = [r["ts"] for r in rows]
        self.assertEqual(stamps, sorted(stamps))
        self.assertGreater(len({r["session_id"] for r in rows}), 150, "sessions interleave")

    def test_each_request_stores_e_events(self):
        for e in (1, 3, 10):
            rows = records("--sessions", "30", "--e", str(e))
            per_request = Counter((r["session_id"], r["request_id"]) for r in rows)
            self.assertEqual(set(per_request.values()), {e}, f"--e {e}")

    def test_a_session_id_carries_its_start_time_in_the_high_bits(self):
        rows = records("--sessions", "20", "--sps", "1", "--start", "1800000000")
        for r in rows:
            start = r["session_id"] >> 32
            self.assertGreaterEqual(start, 1_800_000_000)
            self.assertLess(start, 1_800_000_020)
            self.assertLess(r["session_id"], 1 << 63, "fits a signed 64-bit integer")

    def test_event_names_are_skewed(self):
        rows = records("--sessions", "300", "--catalogue", "100")
        counts = Counter(r["event_id"] for r in rows).most_common()
        self.assertGreater(counts[0][1], 5 * counts[-1][1], "Zipf: the top name dominates")
        self.assertTrue(all(re.fullmatch(r"ev_\d{4}", name) for name, _ in counts))

    def test_value_and_score_are_optional(self):
        rows = records("--sessions", "50")
        self.assertTrue(any("value" in r for r in rows))
        self.assertTrue(any("value" not in r for r in rows))
        self.assertTrue(any("score" not in r for r in rows))

    def test_transactions_carry_exact_decimal_amounts(self):
        rows = records("--kind", "transactions", "--sessions", "400", "--seed", "2")
        self.assertTrue(60 < len(rows) < 140, f"about one session in four: {len(rows)}")
        for r in rows:
            self.assertRegex(r["amount"], r"^\d+\.\d{2}$")
            self.assertIn(r["channel"], gen_events.CHANNELS)
        self.assertEqual([r["ts"] for r in rows], sorted(r["ts"] for r in rows))

    def test_summary_counts_what_was_written(self):
        out, err = run("--sessions", "40", "--e", "3", "--summary")
        lines = out.splitlines()
        m = re.search(r"sessions=(\d+) records=(\d+) events_per_request=([\d.]+) bytes=(\d+)", err)
        self.assertIsNotNone(m, err)
        self.assertEqual(int(m.group(1)), 40)
        self.assertEqual(int(m.group(2)), len(lines))
        self.assertEqual(m.group(3), "3.00")
        self.assertEqual(int(m.group(4)), len(out.encode()))

    def test_duration_sets_the_session_count(self):
        rows = records("--duration", "10", "--sps", "3")
        self.assertEqual(len({r["session_id"] for r in rows}), 30)

    def test_rate_paces_the_output(self):
        # 300 lines at 150/s: about two seconds.
        start = time.monotonic()
        out, _ = run("--sessions", "13", "--requests", "8", "--e", "3", "--seed", "4", "--rate", "150")
        elapsed = time.monotonic() - start
        lines = len(out.splitlines())
        self.assertGreater(lines, 150)
        expected = lines / 150
        self.assertLess(abs(elapsed - expected) / expected, 0.10, f"{lines} lines in {elapsed:.2f}s")


if __name__ == "__main__":
    unittest.main()
