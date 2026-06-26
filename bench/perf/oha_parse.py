#!/usr/bin/env python3
"""Parse one oha JSON report and print 'rps p50 p90 p95 p99 p99_9' (latencies in ms).

oha reports latencyPercentiles in seconds; we scale to milliseconds. Used by run-perf.sh to turn
oha runs into wasm_overhead.csv / tls.csv rows.
"""
import json
import sys

d = json.load(open(sys.argv[1]))
lp = d["latencyPercentiles"]
rps = d["summary"]["requestsPerSec"]


def ms(key):
    return lp.get(key, 0) * 1000


print(f"{rps:.1f} {ms('p50'):.4f} {ms('p90'):.4f} {ms('p95'):.4f} {ms('p99'):.4f} {ms('p99.9'):.4f}")
