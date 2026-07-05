#!/usr/bin/env bash
# Build the JavaScript guest into a zero-WASI plecto:filter component.
#
# Requires: node >= 20 + npm, wasm-tools >= 1.252 on PATH.
#
# ComponentizeJS embeds the StarlingMonkey engine (~12 MB fixed cost, independent of
# filter code size). build.mjs disables every WASI-backed engine feature
# (random/stdio/clocks/http/fetch-event), so the result is a "pure component"
# importing only the plecto host-API.
set -euo pipefail
cd "$(dirname "$0")"

npm ci
node build.mjs

if wasm-tools component wit dist/filter_hello_js.wasm | grep -q 'wasi:'; then
  echo "ERROR: dist/filter_hello_js.wasm imports WASI — the default Linker will refuse it" >&2
  exit 1
fi
echo "OK: dist/filter_hello_js.wasm (zero WASI imports)"
