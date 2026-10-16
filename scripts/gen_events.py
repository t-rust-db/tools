#!/usr/bin/env python3
"""Seeded event workload for column-ingest (tools#36, db-core#592's shape).

Writes a time-ordered JSON Lines stream to stdout:

- ``--kind events`` (default): one line per stored event,
  ``{"ts", "session_id", "request_id", "event_id", "value", "score"}``.
  Sessions start at ``--sps`` per second (simulated time). Each session
  makes ``--requests`` requests on average. Each request stores ``--e``
  events (1, 3 or 10), named from a catalogue of ``--catalogue`` names
  with a Zipf frequency. ``value`` (a hex payload) and ``score`` are
  optional, so some lines lack them.
- ``--kind transactions``: one line per transaction, made by about one
  session in four:
  ``{"ts", "txn_id", "session_id", "amount", "currency", "from_account",
  "to_account", "channel"}``, with ``amount`` an exact decimal string.

A session id packs ``ts:32 | pid:16 | machine:4 | seq:12``, with the
session's start time (epoch seconds) in the high bits.

The same arguments and seed give the same bytes. Simulated time starts
at ``--start`` (2026-10-11T00:00:00Z by default). Without ``--rate``,
output is written as fast as it can be; ``--rate N`` paces it to N lines
per second of wall time, for live demos with ``column-ingest --follow``.

Usage:
    gen_events.py --sessions 1000 --e 3 --seed 1 | column-ingest events/
    gen_events.py --kind transactions --sessions 1000 --seed 1 | column-ingest txns/
    gen_events.py --duration 600 --sps 5 --rate 200 >> app.log
    gen_events.py --sessions 100 --summary > /dev/null
"""

import argparse
import heapq
import json
import random
import sys
import time
from datetime import datetime, timezone

DEFAULT_START = int(datetime(2026, 10, 11, tzinfo=timezone.utc).timestamp())
CHANNELS = ["web", "app", "api", "branch"]
CURRENCIES = ["EUR", "EUR", "EUR", "USD", "GBP"]


def session_id(start_s, pid, machine, seq):
    """``ts:32 | pid:16 | machine:4 | seq:12``."""
    return (
        (start_s & 0xFFFF_FFFF) << 32
        | (pid & 0xFFFF) << 16
        | (machine & 0xF) << 12
        | (seq & 0xFFF)
    )


def iso(ts):
    """Epoch seconds (float) as ISO 8601 UTC with milliseconds."""
    dt = datetime.fromtimestamp(ts, tz=timezone.utc)
    return dt.strftime("%Y-%m-%dT%H:%M:%S.") + f"{dt.microsecond // 1000:03d}Z"


class Catalogue:
    """``size`` event names drawn with a Zipf(s) frequency."""

    def __init__(self, size, s=1.1):
        self.names = [f"ev_{i:04d}" for i in range(size)]
        weights = [1.0 / (rank**s) for rank in range(1, size + 1)]
        total = sum(weights)
        self.cumulative = []
        acc = 0.0
        for w in weights:
            acc += w / total
            self.cumulative.append(acc)

    def draw(self, rng):
        x = rng.random()
        lo, hi = 0, len(self.cumulative) - 1
        while lo < hi:
            mid = (lo + hi) // 2
            if self.cumulative[mid] < x:
                lo = mid + 1
            else:
                hi = mid
        return self.names[lo]


def session_records(args, rng, catalogue, index):
    """Session `index`'s records, in time order, as (ts, line) pairs."""
    start = args.start + index / args.sps
    sid = session_id(int(start), rng.randrange(1 << 16), rng.randrange(16), index & 0xFFF)
    requests = max(1, round(rng.expovariate(1.0 / args.requests)))
    ts = start
    if args.kind == "transactions":
        if rng.random() >= 0.25:
            return
        ts += rng.uniform(1.0, 60.0)
        cents = rng.randrange(1, 500_000)
        yield ts, {
            "ts": iso(ts),
            "txn_id": f"t{index:08d}",
            "session_id": sid,
            "amount": f"{cents // 100}.{cents % 100:02d}",
            "currency": rng.choice(CURRENCIES),
            "from_account": f"NL{rng.randrange(100):02d}BANK{rng.randrange(10_000):04d}",
            "to_account": f"NL{rng.randrange(100):02d}BANK{rng.randrange(10_000):04d}",
            "channel": rng.choice(CHANNELS),
        }
        return
    for request_id in range(requests):
        ts += rng.uniform(0.2, 5.0)
        for k in range(args.e):
            at = ts + k * 0.001
            record = {
                "ts": iso(at),
                "session_id": sid,
                "request_id": request_id,
                "event_id": catalogue.draw(rng),
            }
            if rng.random() < 0.4:
                record["value"] = rng.randbytes(rng.randrange(4, 24)).hex()
            if rng.random() < 0.7:
                record["score"] = round(rng.uniform(0.0, 100.0), 3)
            yield at, record


def records(args):
    """Every record, in time order: sessions merged as they become active."""
    rng = random.Random(args.seed)
    catalogue = Catalogue(args.catalogue)
    total = args.sessions if args.sessions is not None else int(args.duration * args.sps)
    heap = []  # (next ts, session index, generator)
    started = 0

    def start_next():
        nonlocal started
        # One generator per session, each with its own seeded stream, so a
        # session's records do not depend on how sessions interleave.
        gen = session_records(args, random.Random(rng.getrandbits(64)), catalogue, started)
        first = next(gen, None)
        if first is not None:
            heapq.heappush(heap, (first[0], started, first[1], gen))
        started += 1

    while started < total or heap:
        next_start = args.start + started / args.sps
        if started < total and (not heap or next_start <= heap[0][0]):
            start_next()
            continue
        ts, index, record, gen = heapq.heappop(heap)
        yield record
        following = next(gen, None)
        if following is not None:
            heapq.heappush(heap, (following[0], index, following[1], gen))


def parse_args(argv):
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    size = p.add_mutually_exclusive_group()
    size.add_argument("--sessions", type=int, help="number of sessions (default 100)")
    size.add_argument("--duration", type=float, help="seconds of simulated time")
    p.add_argument("--kind", choices=["events", "transactions"], default="events")
    p.add_argument("--sps", type=float, default=2.0, help="sessions started per second")
    p.add_argument("--requests", type=float, default=8.0, help="mean requests per session")
    p.add_argument("--e", type=int, choices=[1, 3, 10], default=3, help="events stored per request")
    p.add_argument("--catalogue", type=int, default=300, help="distinct event names")
    p.add_argument("--seed", type=int, default=1)
    p.add_argument("--start", type=int, default=DEFAULT_START, help="simulated start, epoch seconds")
    p.add_argument("--rate", type=float, help="pace output to this many lines per second")
    p.add_argument("--summary", action="store_true", help="print totals to stderr at the end")
    args = p.parse_args(argv)
    if args.sessions is None and args.duration is None:
        args.sessions = 100
    if args.sps <= 0 or args.requests <= 0 or args.catalogue <= 0:
        p.error("--sps, --requests and --catalogue must be positive")
    if args.rate is not None and args.rate <= 0:
        p.error("--rate must be positive")
    return args


def main(argv=None, out=None):
    args = parse_args(argv)
    out = out or sys.stdout
    lines = written = 0
    sessions = set()
    requests = set()
    began = time.monotonic()
    try:
        for record in records(args):
            line = json.dumps(record, separators=(",", ":")) + "\n"
            out.write(line)
            lines += 1
            written += len(line.encode())
            sessions.add(record["session_id"])
            if "request_id" in record:
                requests.add((record["session_id"], record["request_id"]))
            if args.rate:
                # Pace against the start, not the previous line, so sleep
                # overshoot does not accumulate.
                ahead = lines / args.rate - (time.monotonic() - began)
                if ahead > 0:
                    out.flush()
                    time.sleep(ahead)
        out.flush()
    except BrokenPipeError:
        # A consumer that stopped reading (head, a killed column-ingest).
        sys.stderr.close()
        return 0
    if args.summary:
        per_request = lines / len(requests) if requests else 0.0
        print(
            f"gen_events: kind={args.kind} sessions={len(sessions)} records={lines} "
            f"events_per_request={per_request:.2f} bytes={written}",
            file=sys.stderr,
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
