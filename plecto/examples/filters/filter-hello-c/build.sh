#!/usr/bin/env bash
# Build the C guest into a zero-WASI plecto:filter component.
#
# Requires: wasi-sdk >= 33 ($WASI_SDK_PATH, default /opt/wasi-sdk),
#           wit-bindgen >= 0.60, wasm-tools >= 1.252 on PATH.
#
# The wasm32-wasip2 clang driver links with wasm-component-ld and emits a component
# directly. Because the filter calls no WASI API (time comes from host-clock, logging
# from host-log), the component imports ONLY the plecto host-API — the zero-WASI
# property the default deny-by-default Linker requires. The final grep makes that
# property a build failure instead of a load-time surprise.
set -euo pipefail
cd "$(dirname "$0")"

WASI_SDK_PATH="${WASI_SDK_PATH:-/opt/wasi-sdk}"

wit-bindgen c ../../../wit --world filter-body --out-dir gen
mkdir -p dist
"$WASI_SDK_PATH/bin/clang" --target=wasm32-wasip2 -mexec-model=reactor -O2 -Wall -Wextra -Igen \
  src/filter.c gen/filter_body.c gen/filter_body_component_type.o \
  -o dist/filter_hello_c.wasm

if wasm-tools component wit dist/filter_hello_c.wasm | grep -q 'wasi:'; then
  echo "ERROR: dist/filter_hello_c.wasm imports WASI — the default Linker will refuse it" >&2
  exit 1
fi
echo "OK: dist/filter_hello_c.wasm (zero WASI imports)"
