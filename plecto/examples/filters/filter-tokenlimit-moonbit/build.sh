#!/usr/bin/env bash
# Build the MoonBit token-cost limiter into a zero-WASI plecto:filter component.
#
# Requires: moon (MoonBit toolchain), wasm-tools >= 1.252 on PATH.
#
# This guest declares its OWN world (wit/world.wit, `filter-request-body`): the base `filter`
# floor plus `on-request-body` and deliberately without `on-response-body`. The canonical
# contract is never edited to make room for it (ADR 000063 Decision 3); instead wit/deps/ is
# MATERIALISED here from plecto/wit/world.wit, so the 0.4.0 types this world `use`s come from
# the contract itself and a vendored copy cannot drift away from it.
#
# The wit-bindgen bindings (interface/ world/ gen/ + moon.mod.json) are COMMITTED, not
# regenerated here: gen/world/filter-request-body/ carries the hand-written tokenlimit.mbt and
# a moon.pkg.json whose host-API imports wit-bindgen would reset to just `types`. Regenerate
# only when the WIT changes, then re-add the hostLog / hostRatelimit / hostConfig imports:
#   mkdir -p wit/deps/plecto-filter && cp ../../../wit/world.wit wit/deps/plecto-filter/
#   wit-bindgen moonbit ./wit --world filter-request-body --out-dir .
#
# MoonBit strings are UTF-16, so `component embed` must declare --encoding utf16 — without it
# every string crossing the boundary is lifted as (wrong) UTF-8.
set -euo pipefail
cd "$(dirname "$0")"

mkdir -p wit/deps/plecto-filter
cp ../../../wit/world.wit wit/deps/plecto-filter/world.wit

moon build --target wasm --release
mkdir -p dist
core=_build/wasm/release/build/gen/gen.wasm
wasm-tools component embed wit --world filter-request-body --encoding utf16 "$core" -o dist/.embedded.wasm
wasm-tools component new dist/.embedded.wasm -o dist/filter_tokenlimit_moonbit.wasm
rm dist/.embedded.wasm

wit_text="$(wasm-tools component wit dist/filter_tokenlimit_moonbit.wasm)"

# Tier A: the default deny-by-default Linker lends no WASI at all, so a single wasi: import is
# a load failure. Catching it here makes it a build failure instead.
if echo "$wit_text" | grep -q 'wasi:'; then
  echo "ERROR: dist/filter_tokenlimit_moonbit.wasm imports WASI — the default Linker will refuse it" >&2
  exit 1
fi

# The absence of on-response-body is contractual, not incidental: the host reads it as "this
# filter never inspects a response body" and keeps that direction streaming zero-copy. A
# regenerated binding that quietly added the export would silently re-import the buffer tax.
if echo "$wit_text" | grep -q 'export on-response-body'; then
  echo "ERROR: dist/filter_tokenlimit_moonbit.wasm exports on-response-body — this filter must not read response bodies" >&2
  exit 1
fi
for want in 'export init' 'export on-request:' 'export on-request-body:' 'export on-response:'; do
  if ! echo "$wit_text" | grep -q "$want"; then
    echo "ERROR: dist/filter_tokenlimit_moonbit.wasm is missing '$want'" >&2
    exit 1
  fi
done

echo "OK: dist/filter_tokenlimit_moonbit.wasm (zero WASI imports)"
echo "$wit_text" | grep -E '^\s+(import|export) ' | sed 's/^\s*/  /'
