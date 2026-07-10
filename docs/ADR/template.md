---
title: <動詞始まりの行動指向の一文（ADR 番号は含めない）>
date: <YYYY-MM-DD>
status: proposed        # proposed | accepted | superseded
tags: [<許可タグから最大5>]
affected_components:
  - <component — 変更概要を1行>
aliases: ["ADR-NNN", "ADR-000NNN"]
# Optional graph edges (append-only history; do not rewrite Decision bodies in place):
# amends: ["000052"]       # this ADR refines / corrects an earlier decision
# supersedes: ["000037"]   # this ADR replaces an earlier decision entirely
# amends_tenets: ["P4"]     # when this ADR changes a design principle (rare; principles live in design-principles.md)
---

# <タイトル>

## Status
<proposed | accepted | superseded by [[000NNN]]>

## Date
<YYYY-MM-DD>

## Affected Components
- <component — 何がどう変わるか 1 行/件>

## Context
<なぜこの決定が必要だったか。定量/定性の根拠。計測があれば数値を残す。
仕様変動領域は「確定事実 / 現時点の計画(projected)」を区別する。
どの Tenet / Fork（判断・根拠・再検討条件）に関係するかを明記。>

## Decision
<採用した選択肢。そして **検討した代替案と却下理由**（後から読む人への最大の贈り物）。
どの Fork / Tenet（設計 tenets は `CLAUDE.md`）を具体化・更新するか。設計原則
（安全 × ポータビリティ × セルフホスト性 × 運用の単純さ を優先）と矛盾しないことを確認。>

## Consequences

### Pros
- <得られる利点>

### Cons / Tradeoffs
- <受け入れる代償・未解決の負債>
- <Fork の「再検討条件」に当たるもの（いつこの判断を見直すか）>

## Related ADRs
- [[000NNN]] <関連 ADR のタイトル>
- `CLAUDE.md` <該当 Fork / Tenet>
