---
name: plecto-postmortem-writer
description: Writes a blameless incident postmortem for the Plecto project in Japanese, grounded in the real timeline, commits, and logs (never speculation). Use when the user says "ポストモーテム書いて" / "障害報告書いて" / "事後検証" / "incident report" / "RCA" / "根本原因", or after an incident, outage, sandbox/capability scare, or a committed bug fix worth a durable record. Authoring only — no deploy.
allowed-tools: Bash, Read, Glob, Grep, Edit, Write
---

# Plecto Postmortem Writer

Plecto のインシデント・ポストモーテムを **日本語で・blameless に・事実に基づいて**執筆するスキル。
デプロイやインフラ操作は扱わない（執筆のみ）。

**Blameless の原則**（Google SRE / Atlassian）: 個人やチームを責めず、**寄与原因（contributing
causes）**に集中する。関係者は手元の情報で善意に基づき最善を尽くしたと仮定する。ポストモーテムは
罰ではなく学習の記録。

実行は 3 ステップ。途中はこのチェックリストを応答にコピーして埋める。

```
Postmortem Progress:
- [ ] 1. 事実確認（git / ログ / コードから timeline と根本原因を裏取り）
- [ ] 2. Plecto レンズで影響を分類（信頼境界 / データ経路 / 設定整合 / 状態）
- [ ] 3. テンプレに沿って docs/postmortem/ に執筆
```

---

## §1. 事実確認（先に裏取り。推測で書かない）

ポストモーテムは「実際に起きたこと」の記録。書く前に一次情報から timeline・影響・根本原因・修正を
確定する。**確認できない点は「不明」と明記**し、創作しない。

| 知りたいこと | 取り方 |
|---|---|
| 修正コミット・関連変更 | `git log --oneline`, `git show --stat <sha>`, `git log -S<symbol>` |
| エラー・再現 | ユーザ提供のログ、`cargo test` / 手元のプロキシ実行出力、tracing/OTel のスパン |
| 影響範囲のコード | `Grep` / `Read` で該当の fast path / host runtime / host-API / filter / 設定を確認 |
| 設計上の位置づけ | 関連 ADR（`docs/ADR/`）、`CONTEXT.md`、`CLAUDE.md`（設計 tenets / 該当 Fork） |

本番への直接操作・副作用は**このスキルでは行わない**。必要なら手順をユーザに渡す。

---

## §2. Plecto レンズで影響を分類

影響評価は必ずこの軸で書く。Plecto は untrusted コードを in-process で動かす L7 プロキシなので、
**信頼境界の破れ**が最重大になる。

- **信頼境界（capability / sandbox）**: フィルタが貸されていない能力に到達した・サンドボックスを
  脱出した・テナント間で状態が漏れた（CVE-2022-39393 型）か。**破れていれば最重大**。
- **データ経路の可用性（fast path / filter chain）**: リクエストが落ちた/素通りした（fail-open）/
  遅延した範囲と継続時間。プロキシ worker が panic で巻き込まれたか。
- **設定整合（manifest / content-hash / consensus）**: hot-reload のアトミック切替や drain が壊れたか、
  content hash と実体がズレたか、（分散時）openraft/foca の合意がズレたか。
- **状態（host KV / redb）**: レート制限/セッション/キャッシュ状態の破損・不整合と、復旧可能性。
- **設計原則との関係**: deny-by-default、stateless filter、single-node-first、fail-closed。
  違反やニアミスがあれば明記。

### Severity（基準は「信頼境界が破れたか / データ経路が止まったか」）

| SEV | 目安 |
|---|---|
| SEV1 | サンドボックス脱出・テナント間漏洩など**信頼境界の破れ**、またはデータ経路の全停止（プロキシ全断 / 全リクエスト失敗 / 認証 fail-open で素通り）。要即時 |
| SEV2 | 部分劣化（一部ルート/フィルタ停止、性能劣化、hot-reload 失敗で旧設定固定、設定合意のズレ、host KV 不整合）。復旧可能 |
| SEV3 | 軽微・一過性・自動復旧。記録はするが影響小 |

---

## §3. 執筆

### 3.1 ファイル

`docs/postmortem/YYYY-MM-DD-<slug>.md`（無ければディレクトリごと作る）。`<slug>` は英小文字・ハイフン
（例: `filter-trap-fail-open-on-auth`）。日付は当日（YYYY-MM-DD）。

### 3.2 テンプレート

[references/template.md](references/template.md) の見出しと順序をそのまま使う（勝手に増減しない）。
Five Whys で根本原因まで辿り、action item には **担当・状態・種別（恒久対策 / 改善）・追跡先** を必ず付ける。

### 3.3 書き方の規律

- **Blameless**: 主語を人でなく**仕組み・条件**にする（「X が Y を許していた」）。個人名・属人的な非難を書かない。
- **平易な語**: 誇張・芝居がかった語を避け、淡々と書く。
- **事実と推測を分ける**: 確証は断定、推測は「推測」と明記、不明は「不明」。
- **時刻は JST**（ログが UTC なら換算して JST と明記）。
- **再発確認**: 同じ根本原因の別事象が無いか（他フィルタ・他ルート・他ノード）を 1 行で。

---

## §4. 情報衛生

Plecto はセルフホスト / データ主権志向の公開プロジェクト。ポストモーテムにも以下を**含めない**:

- 本番 IP / 本番ドメイン / 秘匿ポート / 資格情報・鍵・cosign 鍵・シークレット（`localhost:PORT` は可）
- 私的サーバー名 / 個人への非難（blameless）

---

## §5. 完了報告

ユーザに次を伝える:

- 書いたパス（`docs/postmortem/YYYY-MM-DD-<slug>.md`）と一行サマリ・Severity
- §1 で裏取りに使った事実（コミット sha・ログ・該当ファイル）
- action item の一覧（担当が未定なら「TBD」と明示）
- commit するかはユーザに確認する（このスキルは commit / push を勝手に行わない）

---

## 参照

- [references/template.md](references/template.md) — ポストモーテムの正規テンプレート（見出し順のソース）
- `docs/ADR/` — 関連 ADR（wikilink `[[000NNN]]` 形式で引く）
- `CLAUDE.md` — 信頼境界・設計原則の要約（Tenets / Fork 1–10）
