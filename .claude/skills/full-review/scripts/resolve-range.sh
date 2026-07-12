#!/usr/bin/env bash
# Resolve a review range for full-review.
# Usage:
#   resolve-range.sh              # latest merge commit
#   resolve-range.sh HEAD         # latest merge if HEAD is merge, else first-parent merge base guess
#   resolve-range.sh <merge-sha>
#   resolve-range.sh pr <N>
# Prints: BASE HEAD SUBJECT
set -euo pipefail

mode="${1:-merge}"

if [[ "$mode" == "pr" ]]; then
  n="${2:?PR number required}"
  json="$(gh pr view "$n" --json baseRefOid,headRefOid,title,mergeCommit)"
  # Prefer merge commit parents when merged ("mergeCommit":{"oid":"..."} or null)
  merge="$(printf '%s' "$json" | python3 -c 'import json,sys; d=json.load(sys.stdin); print((d.get("mergeCommit") or {}).get("oid") or "")')"
  if [[ -n "$merge" ]]; then
    parents="$(git rev-parse "${merge}^1" "${merge}^2")"
    base="$(echo "$parents" | sed -n '1p')"
    head="$(echo "$parents" | sed -n '2p')"
  else
    head="$(printf '%s' "$json" | python3 -c 'import json,sys; print(json.load(sys.stdin)["headRefOid"])')"
    base="$(printf '%s' "$json" | python3 -c 'import json,sys; print(json.load(sys.stdin)["baseRefOid"])')"
  fi
  subject="$(printf '%s' "$json" | python3 -c 'import json,sys; print(json.load(sys.stdin)["title"])')"
  echo "$base $head $subject"
  exit 0
fi

sha="${1:-$(git log --merges -1 --format='%H')}"
if ! git rev-parse -q --verify "$sha^{commit}" >/dev/null; then
  echo "error: not a commit: $sha" >&2
  exit 1
fi

parents="$(git rev-list --parents -n 1 "$sha")"
# format: sha parent1 parent2 ...
set -- $parents
commit="$1"
shift
if [[ "$#" -lt 2 ]]; then
  echo "error: $commit is not a merge commit (need two parents). Pass pr <N> or base...head manually." >&2
  exit 1
fi
base="$1"
head="$2"
subject="$(git log -1 --format='%s' "$commit")"
echo "$base $head $subject"
