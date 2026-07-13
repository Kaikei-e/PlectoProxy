# 検証マップ

[English](verification.md)

Plecto が何を・どこで検証しているかの一覧。このページは**台帳ではなく、機構への地図**
である: 下表の各項目の記録の実体は、対応する workflow が default branch（release 系
ジョブは tag）で green であることであり、独立したスコアボードは維持しない——だから
このページは実態から静かに乖離できない（[ADR 000086](ADR/000086.md) /
[ADR 000089](ADR/000089.md)）。

CI は意図して **PR-light / merge-heavy** に分割している: pull request には高速で
シグナルの高いジョブを、重いビルドは `main` で回す（release gate が要求するのは
後者）。schedule 実行は、どちらもブロックする必要のないものを受け持つ。

| 検証対象 | ジョブ（workflow） | タイミング |
| --- | --- | --- |
| フォーマット（`cargo fmt --check`） | `fmt`（[ci.yml](../.github/workflows/ci.yml)） | 全 PR + main |
| Lint、全 feature、warning をエラー扱い | `clippy`（ci.yml） | 全 PR + main |
| テストスイート、minimal profile（default features） | `test`（ci.yml） | 全 PR + main |
| テストスイート、capability superset + **polyglot conformance**（MoonBit / JS / C の zero-WASI ゲストと Go/TinyGo fat ゲストを、全言語同一アサーションで検証） | `test-features`（ci.yml） | 全 PR + main |
| reference filter の encode + import floor | `shelf`（ci.yml） | 全 PR + main |
| guest crate の lint | `guest-lint`（ci.yml） | 全 PR + main |
| 供給網ポリシー（ライセンス・advisory・取得元） | `cargo-deny`（ci.yml） | 全 PR + main |
| ADR グラフ（append-only エッジ・wikilink・frontmatter） | `docs`（ci.yml → `scripts/check_adr_graph.py`） | 全 PR + main |
| 両 capability profile の release ビルド | `release-parity`（ci.yml） | main のみ（merge-heavy） |
| **Fuzzing** — `plecto/fuzz/` の全 libfuzzer ターゲットを、コミット済み corpus 起点で時間制限つき実行 | `fuzz`（[fuzz.yml](../.github/workflows/fuzz.yml)） | 週次 + 手動 |
| release gate: その commit で `main` CI が green のときのみ tag がリリースされる | `gate`（[release.yml](../.github/workflows/release.yml)） | 全 tag |
| 署名付き成果物: cargo-auditable バイナリ、SPDX SBOM、**digest への** cosign keyless 署名、provenance / SBOM attestation、署名付き reference-filter OCI artifact | `binaries` / `container-*` / `filter-publish`（release.yml） | 全 tag |
| unsolicited PR ポリシー（招待制コントリビューション） | [pr-policy.yml](../.github/workflows/pr-policy.yml) | 全 PR |

正直な限界（含意ではなく明記する）:

- **Fuzzing は週次・時間制限つきの smoke** である——コミット済み corpus からターゲット
  あたり数分であり、継続 fuzzing 基盤ではない。最初のターゲットは
  [ADR 000057](ADR/000057.md) が導入した untrusted 入力面である PROXY protocol v2
  パーサ。
- **ベンチマーク**（[bench.yml](../.github/workflows/bench.yml)）は計測であって gate
  ではない。公開数値は [performance/](../performance/README.md) にある。
- 各検証が何を*意味し*、何を意図して主張しないかは、各所からリンクされた ADR に記録
  されている。契約互換の約束と longevity discipline は README の
  [設計判断（ADR）](../README.ja.md#設計判断adr) を参照。
