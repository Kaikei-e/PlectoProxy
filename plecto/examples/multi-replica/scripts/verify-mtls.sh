#!/usr/bin/env bash
# Proves downstream mTLS (scenario B, ADR 000078): a handshake without a client
# certificate is refused; the demo client certificate gets a proxied 200.
set -euo pipefail
cd "$(dirname "$0")/.."
url="${1:-https://localhost:8443/}"

if curl -fsS --max-time 5 --cacert manifests/secrets/server.crt "$url" >/dev/null 2>&1; then
  echo "FAIL: a connection without a client certificate was accepted" >&2
  exit 1
fi
echo "OK: refused without a client certificate"

curl -fsS --max-time 5 --cacert manifests/secrets/server.crt \
  --cert manifests/secrets/client.crt --key manifests/secrets/client.key "$url" >/dev/null
echo "OK: proxied response with the demo client certificate"
