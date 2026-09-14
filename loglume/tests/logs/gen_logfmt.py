#!/usr/bin/env python3
"""Generate synthetic logfmt (key=value) logs for testing.

Format: ts=<ISO8601> level=<level> msg="<quoted, possibly escaped>"
service=<service> duration=<unit-suffixed number> status=<int>

Usage:
    python gen_logfmt.py [-n N] [-s SEED] [-o FILE]
    python gen_logfmt.py --rate 10   # live feed: 10 lines/s to stdout, never exits
"""

import argparse
import random
import sys
import time
from datetime import datetime, timedelta, timezone

LEVELS = ["info", "info", "info", "warn", "warn", "error", "debug"]
SERVICES = ["auth", "billing", "checkout", "search", "notifications"]
MESSAGES = [
    "request completed",
    'user said "hello world" during checkout',
    "slow query detected\ttiming flagged",
    "cache miss for key",
    "retrying after failure",
]
STATUSES = [200, 200, 200, 201, 400, 404, 500]
DURATION_UNITS = ["ms", "s"]


def escape_logfmt_value(value: str) -> str:
    """Escape a value for a double-quoted logfmt field: backslash and
    double-quote must be escaped; embedded tabs/newlines are escaped too so
    the value stays on one physical line."""
    return (
        value.replace("\\", "\\\\")
        .replace('"', '\\"')
        .replace("\t", "\\t")
        .replace("\n", "\\n")
    )


def random_duration(rng: random.Random) -> str:
    unit = rng.choice(DURATION_UNITS)
    if unit == "ms":
        return f"{rng.randint(1, 5000)}ms"
    return f"{rng.uniform(0.001, 5.0):.3f}s"


def generate_line(ts: datetime, rng: random.Random) -> str:
    level = rng.choice(LEVELS)
    msg = escape_logfmt_value(rng.choice(MESSAGES))
    service = rng.choice(SERVICES)
    duration = random_duration(rng)
    status = rng.choice(STATUSES)
    timestamp = ts.strftime("%Y-%m-%dT%H:%M:%SZ")
    return f'ts={timestamp} level={level} msg="{msg}" service={service} duration={duration} status={status}'


def generate_lines(n: int, seed: int | None) -> list[str]:
    rng = random.Random(seed)
    base = datetime(2026, 9, 10, 8, 0, 0, tzinfo=timezone.utc)
    lines = []
    for _ in range(n):
        lines.append(generate_line(base, rng))
        base += timedelta(seconds=rng.uniform(0, 2))
    return lines


def run_rate_feed(rate: float, seed: int | None) -> None:
    """Emit ~`rate` lines/second to stdout forever (for tailing tests)."""
    rng = random.Random(seed)
    interval = 1.0 / rate if rate > 0 else 0.0
    while True:
        now = datetime.now(timezone.utc)
        print(generate_line(now, rng), flush=True)
        if interval > 0:
            time.sleep(interval)


def main() -> None:
    parser = argparse.ArgumentParser(description="Generate synthetic logfmt logs")
    parser.add_argument("-n", "--lines", type=int, default=1000, help="Number of lines (default: 1000)")
    parser.add_argument("-s", "--seed", type=int, help="Random seed for reproducibility")
    parser.add_argument("-o", "--output", type=str, help="Output file (default: stdout)")
    parser.add_argument("--rate", type=float, help="Live-feed mode: lines/s to stdout, never exits")
    args = parser.parse_args()

    if args.rate is not None:
        run_rate_feed(args.rate, args.seed)
        return

    lines = generate_lines(args.lines, args.seed)
    output = "\n".join(lines) + "\n"
    if args.output:
        with open(args.output, "w") as f:
            f.write(output)
        print(f"Wrote {args.lines} lines to {args.output}", file=sys.stderr)
    else:
        print(output, end="")


if __name__ == "__main__":
    main()
