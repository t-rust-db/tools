#!/usr/bin/env python3
"""Generate synthetic nginx combined access logs for testing.

Format: host ident authuser [date] "request" status bytes "referer" "user-agent"

Usage:
    python gen_access.py [-n N] [-s SEED] [-o FILE]
    python gen_access.py --rate 10   # live feed: 10 lines/s to stdout, never exits

Status mix is realistic (85% 2xx, 10% 4xx, 5% 5xx), and IPs/paths/user-agents
are high-cardinality so FieldStore's Dict-column degrade-to-Str path is
exercised, not just a handful of repeated values.
"""

import argparse
import random
import sys
import time
import uuid
from datetime import datetime, timedelta, timezone

METHODS = ["GET", "GET", "GET", "POST", "PUT", "DELETE"]
PATH_PREFIXES = ["/api/users", "/api/orders", "/api/auth", "/api/products", "/health", "/static/js"]
BROWSERS = ["Chrome", "Firefox", "Safari", "Edge"]
OS_LIST = ["Windows NT 10.0", "Macintosh; Intel Mac OS X 10_15_7", "X11; Linux x86_64", "iPhone; CPU iPhone OS 17_0"]
REFERERS = ["-", "https://example.com/", "https://google.com/", "https://app.example.com/dashboard"]

STATUS_2XX = [200, 200, 200, 201, 204]
STATUS_4XX = [400, 401, 403, 404, 429]
STATUS_5XX = [500, 502, 503]


def random_ip(rng: random.Random) -> str:
    return f"{rng.randint(1, 223)}.{rng.randint(0, 255)}.{rng.randint(0, 255)}.{rng.randint(1, 254)}"


def random_path(rng: random.Random) -> str:
    prefix = rng.choice(PATH_PREFIXES)
    return f"{prefix}/{uuid.UUID(int=rng.getrandbits(128))}"


def random_user_agent(rng: random.Random) -> str:
    browser = rng.choice(BROWSERS)
    os_str = rng.choice(OS_LIST)
    major = rng.randint(90, 130)
    return f"Mozilla/5.0 ({os_str}) AppleWebKit/537.36 {browser}/{major}.0"


def random_status(rng: random.Random) -> int:
    roll = rng.random()
    if roll < 0.85:
        return rng.choice(STATUS_2XX)
    if roll < 0.95:
        return rng.choice(STATUS_4XX)
    return rng.choice(STATUS_5XX)


def format_date(ts: datetime) -> str:
    # nginx combined date: [10/Sep/2026:08:00:05 +0200]
    return ts.strftime("%d/%b/%Y:%H:%M:%S %z")


def generate_line(ts: datetime, rng: random.Random) -> str:
    ip = random_ip(rng)
    method = rng.choice(METHODS)
    path = random_path(rng)
    status = random_status(rng)
    bytes_sent = rng.randint(200, 65536)
    referer = rng.choice(REFERERS)
    ua = random_user_agent(rng)
    date = format_date(ts)
    return f'{ip} - - [{date}] "{method} {path} HTTP/1.1" {status} {bytes_sent} "{referer}" "{ua}"'


def generate_lines(n: int, seed: int | None) -> list[str]:
    rng = random.Random(seed)
    base = datetime(2026, 9, 10, 8, 0, 0, tzinfo=timezone(timedelta(hours=2)))
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
    parser = argparse.ArgumentParser(description="Generate synthetic nginx access logs")
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
