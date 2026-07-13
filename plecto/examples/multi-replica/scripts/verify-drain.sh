#!/usr/bin/env bash
# Proves "drain one replica, zero dropped requests" on the base skeleton: constant curls
# through the LB while plecto-1 is stopped. SIGTERM flips /readyz to 503, the LB observes
# it within its fall × inter window and takes the replica out, THEN the drain starts
# (ADR 000059) — so no request ever lands on a dying replica.
set -euo pipefail
cd "$(dirname "$0")/.."
url="${1:-http://localhost:8080/}"
duration="${DURATION:-15}"

echo "curling $url for ${duration}s; stopping plecto-1 after 3s..."
(
  sleep 3
  docker compose stop plecto-1
) &
stopper=$!

total=0 failed=0
end=$((SECONDS + duration))
while ((SECONDS < end)); do
  curl -fsS --max-time 2 "$url" >/dev/null || failed=$((failed + 1))
  total=$((total + 1))
done
wait "$stopper"

echo "requests: $total  failed: $failed"
docker compose start plecto-1
if ((failed > 0)); then
  echo "FAIL: requests were dropped during the drain" >&2
  exit 1
fi
echo "OK: plecto-1 drained with zero dropped requests"
