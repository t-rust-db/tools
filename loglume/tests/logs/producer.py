#!/usr/bin/env python3
"""Append syslog lines to a file continuously, to exercise live tailing."""

import argparse
import random
import sys
import time
from datetime import datetime

from gen_syslog import generate_line


def main():
    parser = argparse.ArgumentParser(description="Continuously append synthetic syslog lines")
    parser.add_argument("-o", "--output", default="tests/logs/live.log", help="File to append to")
    parser.add_argument("-r", "--rate", type=float, default=2.0, help="Lines per second (default: 2)")
    parser.add_argument("-s", "--seed", type=int, help="Random seed for reproducibility")
    parser.add_argument("-t", "--truncate", action="store_true", help="Truncate the file before producing")
    parser.add_argument("-q", "--quiet", action="store_true", help="Do not echo produced lines to stdout")
    args = parser.parse_args()

    rng = random.Random(args.seed)
    delay = 1.0 / args.rate if args.rate > 0 else 0.0

    with open(args.output, "w" if args.truncate else "a") as f:
        print(f"Producing to {args.output} at {args.rate} lines/s (Ctrl-C to stop)", file=sys.stderr)
        try:
            while True:
                line = generate_line(datetime.now(), rng)
                f.write(line + "\n")
                f.flush()
                if not args.quiet:
                    print(line, flush=True)
                time.sleep(delay)
        except KeyboardInterrupt:
            print("\nStopped.", file=sys.stderr)


if __name__ == "__main__":
    main()
