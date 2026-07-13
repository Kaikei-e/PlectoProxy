# Verification map

[日本語](verification.ja.md)

What Plecto verifies, and where. This page is a **map to the machinery, not a ledger**:
the record of every item below is the corresponding workflow being green on the default
branch (or on the tag, for release jobs) — no separate scoreboard is maintained, so this
page cannot silently drift from what actually runs
([ADR 000086](ADR/000086.md) / [ADR 000089](ADR/000089.md)).

CI is split **PR-light / merge-heavy** deliberately: pull requests get the fast,
high-signal jobs; expensive builds run on `main` (and are what the release gate
requires). Scheduled jobs cover what neither needs to block on.

| What is verified | Job (workflow) | When |
| --- | --- | --- |
| Formatting (`cargo fmt --check`) | `fmt` ([ci.yml](../.github/workflows/ci.yml)) | every PR + main |
| Lints, all features, warnings-as-errors | `clippy` (ci.yml) | every PR + main |
| Test suite, minimal profile (default features) | `test` (ci.yml) | every PR + main |
| Test suite, capability superset + **polyglot conformance** (MoonBit / JS / C zero-WASI guests, Go/TinyGo fat guests — same assertions for every language) | `test-features` (ci.yml) | every PR + main |
| Reference filters encode + import floor | `shelf` (ci.yml) | every PR + main |
| Guest crate lints | `guest-lint` (ci.yml) | every PR + main |
| Supply-chain policy (licenses, advisories, sources) | `cargo-deny` (ci.yml) | every PR + main |
| ADR graph (append-only edges, wikilinks, frontmatter) | `docs` (ci.yml → `scripts/check_adr_graph.py`) | every PR + main |
| Release-profile builds of both capability profiles | `release-parity` (ci.yml) | main only (merge-heavy) |
| **Fuzzing** — every libfuzzer target (`plecto/fuzz/`), bounded run from the committed corpus | `fuzz` ([fuzz.yml](../.github/workflows/fuzz.yml)) | weekly + on demand |
| Release gate: a tag only releases if `main` CI was green for that commit | `gate` ([release.yml](../.github/workflows/release.yml)) | every tag |
| Signed artifacts: cargo-auditable binaries, SPDX SBOM, cosign keyless signatures **by digest**, provenance/SBOM attestations, signed reference-filter OCI artifacts | `binaries` / `container-*` / `filter-publish` (release.yml) | every tag |
| Unsolicited-PR policy (invitation-only contributions) | [pr-policy.yml](../.github/workflows/pr-policy.yml) | every PR |

Honest bounds, stated rather than implied:

- **Fuzzing is a weekly, time-bounded smoke** — minutes per target from the committed
  corpus, not a continuous fuzzing farm. Its first target is the PROXY protocol v2
  parser, the untrusted-input surface introduced by [ADR 000057](ADR/000057.md).
- **Benchmarks** ([bench.yml](../.github/workflows/bench.yml)) measure, they do not
  gate; published numbers live in [performance/](../performance/README.md).
- What each verification *means* — and what it deliberately does not claim — is
  recorded in the ADRs linked throughout; the contract compatibility promise and the
  longevity discipline are in the README's
  [Design decisions](../README.md#design-decisions).
