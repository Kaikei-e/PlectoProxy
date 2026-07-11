#!/usr/bin/env bash
# Build the ADR 000080 reference-filter shelf the same way release.yml publishes it.
#
# Used by:
#   - .github/workflows/ci.yml   (release-parity: encode path + import floor)
#   - .github/workflows/release.yml (filter-publish: build, then push/sign/attest)
#
# Usage (from repo root):
#   ./scripts/build-reference-filters.sh <out-dir>
#
# Writes one component per shelf entry to <out-dir>/<short>.component.wasm and a
# <out-dir>/manifest.tsv of short, kind, profile, content-sha256, path. The sha256 is
# taken after `wasm-tools strip` so custom-section noise does not fake a content
# mismatch on republish. Fails closed if a zero-WASI guest imports wasi:http or a
# capabilities guest lacks it.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:?usage: $0 <out-dir>}"
mkdir -p "${OUT}"

: >"${OUT}/manifest.tsv"

# content_sha <wasm> — sha256 of the stripped component (deterministic enough for
# republish comparison; the published artifact remains the unstripped bytes).
content_sha() {
  local src="$1" stripped
  stripped="$(mktemp)"
  # strip drops custom sections (names / producers / dwarfish noise) that commonly
  # differ across otherwise-identical release builds.
  wasm-tools strip "${src}" -o "${stripped}"
  sha256sum "${stripped}" | awk '{print $1}'
  rm -f "${stripped}"
}

# build_one <short> <dir> <kind> <profile>
#   kind: core | wasip2  — same contract as release.yml's filter-publish.
build_one() {
  local short="$1" dir="$2" kind="$3" profile="$4"
  local guest="${ROOT}/plecto/examples/filters/${dir}"
  local stem="${dir//-/_}"
  local component="${OUT}/${short}.component.wasm"
  local wit_text

  if [ "${kind}" = "core" ]; then
    (cd "${guest}" && cargo build --target wasm32-unknown-unknown --release --locked)
    # CLI face of the wit-component encoder crates/host/build.rs uses (wasm-tools 1.252 ↔
    # wit-component 0.252). No WASI adapter: the guest imports only granted plecto caps.
    wasm-tools component new \
      "${guest}/target/wasm32-unknown-unknown/release/${stem}.wasm" \
      -o "${component}"
  else
    (cd "${guest}" && cargo build --target wasm32-wasip2 --release --locked)
    cp "${guest}/target/wasm32-wasip2/release/${stem}.wasm" "${component}"
  fi

  # Import floor vs the compatibility matrix (docs/reference-filters.md). wkg will embed the
  # same imports into the OCI wasm config on push; catching drift here keeps the matrix honest.
  wit_text="$(wasm-tools component wit "${component}")"
  case "${kind}" in
    core)
      if grep -q 'wasi:http' <<<"${wit_text}"; then
        echo "error: ${short}: zero-WASI shelf entry unexpectedly imports wasi:http" >&2
        exit 1
      fi
      if ! grep -q 'plecto:filter' <<<"${wit_text}"; then
        echo "error: ${short}: component does not mention plecto:filter (wrong world?)" >&2
        exit 1
      fi
      ;;
    wasip2)
      if ! grep -q 'wasi:http' <<<"${wit_text}"; then
        echo "error: ${short}: capabilities shelf entry missing wasi:http import" >&2
        exit 1
      fi
      if ! grep -q 'plecto:filter' <<<"${wit_text}"; then
        echo "error: ${short}: component does not mention plecto:filter (wrong world?)" >&2
        exit 1
      fi
      ;;
    *)
      echo "error: unknown kind '${kind}'" >&2
      exit 1
      ;;
  esac

  local sha
  sha="$(content_sha "${component}")"
  printf '%s\t%s\t%s\t%s\t%s\n' "${short}" "${kind}" "${profile}" "${sha}" "${component}" \
    >>"${OUT}/manifest.tsv"
  echo "built ${short}: content-sha256:${sha}"
}

build_one jwt      filter-jwt      wasip2 capabilities
build_one cors     filter-cors     core   "any (minimal+)"
build_one apikey   filter-apikey   core   "any (minimal+)"
build_one extauthz filter-extauthz wasip2 capabilities

echo "wrote ${OUT}/manifest.tsv"
