---
name: plecto-adr-writer
description: Writes an Architecture Decision Record for the Plecto project in Japanese after a decision or completed implementation. Authoring only — no deploy / no container rebuild (Plecto has no deploy infra yet). Trigger when the user says "ADR書いて" / "ADRにまとめて" / "ADRに記録して" / "docs/ADR" 関連のまとめ依頼, or after a change that clearly warrants a decision record (a Fork-style 判断/根拠/再検討条件).
allowed-tools: Bash, Read, Glob, Grep, Edit, Write
---

# Plecto ADR Writer

Plecto の Architecture Decision Record を **日本語で執筆する**だけのスキル。デプロイ・コンテナ
再ビルドは **一切扱わない**（Plecto にデプロイ基盤がまだ無いため）。Plecto の意思決定は
**Fork 形式（判断 / 根拠 / 再検討条件）**で行われるので、ADR はそれを自然に受ける。

実行は 2 ステップ:

1. **実装確認** (§1) — テスト可能なコードがあれば green を担保してから書く
2. **ADR 執筆** (§2) — `docs/ADR/NNNNNN.md` を日本語で追加する

---

## §1. 実装確認（コードがある場合のみ）

ADR が実装の決定記録なら、書く前に最低限のテストで動作を確認する。コードを伴わない純粋な設計判断
（例: `plecto:filter` ワールドの採用方針）なら §1 はスキップして §2 へ。

| 変更の種類 | 最低限回すコマンド |
|---|---|
| Rust（fast path / host） | `cargo test --all`（型/lint は `cargo clippy --all-targets -- -D warnings`） |
| JS/TS（tooling / filter） | `npx tsc --noEmit && npx vitest run`（or `node --test`） |
| WASM フィルタ | ビルド（`cargo build --target wasm32-wasip2` / `jco componentize`）＋ conformance テスト |
| ドキュメント・scripts のみ | 該当テストだけ |

テストが落ちていたら ADR は書かず、原因を報告して止まる。ADR は「動いた実装の決定記録」であり、
憶測を書く場所ではない。

---

## §2. ADR 執筆

### 2.1 番号とテンプレート

番号は手で数えない。`docdag` に採番させる:

```bash
docdag new "<title>" --dry-run --format json   # => {"id": "000115", "path": "docs/ADR/000115.md", ...}
```

`--dry-run` は何も書かずに次の空き番号を予約し、`docdag.yaml` の `filename: "{id}.md"` に従った
`docs/ADR/NNNNNN.md` を返す。`docs/ADR/template.md` は Go template ではなく日本語の雛形なので、
本文生成は `docdag` にさせない——返ってきた `path` に、`template.md` を Read して**その見出しをその
まま使い**（勝手に増減しない）Write する。

### 2.2 既存 ADR の調べ方

`ls` / `grep` でディレクトリを走査しない。グラフに聞く:

| 知りたいこと | コマンド |
|---|---|
| ある ADR と、それが解決する先・近傍（Decision 抜粋つき） | `docdag context 000104 --budget 400` |
| いま有効な決定の一覧（id / title / status） | `docdag query --binding --fields id,title,status` |
| superseded な ADR の後継 | `docdag resolve 000021` |

### 2.3 Frontmatter

| フィールド | 値の決め方 |
|---|---|
| `title` | 動詞始まりの行動指向の一文。ADR 番号は含めない |
| `date` | `YYYY-MM-DD`（当日） |
| `status` | 新規は `proposed`、合意済みなら `accepted`。過去 ADR を無効化する場合のみ `superseded` |
| `tags` | §2.5 の許可タグから最大 5 個 |
| `affected_components` | コンポーネント名と変更概要を 1 行/件で列挙（例: `host runtime — pooling 再利用を追加`） |
| `aliases` | `ADR-NNN` と `ADR-000NNN` の 2 形式を必ず両方入れる（wikilink 解決用） |
| `amends` / `supersedes` | 先行 ADR を精緻化・訂正するなら `amends: ["000052"]`、完全に置き換えるなら `supersedes: ["000037"]` |

`amends` を書いたら、**相手側の frontmatter に `amended_by: ["<新 ADR>"]` を追記する**（既にあれば
末尾に足す）。これは `docdag.yaml` の `inverse: amended_by` が強制する相互性で、欠けると
`inverse_mismatch` エラーになる。確定済み ADR に対して append-only 履歴が許す frontmatter 編集は
この追記だけなので、**他のキーや本文には触らない**。

### 2.4 本文ルール

- **日本語で書く**。コンポーネント名 / コマンド / ライブラリ名 / WIT / ファイルパスは英語のまま。
- **セクション順は `template.md` を尊重**。Status / Date / Affected Components / Context / Decision /
  Consequences (Pros, Cons/Tradeoffs) / Related ADRs の順が基本。
- **Context** は「なぜこの決定が必要だったか」を定量/定性の根拠とともに書く。計測結果があれば数値を残す。
- **Decision** は採用した選択肢に加え、**検討した代替案と却下理由**を書く。これが後から読む人への最大の
  贈り物。プロジェクトの設計 tenets / Fork（判断・根拠・再検討条件、`CLAUDE.md` 参照）と矛盾しないか確認し、
  **どの Fork / Tenet を具体化・更新するか**を明記する。仕様変動領域は「確定事実 / 現時点の計画
  (projected)」を区別する。
- **Consequences** は Pros と Cons/Tradeoffs を分けて列挙。未解決の負債は Cons に書く。Fork の
  「再検討条件」に当たるものはここに残す。
- コードブロックは判断の根拠に必要な最小限。ロジックの羅列は git の diff で読める。
- **Related ADRs は wikilink `[[000NNN]] タイトル` 形式**で列挙（`ADR-000NNN (タイトル)` 形式は使わない）。
  `CLAUDE.md` への参照は通常リンクで良い。

### 2.5 許可タグ

```
architecture, fast-path, extension-plane, filter, chain, decision,
wit, component-model, wasi, wasm, capability, host-api, sandbox,
wasmtime, redb, quinn, openraft, foca, tonic,
http, tls, routing, load-balancing, upstream, rate-limit, waf,
security, observability, otel, oci, distribution, hot-reload, config,
performance, async, testing, ci-cd
```

この外のタグを増やしたくなったら、ADR ではなく先に `CLAUDE.md`（または本スキルの §2.5）を更新する。

### 2.6 情報衛生

Plecto はセルフホスト / データ主権志向の公開プロジェクト。以下を含めない:

- 本番 IP / 本番ドメイン / 秘匿ポート
- 資格情報・API キー・秘密鍵・cosign 鍵などのシークレット（パスと「秘密がある」事実に留める）
- 私的サーバー名

`localhost:XXXX` と設定上のサービス名は OK。

### 2.7 書き込み

Write ツールで `docs/ADR/NNNNNN.md` を作る。heredoc や `cat > ...` は使わない。書き込み後に Read で
自分の出力を読み返し、見出し / frontmatter / wikilink 形式を確認する。

最後にグラフ側を通す。CI（`docs` ジョブ）と同じ検証で、ここで落ちたものは PR でも落ちる:

```bash
docdag validate --touching docs/ADR/NNNNNN.md   # 書いた ADR とその隣接だけを報告（exit code は corpus 全体）
```

---

## §3. 完了報告

ユーザに以下を伝える:

- 書いた ADR のパス（`docs/ADR/NNNNNN.md`）とタイトル
- §1 を実行したなら、緑だったテスト
- commit するかどうかはユーザに確認する（このスキルは commit / push を勝手に行わない）

---

## 参照

- `docs/ADR/template.md` — セクションと frontmatter のソース
- `CLAUDE.md` — プロジェクト規約と設計 tenets の要約（Tenets / Fork 1–10）。ADR はその設計判断に従属し、
  Fork を具体化・更新する
- 軽量な ADR 判断基準は `grill-with-docs/ADR-FORMAT.md`（いつ ADR を書くべきか）
