#!/usr/bin/env bash
# Build the MoonBit guest into a zero-WASI plecto:filter component.
#
# Requires: moon (MoonBit toolchain), wasm-tools >= 1.252 on PATH.
#
# The wit-bindgen bindings (interface/ world/ gen/ + moon.mod.json) are COMMITTED, not
# regenerated here: gen/world/filterBody/ carries the hand-written filter.mbt and a
# moon.pkg.json whose host-API imports wit-bindgen would reset. Regenerate only when
# the WIT changes, then re-add those imports:
#   wit-bindgen moonbit ../../../wit --world filter-body --out-dir .
#
# MoonBit strings are UTF-16, so `component embed` must declare --encoding utf16 —
# without it every string crossing the boundary is lifted as (wrong) UTF-8.
set -euo pipefail
cd "$(dirname "$0")"

moon build --target wasm --release
mkdir -p dist
core=_build/wasm/release/build/gen/gen.wasm
wasm-tools component embed ../../../wit --world filter-body --encoding utf16 "$core" -o dist/.embedded.wasm
wasm-tools component new dist/.embedded.wasm -o dist/filter_hello_moonbit.wasm
rm dist/.embedded.wasm

if wasm-tools component wit dist/filter_hello_moonbit.wasm | grep -q 'wasi:'; then
  echo "ERROR: dist/filter_hello_moonbit.wasm imports WASI — the default Linker will refuse it" >&2
  exit 1
fi
echo "OK: dist/filter_hello_moonbit.wasm (zero WASI imports)"
