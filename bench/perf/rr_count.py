#!/usr/bin/env python3
"""Round-robin distribution counter (Phase 2.3).

Fires N requests at the LB proxy over keep-alive connections and tallies the per-instance
`X-Instance` response header, so the round-robin split can be checked to single-request precision.
Writes rr.csv (instance,count). All three upstreams must be healthy for the duration.

    python3 rr_count.py --target http://127.0.0.1:8080/ --total 60000 --workers 48 --out rr.csv
"""
from __future__ import annotations

import argparse
import csv
import http.client
import threading
from collections import Counter
from urllib.parse import urlparse


def worker(host: str, port: int, path: str, n: int, tally: Counter, lock: threading.Lock) -> None:
    local = Counter()
    conn = http.client.HTTPConnection(host, port)
    for _ in range(n):
        try:
            conn.request("GET", path)
            resp = conn.getresponse()
            inst = resp.getheader("X-Instance") or ("FAIL" if resp.status >= 500 else "other")
            resp.read()
            local[inst] += 1
        except Exception:
            local["error"] += 1
            try:
                conn.close()
            except Exception:
                pass
            conn = http.client.HTTPConnection(host, port)
    conn.close()
    with lock:
        tally.update(local)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--target", default="http://127.0.0.1:8080/")
    ap.add_argument("--total", type=int, default=60000)
    ap.add_argument("--workers", type=int, default=48)
    ap.add_argument("--out", default="rr.csv")
    args = ap.parse_args()

    u = urlparse(args.target)
    host, port, path = u.hostname, u.port or 80, u.path or "/"

    tally: Counter = Counter()
    lock = threading.Lock()
    per = args.total // args.workers
    threads = [
        threading.Thread(target=worker, args=(host, port, path, per, tally, lock))
        for _ in range(args.workers)
    ]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    # rr.csv expects instances a/b/c; emit them in order, then any extras (FAIL/error) for honesty.
    ordered = [k for k in ("a", "b", "c") if k in tally] + \
              [k for k in tally if k not in ("a", "b", "c")]
    with open(args.out, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["instance", "count"])
        for k in ordered:
            w.writerow([k, tally[k]])
    total = sum(tally.values())
    print(f"rr_count: {total} responses -> " +
          ", ".join(f"{k}={tally[k]} ({100*tally[k]/total:.2f}%)" for k in ordered))


if __name__ == "__main__":
    main()
