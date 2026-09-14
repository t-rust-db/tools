#!/usr/bin/env python3
"""Generate synthetic Pino-shaped JSON Lines logs for testing.

Each line: {"time": <ISO8601>, "level": <30|40|50|60>, "pid": <int>,
"hostname": <str>, "req": {...}, "res": {...}, "responseTime": <float>,
"msg": <str>}

Usage:
    python gen_jsonl.py [-n N] [-s SEED] [-o FILE]
    python gen_jsonl.py --docker     # wrap each line as Docker json-file,
                                      # with a *syslog* payload (not JSONL)
                                      # to exercise container unwrap + format
                                      # re-detection
    python gen_jsonl.py --sparse     # add random extra keys to some lines
    python gen_jsonl.py --rate 10    # live feed: 10 lines/s to stdout, never exits
"""

import argparse
import json
import random
import sys
import time
from datetime import datetime, timedelta, timezone

# Pino numeric levels: 30 info, 40 warn, 50 error, 60 fatal.
LEVELS = [30, 30, 30, 30, 40, 40, 50, 60]

HOSTNAMES = ["web01", "web02", "worker01", "gateway"]
METHODS = ["GET", "POST", "PUT", "DELETE"]
URLS = ["/api/users", "/api/orders", "/api/auth/login", "/health"]
MESSAGES = ["request completed", "request errored", "slow query detected", "cache miss", "connection reset"]

EXTRA_KEYS = ["trace_id", "span_id", "user_id", "tenant", "region", "build_sha"]

# For --docker: a small syslog-shaped payload generator (independent of the
# Pino JSON shape above), so a Docker-wrapped line exercises container
# unwrap + per-line format re-detection landing on syslog, not JSONL.
SYSLOG_SERVICES = [
    ("nginx", "GET /api/orders 200"),
    ("app", "worker started"),
    ("app", "job failed: connection refused"),
]


def random_syslog_payload(rng: random.Random, ts: datetime) -> str:
    service, msg = rng.choice(SYSLOG_SERVICES)
    pid = rng.randint(1000, 65000)
    timestamp = ts.strftime("%b %e %H:%M:%S").replace("  ", " ")
    return f"<134>{timestamp} {rng.choice(HOSTNAMES)} {service}[{pid}]: {msg}"


def generate_record(ts: datetime, rng: random.Random, sparse: bool) -> dict:
    level = rng.choice(LEVELS)
    record = {
        "time": ts.strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z",
        "level": level,
        "pid": rng.randint(1000, 65000),
        "hostname": rng.choice(HOSTNAMES),
        "req": {
            "method": rng.choice(METHODS),
            "url": rng.choice(URLS),
        },
        "res": {
            "statusCode": 200 if level == 30 else rng.choice([400, 500]),
        },
        "responseTime": round(rng.uniform(0.5, 500.0), 2),
        "msg": rng.choice(MESSAGES),
    }
    if sparse and rng.random() < 0.3:
        extra_key = rng.choice(EXTRA_KEYS)
        record[extra_key] = str(rng.getrandbits(32))
    return record


def generate_line(ts: datetime, rng: random.Random, sparse: bool, docker: bool) -> str:
    if docker:
        payload = random_syslog_payload(rng, ts)
        envelope = {
            "log": payload + "\n",
            "stream": "stdout",
            "time": ts.strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z",
        }
        return json.dumps(envelope)
    record = generate_record(ts, rng, sparse)
    return json.dumps(record)


def generate_lines(n: int, seed: int | None, sparse: bool, docker: bool) -> list[str]:
    rng = random.Random(seed)
    base = datetime(2026, 9, 10, 8, 0, 0, tzinfo=timezone.utc)
    lines = []
    for _ in range(n):
        lines.append(generate_line(base, rng, sparse, docker))
        base += timedelta(seconds=rng.uniform(0, 2))
    return lines


def run_rate_feed(rate: float, seed: int | None, sparse: bool, docker: bool) -> None:
    """Emit ~`rate` lines/second to stdout forever (for tailing tests)."""
    rng = random.Random(seed)
    interval = 1.0 / rate if rate > 0 else 0.0
    while True:
        now = datetime.now(timezone.utc)
        print(generate_line(now, rng, sparse, docker), flush=True)
        if interval > 0:
            time.sleep(interval)


def main() -> None:
    parser = argparse.ArgumentParser(description="Generate synthetic Pino-shaped JSONL logs")
    parser.add_argument("-n", "--lines", type=int, default=1000, help="Number of lines (default: 1000)")
    parser.add_argument("-s", "--seed", type=int, help="Random seed for reproducibility")
    parser.add_argument("-o", "--output", type=str, help="Output file (default: stdout)")
    parser.add_argument("--docker", action="store_true", help="Wrap each line as Docker json-file with a syslog payload")
    parser.add_argument("--sparse", action="store_true", help="Add random extra keys to some lines")
    parser.add_argument("--rate", type=float, help="Live-feed mode: lines/s to stdout, never exits")
    args = parser.parse_args()

    if args.rate is not None:
        run_rate_feed(args.rate, args.seed, args.sparse, args.docker)
        return

    lines = generate_lines(args.lines, args.seed, args.sparse, args.docker)
    output = "\n".join(lines) + "\n"
    if args.output:
        with open(args.output, "w") as f:
            f.write(output)
        print(f"Wrote {args.lines} lines to {args.output}", file=sys.stderr)
    else:
        print(output, end="")


if __name__ == "__main__":
    main()
