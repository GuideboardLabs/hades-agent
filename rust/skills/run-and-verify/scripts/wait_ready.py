#!/usr/bin/env python3
"""Wait for a service to become ready, then report the evidence.

Readiness is a port that accepts, an HTTP response under the failure range, or a log line. A fixed sleep
is not a readiness check; this polls with a deadline and exits non-zero when the deadline passes.
"""

from __future__ import annotations

import argparse
import json
import os
import socket
import time
import urllib.error
import urllib.request


def process_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


def port_ready(host: str, port: int, timeout: float) -> tuple[bool, str]:
    try:
        with socket.create_connection((host, port), timeout=timeout):
            return True, f"{host}:{port} accepted"
    except OSError as error:
        return False, str(error)


def url_ready(url: str, timeout: float, expect_status: int | None, max_status: int) -> tuple[bool, str, int | None]:
    request = urllib.request.Request(url, headers={"User-Agent": "hades-wait-ready/1.0"})
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            status = response.status
            good = status == expect_status if expect_status is not None else status < max_status
            return good, f"HTTP {status}", status
    except urllib.error.HTTPError as error:
        good = error.code == expect_status if expect_status is not None else error.code < max_status
        return good, f"HTTP {error.code}", error.code
    except Exception as error:
        return False, str(error), None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    target = parser.add_mutually_exclusive_group(required=True)
    target.add_argument("--port", type=int)
    target.add_argument("--url")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--timeout", type=float, default=30.0)
    parser.add_argument("--interval", type=float, default=0.25)
    parser.add_argument("--expect-status", type=int)
    parser.add_argument("--max-status", type=int, default=500)
    parser.add_argument("--pid", type=int, help="also fail if this process exits")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()

    started = time.monotonic()
    attempts = 0
    detail = "not started"
    status = None
    while time.monotonic() - started < args.timeout:
        attempts += 1
        if args.pid is not None and not process_alive(args.pid):
            result = {"ready": False, "detail": f"process {args.pid} exited", "attempts": attempts}
            print(json.dumps(result, indent=2) if args.json else result["detail"])
            return 2
        if args.port is not None:
            ready, detail = port_ready(args.host, args.port, min(1.0, args.interval + 0.5))
        else:
            ready, detail, status = url_ready(args.url, min(5.0, args.timeout), args.expect_status, args.max_status)
        if ready:
            result = {
                "ready": True,
                "detail": detail,
                "status": status,
                "attempts": attempts,
                "seconds": round(time.monotonic() - started, 3),
            }
            print(json.dumps(result, indent=2) if args.json else f"ready: {detail} after {result['seconds']:g}s")
            return 0
        time.sleep(args.interval)

    result = {
        "ready": False,
        "detail": detail,
        "status": status,
        "attempts": attempts,
        "seconds": round(time.monotonic() - started, 3),
    }
    print(json.dumps(result, indent=2) if args.json else f"not ready after {args.timeout:g}s: {detail}")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
