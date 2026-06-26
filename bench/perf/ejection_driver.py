#!/usr/bin/env python3
"""Resilience / fault-injection driver (Phase 2.4).

Holds a fixed open-loop arrival rate against the LB proxy while a controller drives a fault
timeline on the upstream `/toggle` endpoints, and aggregates per-second per-instance served counts
plus the 503/error rate. Writes:

  * ejection_timeline.csv   t,a,b,c,failed       (requests/second in each 1 s bucket)
  * ejection_events.csv     t,label              (fault timeline markers)

Timeline (relative seconds): 15 eject b / 30 rejoin b / 45 eject all / 60 restore all / 75 end.
`/toggle` flips an instance's health; the active health check (500 ms probe, 2-sample threshold)
acts on it in ~1 s.

    python3 ejection_driver.py --target http://127.0.0.1:8080/ --rate 4000 \
        --toggle a=http://127.0.0.1:PA/toggle b=http://127.0.0.1:PB/toggle c=http://127.0.0.1:PC/toggle
"""
from __future__ import annotations

import argparse
import csv
import http.client
import queue
import threading
import time
import urllib.request
from collections import defaultdict
from urllib.parse import urlparse

STOP = object()


def worker(host, port, path, q: queue.Queue, start: float, buckets: list, lock: threading.Lock):
    local = defaultdict(lambda: defaultdict(int))  # sec -> instance -> count
    conn = http.client.HTTPConnection(host, port, timeout=5)
    while True:
        item = q.get()
        if item is STOP:
            break
        try:
            conn.request("GET", path)
            resp = conn.getresponse()
            status = resp.status
            inst = resp.getheader("X-Instance")
            resp.read()
            sec = int(time.monotonic() - start)
            if status >= 500 or inst is None:
                local[sec]["failed"] += 1
            else:
                local[sec][inst] += 1
        except Exception:
            sec = int(time.monotonic() - start)
            local[sec]["failed"] += 1
            try:
                conn.close()
            except Exception:
                pass
            conn = http.client.HTTPConnection(host, port, timeout=5)
        finally:
            q.task_done()
    conn.close()
    with lock:
        buckets.append(local)


def controller(toggles: dict, start: float, events: list):
    def at(delay, fn, label):
        while time.monotonic() - start < delay:
            time.sleep(0.02)
        fn()
        events.append((int(round(delay)), label))

    def toggle(url):
        try:
            urllib.request.urlopen(url, timeout=2).read()
        except Exception:
            pass

    at(15, lambda: toggle(toggles["b"]), "eject b")
    at(30, lambda: toggle(toggles["b"]), "rejoin b")
    at(45, lambda: [toggle(toggles[k]) for k in ("a", "b", "c")], "eject all")
    at(60, lambda: [toggle(toggles[k]) for k in ("a", "b", "c")], "restore all")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--target", default="http://127.0.0.1:8080/")
    ap.add_argument("--rate", type=int, default=4000)
    ap.add_argument("--duration", type=int, default=75)
    ap.add_argument("--workers", type=int, default=64)
    ap.add_argument("--toggle", nargs=3, required=True, help="a=URL b=URL c=URL")
    ap.add_argument("--out", default="ejection_timeline.csv")
    ap.add_argument("--events-out", default="ejection_events.csv")
    args = ap.parse_args()

    toggles = dict(kv.split("=", 1) for kv in args.toggle)
    u = urlparse(args.target)
    host, port, path = u.hostname, u.port or 80, u.path or "/"

    q: queue.Queue = queue.Queue(maxsize=args.rate * 3)
    buckets: list = []
    events: list = []
    lock = threading.Lock()
    start = time.monotonic()

    workers = [
        threading.Thread(target=worker, args=(host, port, path, q, start, buckets, lock))
        for _ in range(args.workers)
    ]
    for w in workers:
        w.start()
    ctl = threading.Thread(target=controller, args=(toggles, start, events))
    ctl.start()

    # Pace arrivals open-loop: each 10 ms slot enqueue rate/100 items on a monotonic schedule.
    slot = 0.01
    per_slot = max(1, round(args.rate * slot))
    n_slots = int(args.duration / slot)
    for i in range(n_slots):
        deadline = start + (i + 1) * slot
        for _ in range(per_slot):
            try:
                q.put_nowait(0)  # payload unused; 0 is just a "fire one request" token
            except queue.Full:
                break
        now = time.monotonic()
        if deadline > now:
            time.sleep(deadline - now)

    for _ in workers:
        q.put(STOP)
    for w in workers:
        w.join()
    ctl.join()

    # Merge per-thread buckets -> timeline.
    timeline = defaultdict(lambda: defaultdict(int))
    for b in buckets:
        for sec, insts in b.items():
            for k, v in insts.items():
                timeline[sec][k] += v

    secs = sorted(s for s in timeline if 0 <= s < args.duration)
    with open(args.out, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["t", "a", "b", "c", "failed"])
        for s in secs:
            row = timeline[s]
            w.writerow([s, row.get("a", 0), row.get("b", 0), row.get("c", 0), row.get("failed", 0)])
    with open(args.events_out, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["t", "label"])
        for t, label in sorted(events):
            w.writerow([t, label])

    total = sum(sum(v.values()) for v in timeline.values())
    failed = sum(v.get("failed", 0) for v in timeline.values())
    print(f"ejection_driver: {total} responses over {len(secs)} s, "
          f"{failed} failed ({100*failed/max(total,1):.2f}%); events={[e[1] for e in sorted(events)]}")


if __name__ == "__main__":
    main()
