---
name: full-review
description: |
  Runs an adversarial full review of an AI-authored (or any) merge commit / PR / branch
  range: Builder/Critic session separation, Spec+Diff-only critic input, parallel critic
  lanes (correctness, security, tests/CI), evidence-gated findings, and a structured
  merge verdict. This skill should be used when the user asks to "full-review",
  "/full-review", "敵対的レビュー", "フルレビュー", "AI実装をレビュー", "マージコミットを
  レビュー", or to adversarially review a merge commit, PR, or AI-generated change set.
user-invocable: true
allowed-tools: Bash, Read, Glob, Grep, Agent, WebFetch, WebSearch
argument-hint: [<commit|PR|range>] [--lanes=correctness,security,tests] [--depth=standard|deep]
---

# Full Review (Adversarial)

AI 実装・大規模マージ差分向けの **敵対的フルレビュー**。自己検証バイアスを構造的に避け、
見た目の正しさではなく **前提・境界・負の証拠・スコープ外変更** を検証する。

規範の一次ソース（要約は `references/`）:

- OWASP AISVS Appendix C（人間レビュー必須・職務分離・クリティカル面の閾値引き上げ）
- OWASP Secure Coding with AI（out-of-scope edits / test fabrication / CI supply chain）
- ASDLC Adversarial Code Review（Builder ≠ Critic、Spec+Diff のみ、並列 Critic）

詳細チェックリストは [references/adversarial-checklist.md](references/adversarial-checklist.md)。
出力テンプレは [references/report-template.md](references/report-template.md)。
Plecto 固有のセキュリティ深掘りが必要なら `security-auditor` を **別レーン**で起動する
（本スキルが代わりにならない）。

## Hard rules

1. **Critic に Builder の推論を渡さない。** この会話で実装・説明した内容を「正しい前提」として
   Critic に載せない。Critic 入力は **Spec（ADR / 設計原則 / PR 本文の要求）+ Diff + 周辺コード**
   のみ。
2. **同一セッション自己レビュー禁止。** 実装に関与した文脈で「自分をレビュー」しない。並列
   `Agent`（readonly）を Critic レーンとして起動する。親エージェントは Moderator に徹する。
3. **説明は証拠にならない。** 「安全なはず」は棄却。finding はコード引用・実行結果・欠落テスト
   のいずれかで裏付ける。裏付け不可なら severity を下げ `Needs evidence` にする。
4. **決定論ゲートを先に。** lint / test / deny で機械が拾える問題に人間・LLM 時間を使わない。
5. **AI は人間レビューの代替にならない**（AISVS AC.4.1）。最終 verdict は人間オーナー向けの
   判断材料。マージ可否の決定権は人間。

## Progress checklist

```
Full Review Progress:
- [ ] Phase 0: Resolve target + Spec + blast radius
- [ ] Phase 1: Deterministic gates (fmt/clippy/test/deny as applicable)
- [ ] Phase 2: Spawn Critic lanes (parallel Agents, Spec+Diff only)
- [ ] Phase 3: Moderator synthesis (dedupe, evidence gate, severity)
- [ ] Phase 4: Emit report + merge recommendation
```

## Phase 0: Scope intake

明文化してから進む（1 段落で可）:

| 項目 | 内容 |
|---|---|
| **Target** | merge SHA / PR number / `base...head`。未指定なら最新 merge commit |
| **Spec** | PR 本文・関連 ADR・触ったコンテキストの `CONTEXT.md` / `design-principles` 該当節 |
| **Blast radius** | 変更ファイル一覧。CI / deny / rules / lockfile / auth / sandbox / TLS をハイライト |
| **Trust boundaries** | クライアント入力 / untrusted filter 出力 / upstream / 設定 / host-API |
| **Lanes** | 既定: `correctness`, `security`, `tests`。`--lanes` で絞る |

範囲解決:

```bash
# 最新 merge
git log --merges -1 --format='%H %P'
# PR の場合
gh pr view <N> --json baseRefName,headRefOid,mergeCommit,files,body,title
# 差分一覧
git diff --stat <base>...<head>
git diff --name-only <base>...<head>
```

便利スクリプト: `scripts/resolve-range.sh`（あれば実行して base/head を確定）。

**Security-critical surfaces**（該当ファイルが触られていたら security レーン必須・閾値引き上げ）:

- auth / TLS / STEK / client_auth / capability / pool / Linker / outbound
- `.github/workflows/**`, `deny.toml`, `Cargo.lock`, rules（`CLAUDE.md`, `.cursor/rules/**`）
- header / forward / smuggling 境界、fail-open 経路

## Phase 1: Deterministic gates

変更種別に応じてローカルで回す。失敗は finding（Critical/High）として記録し、Critic 前に報告。

| 変更 | 最低限 |
|---|---|
| Rust workspace | `cargo fmt --all -- --check` / `cargo clippy --all-targets --all-features -- -D warnings` / 影響クレートの `cargo test` |
| CI / deny | workflow YAML・`deny.toml` の diff を人手で読む（スクリプト実行は任意） |
| docs-only | Phase 1 スキップ可 |

`--depth=standard` では影響クレート中心。`--depth=deep` では `cargo test --all` + deny 相当。

## Phase 2: Critic lanes（並列）

各レーンを **別 `Agent`（readonly）** で起動。プロンプトには次のみを渡す:

- Spec 要約（PR 要求 / ADR 番号）
- `base...head` と `git diff --stat` / 重要ファイルパス
- そのレーン専用の検査観点（下記）
- 「PASS か、違反リストのみ。代替実装を書くな。証拠なしの主張をするな」

**渡してはいけないもの**: 実装チャットの経緯、Builder の自己正当化、「意図はこうだった」。

### Lane A — Correctness / contract

- 要求どおりか、隣接機能の過剰実装か
- エッジケース・順序依存・状態遷移の破綻
- ADR / WIT / fail-closed / panic-free data plane との矛盾
- 「動くが契約違反」（例: 全件ロード後フィルタ）を探す

### Lane B — Security / supply chain

- authz・信頼境界・fail-open・secret 露出
- CI/CD・deny・lockfile・依存追加のサプライチェーン
- WASM pool / capability / outbound の権限拡大
- 詳細は必要なら `security-auditor` の reference を読ませる（親がパスを指示）

### Lane C — Tests / CI honesty

- テスト削除・アサーション弱体化・実装ミラーテスト
- ネガティブテスト欠落（不正入力・認可失敗・trap/deadline）
- CI が「緑にする」方向に緩んでいないか（continue-on-error、除外拡大）

レーン指示の詳細文面: [references/adversarial-checklist.md](references/adversarial-checklist.md)。

## Phase 3: Moderator synthesis

親エージェントのみが行う:

1. レーン結果を読み、**重複統合**（同一根因は 1 finding）
2. **Evidence gate** — コード引用 or コマンド出力が無い finding は降格 or 破棄
3. **Severity** を正規化:

| Severity | 意味 |
|---|---|
| Critical | 本番悪用可能 / fail-open / 秘密漏洩 / CI 汚染。マージ前必須修正 |
| High | 契約・境界の実害。マージ前修正が強く推奨 |
| Medium | 実害は条件付き。追跡 issue 可 |
| Low | 保守性・明確化 |
| Needs evidence | 疑わしいが未検証 |

4. **Out-of-scope edits** を明示（PR 説明に無い CI/lockfile/rules 変更）
5. 矛盾するレーン意見は両方残し、どちらを採るかを一言で決める

## Phase 4: Report

[references/report-template.md](references/report-template.md) に従い、日本語で出力する
（識別子・パス・コマンドは英語のまま）。

必須セクション:

1. Verdict（`merge-ready` / `fix-then-merge` / `do-not-merge`）と一文理由
2. Scope（target SHA / Spec / blast radius）
3. Deterministic gates 結果
4. Findings（severity 順、証拠付き）
5. Test / CI honesty
6. Out-of-scope / surprise edits
7. Open questions（人間判断が必要な点）

修正実装は **レビュー報告後にユーザーが依頼したときだけ**行う。レビュー中に黙って直さない。

## Additional resources

- [references/adversarial-checklist.md](references/adversarial-checklist.md) — レーン別チェックと OWASP 対応
- [references/report-template.md](references/report-template.md) — レポート雛形
- `scripts/resolve-range.sh` — merge/PR から base...head を解決
