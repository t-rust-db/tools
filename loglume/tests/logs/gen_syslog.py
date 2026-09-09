#!/usr/bin/env python3
"""Generate synthetic syslog RFC 3164 logs for testing.

Usage:
    python gen_syslog.py [--lines N] [--seed S] [--output FILE]
    python gen_syslog.py --help

Examples:
    python gen_syslog.py                        # 1000 lines to stdout
    python gen_syslog.py -n 10000 -o app.log    # 10k lines to file
    python gen_syslog.py --seed 42              # reproducible output
"""

import argparse
import random
import sys
from datetime import datetime, timedelta

# RFC 3164 format: <PRI>TIMESTAMP HOSTNAME TAG[PID]: MESSAGE
# Example: <134>Sep  9 14:23:01 webserver nginx[1234]: GET /api/health 200

FACILITIES = {
    "kern": 0, "user": 1, "mail": 2, "daemon": 3, "auth": 4,
    "syslog": 5, "lpr": 6, "news": 7, "local0": 16, "local7": 23,
}

SEVERITIES = {
    "emerg": 0, "alert": 1, "crit": 2, "err": 3,
    "warning": 4, "notice": 5, "info": 6, "debug": 7,
}

SERVICES = [
    ("nginx", "daemon", ["GET /api/users 200", "POST /api/auth 401", "GET /health 200",
                         "GET /api/orders 500", "connection timeout upstream"]),
    ("sshd", "auth", ["Accepted publickey for admin", "Failed password for root",
                      "Connection closed by authenticating user", "session opened for user deploy"]),
    ("cron", "daemon", ["(root) CMD (/usr/local/bin/backup.sh)", "(www-data) CMD (php artisan schedule:run)",
                        "CRON[12345]: (CRON) info (No MTA installed)"]),
    ("systemd", "daemon", ["Started Daily apt upgrade", "Stopping User Manager for UID 1000",
                           "Started Session 42 of user admin", "Reached target Multi-User System"]),
    ("kernel", "kern", ["[UFW BLOCK] IN=eth0 OUT=", "TCP: out of memory",
                        "EXT4-fs (sda1): mounted filesystem", "oom-killer: Kill process"]),
    ("postgres", "local0", ["LOG: connection authorized", "ERROR: relation does not exist",
                            "LOG: checkpoint starting", "FATAL: password authentication failed"]),
]

HOSTNAMES = ["web01", "web02", "db01", "cache01", "worker01", "gateway"]


def pri(facility: str, severity: str) -> int:
    """Calculate PRI value: facility * 8 + severity."""
    return FACILITIES[facility] * 8 + SEVERITIES[severity]


def severity_for_message(msg: str) -> str:
    """Infer severity from message content."""
    msg_lower = msg.lower()
    if any(x in msg_lower for x in ["fatal", "oom", "kill", "out of memory"]):
        return "crit"
    if any(x in msg_lower for x in ["error", "fail", "500", "401"]):
        return "err"
    if any(x in msg_lower for x in ["warning", "timeout", "block"]):
        return "warning"
    if any(x in msg_lower for x in ["notice", "started", "stopped"]):
        return "notice"
    if "debug" in msg_lower:
        return "debug"
    return "info"


def generate_line(ts: datetime, rng: random.Random) -> str:
    """Generate one RFC 3164 syslog line."""
    service, facility, messages = rng.choice(SERVICES)
    hostname = rng.choice(HOSTNAMES)
    msg = rng.choice(messages)
    severity = severity_for_message(msg)

    priority = pri(facility, severity)
    pid = rng.randint(1000, 65000)

    # RFC 3164 timestamp: "Mmm dd HH:MM:SS" (day is space-padded)
    timestamp = ts.strftime("%b %e %H:%M:%S").replace("  ", " ")

    return f"<{priority}>{timestamp} {hostname} {service}[{pid}]: {msg}"


def generate_logs(n: int, seed: int | None = None) -> list[str]:
    """Generate n syslog lines with realistic timing."""
    rng = random.Random(seed)

    # Start from a base time, advance randomly
    base = datetime(2024, 9, 9, 8, 0, 0)
    lines = []

    for _ in range(n):
        lines.append(generate_line(base, rng))
        # Advance 0-5 seconds (bursty logs)
        base += timedelta(seconds=rng.uniform(0, 5))

    return lines


def main():
    parser = argparse.ArgumentParser(description="Generate synthetic syslog logs")
    parser.add_argument("-n", "--lines", type=int, default=1000, help="Number of lines (default: 1000)")
    parser.add_argument("-s", "--seed", type=int, help="Random seed for reproducibility")
    parser.add_argument("-o", "--output", type=str, help="Output file (default: stdout)")
    args = parser.parse_args()

    lines = generate_logs(args.lines, args.seed)

    output = "\n".join(lines) + "\n"
    if args.output:
        with open(args.output, "w") as f:
            f.write(output)
        print(f"Wrote {args.lines} lines to {args.output}", file=sys.stderr)
    else:
        print(output, end="")


if __name__ == "__main__":
    main()
