# Postmortem template (Plecto)

この見出しと順序をそのまま使う。`<...>` を埋め、不要な補足コメント（`<!-- -->`）は消す。
blameless（主語は仕組み・条件、個人を責めない）・平易・事実ベースで書く。

## Contents
- 概要 / 影響 / タイムライン / 根本原因 / 検知 / 対応と復旧 /
  ふりかえり / 再発防止アクション / 再発確認 / 教訓 / 関連

---

```markdown
---
title: <一行サマリ（動詞含む。例: auth フィルタの trap が fail-open でリクエストを素通りさせた）>
date: <YYYY-MM-DD>
status: resolved        # resolved | monitoring | investigating
severity: <SEV1|SEV2|SEV3>
authors:
  - <name>
tags:
  - <例: filter, capability, fail-open, hot-reload>
---

# Postmortem: <タイトル>

## 概要

<2–3 文。何が起きたか・なぜか・影響と継続時間・現状（解決済みか）。>

## 影響

- **信頼境界（capability / sandbox）**: <破れの有無。サンドボックス脱出・テナント間漏洩が無いか。最優先で判定。>
- **データ経路（fast path / filter chain）**: <落ちた/素通りした/遅延した範囲。worker 巻き込みの有無。>
- **設定整合 / 状態**: <hot-reload / content-hash / consensus / host KV の整合と復旧可能性。>
- **利用者・対象**: <どのルート/フィルタ/期間が影響を受けたか。>
- **継続時間**: <発生〜復旧（JST）。不明なら「不明」。>

## タイムライン（JST）

<!-- ログが UTC なら JST に換算し、その旨を1行添える。 -->

- `YYYY-MM-DD HH:MM` <出来事（発生）>
- `YYYY-MM-DD HH:MM` <検知>
- `YYYY-MM-DD HH:MM` <原因特定>
- `YYYY-MM-DD HH:MM` <修正コミット <sha>>
- `YYYY-MM-DD HH:MM` <復旧確認>

## 根本原因

<Five Whys で寄与原因まで辿る。主語は仕組み・条件にする。
どの設計原則・Fork が想定していた前提が、どこで破れたかを Plecto の語彙で書く。>

1. なぜ <事象> が起きたか → <…>
2. なぜ <…> → <…>
3. （根本原因に至るまで）

**根本原因**: <一文で。>

## 検知

<どう気づいたか（ユーザ報告 / アラート / テスト / OTel）。検知が遅れた/取りこぼした
仕組み上のギャップがあれば書く（例: フィルタ trap を fail-open で握り潰し、メトリクスにも出なかった）。>

## 対応と復旧

<実施した修正（コミット <sha>）。なぜその層で直したか（信頼境界・fail-closed 原則に照らした判断）。
復旧手順（hot-reload / 設定ロールバック / host KV 修復等）。回避した代替案と却下理由があれば一行。>

## ふりかえり（blameless）

- **うまくいったこと**: <…>
- **まずかったこと（仕組みの話）**: <…>
- **幸運だったこと**: <…（例: サンドボックスは保たれ、漏洩は無かった）>

## 再発防止アクション

| 内容 | 担当 | 種別 | 状態 | 追跡 |
|---|---|---|---|---|
| <恒久対策の具体> | <name/TBD> | 恒久対策 | done/todo | <commit / issue> |
| <検知ギャップを埋める> | <name/TBD> | 改善 | todo | <…> |

## 再発確認

<同じ根本原因の別事象が無いか（他フィルタ・他ルート・他ノード）。確認結果を1–2行。>

## 教訓

<次に同種を防ぐための一般化。該当する設計原則（deny-by-default, stateless filter, fail-closed,
init/per-request 分離 等）や Fork を名前で引く。>

## 関連

- [[000NNN]] <関連 ADR>
- 修正コミット: `<sha>`
- `CLAUDE.md` <該当 Fork / Tenet>
```
