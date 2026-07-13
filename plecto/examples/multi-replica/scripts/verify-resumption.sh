#!/usr/bin/env bash
# Proves the shared STEK (scenario A, ADR 000062): a session ticket issued by plecto-1
# resumes on plecto-2 — both replicas derive the same ticket keys from the shared key
# file, so resumption survives LB re-balancing. Uses the direct per-replica ports from
# compose.scenario-a.yaml to make the CROSS-replica hop deterministic.
set -euo pipefail
cd "$(dirname "$0")/.."
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# Hold the first connection open a beat so the TLS 1.3 ticket (sent after the
# handshake) is received before s_client exits.
sleep 1 | openssl s_client -connect localhost:18443 -servername localhost \
  -CAfile manifests/secrets/server.crt -sess_out "$tmp/session.pem" >/dev/null 2>&1

out=$(sleep 1 | openssl s_client -connect localhost:28443 -servername localhost \
  -CAfile manifests/secrets/server.crt -sess_in "$tmp/session.pem" 2>/dev/null)

if grep -q "Reused, TLSv1.3" <<<"$out"; then
  echo "OK: ticket from plecto-1 resumed on plecto-2 (shared STEK)"
else
  echo "FAIL: no cross-replica resumption observed" >&2
  grep -m1 -E "New|Reused" <<<"$out" >&2 || true
  exit 1
fi
