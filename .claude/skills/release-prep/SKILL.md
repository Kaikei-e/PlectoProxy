---
name: release-prep
description: Prepare a PlectoProxy release locally — decide the bump, land the two-commit convention (chore(deps) then chore(release)), sweep every version-bearing file, and run the verification chain in the order that actually works on the maintainer box (just check → just release-check → T1 gate) plus the extra gates a guest-toolchain bump needs. Use when the user says 「リリース準備」「バージョン上げて」「0.x.y にして」「過去と同じ手順で」 or after a dependency/toolchain bump that should ship. Never pushes, tags, or publishes.
allowed-tools: Bash, Read, Glob, Grep, Edit, Write
argument-hint: <patch|minor|X.Y.Z>
---

# Release prep (PlectoProxy)

ローカルで「push すれば出せる」状態まで整える。**push / tag / `cargo publish` は maintainer の手動
作業**（`docs/plans/crates-io-publishing-guide-plecto.md` §4、意図的に自動化しない）。このスキルは
それらを絶対に実行しない。

## Phase 0: バンプ幅を決める

CHANGELOG 冒頭の Versioning policy (pre-1.0) に従う:

- WIT 契約 / manifest schema / CLI / 公開 API に変更 → **minor**（`0.11 → 0.12`）
- 依存・ツールチェイン・内部修正のみ → **patch**
- ライブラリ crate の判定は推測せず `just semver-check` の結果で決める（Phase 2）

`$ARGUMENTS` が `X.Y.Z` ならそれを採用、`patch` / `minor` なら `plecto/Cargo.toml` の
`[workspace.package] version` から算出する。

## Phase 1: 二段コミットとファイル掃引

過去のリリース（`git log --grep='chore(release)'`）と同じ形にする:

1. `chore(deps): …` — 依存 / ツールチェインの変更だけ。リリース関連 hunk は含めない。
2. `chore(release): prepare X.Y.Z` — 版数掃引 + CHANGELOG。

版数を持つファイル（`grep -rn '<old-version>'` で毎回実測し、この一覧は下限として扱う）:

| ファイル | 何を上げるか |
| --- | --- |
| `plecto/Cargo.toml` | `[workspace.package] version` と `[workspace.dependencies]` の `plecto-host/-control/-server` の `version` |
| `plecto/Cargo.lock` | `cargo check --offline` で plecto-* 行が追従する（手で書かない） |
| `CHANGELOG.md` | `## [X.Y.Z] - YYYY-MM-DD` 節。patch なら「Patch release:」前置き（semver-checks clean / filters need no rebuild / contract stays `plecto:filter@0.4.0`） |
| `README.md` / `README.ja.md` / `docs/install*.md` / `docs/quickstart/README*.md` | `TAG=X.Y.Z` |
| `docs/operations*.md` | `plecto X.Y.Z (profile: minimal)` の出力例 |
| `plecto/examples/multi-replica/compose.yaml` | `ghcr.io/kaikei-e/plecto:X.Y.Z` |

コミットは分けるが、**実装途中でステップ単位に commit しない**（ユーザ規約）。commit 前にユーザ確認。

## Phase 2: 検証チェーン（この順、cargo は同時に 1 本）

maintainer の 24 コア機は wasmtime 規模の workspace で OOM しかける。cargo を並列に走らせず、
`CARGO_BUILD_JOBS=4` を環境に置いてから始める。

```bash
just check          # fmt-check + clippy -D warnings + cargo test --all + drift-check
just release-check  # drift-check + semver-check + publish-dry-run
just bench-build && just gate   # T1 perf gate — runtime (wasmtime) やホットパスが動いたときのみ
```

`just semver-check` は plecto-host を `--default-features` で見る。all-features で出る
`--print=file-names missing ... --target wasm32-unknown-unknown` は build.rs のゲストビルド失敗で、
**API 破壊ではない**。CI (`semver-checks` job) も同じ理由で default-features を使っている。

`just gate` の帯（`bench/perf/gate_tolerances.toml`）は狭い。上限際の値は PASS でも数値ごと報告する。
`performance/data/gate.csv` は git-ignored なので commit 対象にならない。

CI に対応する feature 一括テスト（polyglot Tier A/B・tokenlimit・outbound）はローカルでも回す:
`cargo test -p plecto-host --features polyglot-conformance,fat-guest --test polyglot --test polyglot_tier_b`
等、`.github/workflows/ci.yml` の該当 job の `run:` をそのまま写す。

## Phase 3: ゲストツールチェインを上げたときの追加ゲート

wit-bindgen / componentize-js / wasi-sdk / wasm-tools のいずれかを動かしたら、Phase 2 に加えて:

- **reference filter shelf の版上げ**（最優先、tag を打つ前に）: wit-bindgen や依存の更新で
  `filter-jwt` / `filter-cors` / `filter-apikey` / `filter-extauthz` のコンポーネントバイトが変わると、
  release.yml の filter-publish が「同版で内容不一致」で fail-closed する（ADR 000080、tag は不変）。
  `./scripts/build-reference-filters.sh <out>` の content-sha256 を、公開済み
  （`wkg oci pull ghcr.io/kaikei-e/plecto/filters/<short>:<version>` → `wasm-tools strip` → sha256、
  wasm-tools は CI pin と同版）と照合し、違う filter は Cargo.toml の version を patch 上げ →
  `cargo update -w --offline` で Cargo.lock 追従 → `docs/reference-filters.md` の互換行列 → CHANGELOG に
  「Reference-filter shelf republished」bullet。前例: `98e7fef`、`c0ada86`。
- **vendored template**: `examples/filters/filter-template/` を変えたら `just sync-template-crate`。
  `crates/plecto/templates/filter-template/` はバイト一致が要件（`check_wit_vendoring.py`）。
- **JS lockfile**: `just regen-js-lockfiles`。in-place の `npm install` は peer dep を nested-only に
  残し、runner の新しい npm が `npm ci` で拒否する（ローカルの npm では通ってしまう）。
- **系列パリティ**: `wit-component` / `wit-parser` と CI の `wasm-tools` CLI は wasmtime が同梱する
  wasm-tools 系列に固定（ADR 000114、`scripts/build-reference-filters.sh` の不変条件）。wit-bindgen の
  バンプでは動かさない。wasmtime のメジャーが上がったときだけ一緒に動く。
- **ポリグロットゲスト再ビルド**: `ci.yml` polyglot job が pin する版（wasm-tools / wit-bindgen CLI /
  wasi-sdk / tinygo）と同じ版で `examples/filters/*/build.sh` を通し、import アサーション
  （Tier A = WASI import ゼロ、Tier B = `wasi:` allowlist）が保たれることを確認する。PATH 上の
  wasm-tools が pin より新しいことがあるので版を必ず出力する。
- **MoonBit の生成コード**: コミット済みバインディングは、契約が使う経路に変更がない限り再生成しない。
  再生成すると `gen/world/filterBody/moon.pkg.json` の import が消えるので手で戻す。
- CI の sha256 pin（wit-bindgen / wasi-sdk のリリース tarball）を更新した場合は GitHub Releases の
  `.sha256` を一次情報として突き合わせる。

## Phase 4: 引き渡し

報告に必ず含める: 作ったコミット（hash + 件名）、**push していないこと**、各ゲートの結果
（テスト件数・semver 判定・gate の数値）、据え置いた更新とその理由、残る手動手順
（`git push` → tag → `cargo publish`、`docs/plans/crates-io-publishing-guide-plecto.md` §4）。
push するかは 1 行で確認し、番号参照や代名詞では実行しない。
