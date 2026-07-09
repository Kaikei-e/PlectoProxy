#!/usr/bin/env bash
# Build the Go (TinyGo) guest into a Tier B ("fat guest") plecto:filter component (ADR 000063).
#
# Requires: TinyGo >= 0.34, wasm-tools >= 1.252, wkg (wasm-pkg-tools), wit-bindgen-go
# (go.bytecodealliance.org/cmd/wit-bindgen-go, `go install ...@v0.7.0` — pin to the version CI
# pins, .github/workflows/ci.yml job `polyglot-guest-go`) on PATH.
#
# Unlike the Tier A guests (filter-hello-{c,moonbit,js}), this is NOT zero-WASI: TinyGo's wasip2
# target assumes the `wasi:cli/command` world, so `wit/world.wit` composes the base
# `plecto:filter` world with `include wasi:cli/imports@0.2.0` on the guest side (the shared
# `plecto/wit/world.wit` stays untouched — ADR 000063 Decision 3). `wasi:filesystem` also appears
# in the import set: TinyGo's wasip2 runtime unconditionally imports it even though this program
# touches no file (confirmed against TinyGo 0.41.1) — the host lends an EMPTY one (zero preopens,
# so zero reachable paths; see `add_inert_filesystem` in crates/host/src/state.rs). `wasi:sockets`
# is also present in the WIT type universe (a wasi:cli/imports transitive dependency) but never
# imported by the compiled component below, and the host links no `wasi:sockets` for a fat guest —
# confirmed by the allowlist assertion.
set -euo pipefail
cd "$(dirname "$0")"

wkg wit fetch -t wit
wit-bindgen-go generate --world filter-body-go --out internal ./wit
mkdir -p dist
tinygo build -target=wasip2 -o dist/filter_hello_go.wasm --wit-package wit --wit-world filter-body-go main.go

# Tier B allowlist (ADR 000063 Decision 4): unlike Tier A's bare "no wasi:* at all", a fat guest
# may import ONLY io / clocks / random / cli / filesystem — never sockets or http (the outbound
# capabilities stay their own separate, allowlisted opt-in, ADR 000036 / 000060).
imports="$(wasm-tools component wit dist/filter_hello_go.wasm | grep -oE 'wasi:[a-z-]+' | sort -u)"
disallowed="$(echo "$imports" | grep -vE '^wasi:(io|clocks|random|cli|filesystem)$' || true)"
if [ -n "$disallowed" ]; then
  echo "ERROR: dist/filter_hello_go.wasm imports WASI outside the Tier B allowlist:" >&2
  echo "$disallowed" >&2
  exit 1
fi
if ! echo "$imports" | grep -q '^wasi:cli$'; then
  echo "ERROR: dist/filter_hello_go.wasm imports no wasi:cli — is this still a fat guest?" >&2
  exit 1
fi
echo "OK: dist/filter_hello_go.wasm (Tier B: wasi: imports confined to io/clocks/random/cli/filesystem)"
echo "$imports" | sed 's/^/  /'
