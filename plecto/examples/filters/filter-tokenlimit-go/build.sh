#!/usr/bin/env bash
# Build the Go (TinyGo) token-cost limiter into a Tier B ("fat guest") plecto:filter@0.4.0
# component (ADR 000063).
#
# Requires: TinyGo >= 0.41, wasm-tools >= 1.252, wit-bindgen-go
# (go.bytecodealliance.org/cmd/wit-bindgen-go, `go install ...@v0.7.0` — pin the version CI pins)
# on PATH, plus a Go toolchain for the module dependencies.
#
# WIT deps are VENDORED under `wit/deps/` and checked in, so this build fetches nothing:
#   - `wit/deps/plecto-filter-0.4.0/package.wit` is a verbatim copy of the canonical
#     `plecto/wit/world.wit`. The copy is asserted byte-identical below — a guest that silently
#     drifts from the contract it claims to implement is worse than one that fails to build.
#   - the `wasi:*` 0.2.0 packages come from the same pinned set the other Go guest uses.
# (`wkg wit fetch` cannot supply the first one: the `plecto` namespace resolves to this repo, not
# to a registry.)
#
# Unlike the Tier A guests, this is NOT zero-WASI: TinyGo's wasip2 target assumes the
# `wasi:cli/command` world, so `wit/world.wit` composes the plecto:filter interfaces with
# `include wasi:cli/imports@0.2.0` GUEST-SIDE (the canonical `plecto/wit/world.wit` stays
# untouched — ADR 000063 Decision 3). `wasi:filesystem` also appears: TinyGo's wasip2 runtime
# unconditionally imports it even though this program touches no file — the host lends an EMPTY
# one (zero preopens, so zero reachable paths).
set -euo pipefail
cd "$(dirname "$0")"

canonical_wit="../../../wit/world.wit"
vendored_wit="wit/deps/plecto-filter-0.4.0/package.wit"
if ! diff -q "$canonical_wit" "$vendored_wit" >/dev/null; then
  echo "ERROR: $vendored_wit has drifted from the canonical contract at $canonical_wit." >&2
  echo "       Re-vendor it (cp \"$canonical_wit\" \"$vendored_wit\") and rebuild." >&2
  exit 1
fi

wit-bindgen-go generate --world filter-request-body --out internal ./wit
mkdir -p dist
tinygo build -target=wasip2 -o dist/filter_tokenlimit_go.wasm \
  --wit-package wit --wit-world filter-request-body main.go

# Tier B allowlist (ADR 000063 Decision 4): unlike Tier A's bare "no wasi:* at all", a fat guest
# may import ONLY io / clocks / random / cli / filesystem — never sockets or http (the outbound
# capabilities stay their own separate, allowlisted opt-in, ADR 000036 / 000060).
imports="$(wasm-tools component wit dist/filter_tokenlimit_go.wasm | grep -oE 'wasi:[a-z-]+' | sort -u)"
disallowed="$(echo "$imports" | grep -vE '^wasi:(io|clocks|random|cli|filesystem)$' || true)"
if [ -n "$disallowed" ]; then
  echo "ERROR: dist/filter_tokenlimit_go.wasm imports WASI outside the Tier B allowlist:" >&2
  echo "$disallowed" >&2
  exit 1
fi
if ! echo "$imports" | grep -q '^wasi:cli$'; then
  echo "ERROR: dist/filter_tokenlimit_go.wasm imports no wasi:cli — is this still a fat guest?" >&2
  exit 1
fi

# The export set is part of the contract too: `on-response-body` MUST stay absent, because the
# host reads that absence as "never buffer the response body" (ADR 000038 / 000098).
exports="$(wasm-tools component wit dist/filter_tokenlimit_go.wasm | grep -oE '^  export [a-z-]+' | awk '{print $2}' | sort)"
expected="$(printf 'init\non-request\non-request-body\non-response\n' | sort)"
if [ "$exports" != "$expected" ]; then
  echo "ERROR: unexpected export set in dist/filter_tokenlimit_go.wasm:" >&2
  diff <(echo "$expected") <(echo "$exports") >&2 || true
  exit 1
fi

echo "OK: dist/filter_tokenlimit_go.wasm (Tier B: wasi: imports confined to io/clocks/random/cli/filesystem)"
echo "$imports" | sed 's/^/  /'
