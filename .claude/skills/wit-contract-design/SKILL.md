---
name: wit-contract-design
description: >-
  Design and evolve Plecto's `plecto:filter` WIT world and host-API surface — the type
  contract between the native fast path and untrusted WASM filters. Covers
  worlds/interfaces, resources, the decision variant, minimal-capability host-API slicing,
  header bytes (list<u8>), versioning/compat with frozen v0.1/v0.2 load-time adapters, and
  the projected body/stream<u8> + wasm32-wasip2 migration.
when_to_use: >-
  Use when authoring or changing .wit files, designing the filter contract or host-API,
  or when the user mentions WIT / Component Model / world / 「契約を設計」「WIT を書く/直す」.
allowed-tools: Read, Glob, Grep, Write, Edit, WebSearch, WebFetch, Agent
argument-hint: "<contract or interface to design/change>"
---

# WIT Contract Design (Plecto)

`plecto:filter` は Plecto の中核契約 — fast path（host）と untrusted な WASM フィルタの間の型付き境界。
ここを変えると全フィルタとホストに波及するので、**contract-first**（先に WIT、それから実装）で進める。
仕様変動が速い領域なので、着手前に最新を Web 調査し、**確定事実 / projected を区別**する
（設計 tenets は `CLAUDE.md`、調査は `web-researcher` スキル）。

ラディカルに異なる形を比較したいときは `design-an-interface`、host 側の実装は `wasmtime-host`、
適合検証は `tdd-workflow` Phase 1（WIT-conformance）へ。

## 原則

1. **Contract-first.** 先に `.wit` を確定し、`wit-bindgen`（Rust/host）・`jco types`（JS）等で
   各言語のバインディングを生成してから実装する。host と全 component は **同じ WIT バージョン**を
   ターゲットにする（バージョン不一致はリンク不能/誤動作の元）。
2. **独自ワールド（Fork 2）— 現行は zero-WASI / header-only（ADR 000010）.** 現行契約は
   `plecto:filter@0.3.0`（正文は `wit/`。0.1 / 0.2 は `wit/v0.1.0/` / `wit/v0.2.0/` に凍結し
   ロード時アダプタで吸収、ADR 000071 / 000073）。`wasi:http` 型の再利用は body / stream 対応
   （wasm32-wasip2 移行後）の projected。`wasi:http/middleware`（コンポーネント＝完全な
   HTTP ハンドラ）は粒度が違うので採らない。
3. **判断は variant で（Tenet 3）.** 戻り値は `decision` variant: request 側 `continue` /
   `modified` / `short-circuit`、response 側 `continue` / `modified` / `replace`（ADR 000073）。
   ヘッダ値は原文バイト `list<u8>`（ADR 000071）。曖昧なフラグや暗黙の副作用にしない。将来の
   追加に備え host 側 match は網羅的に。
4. **host-API は deny-by-default・最小スライス（Tenet 2 / Fork 7）.** フィルタが import できる能力は
   「許可された request/response 読書き・KV/counter・metrics/trace・log・clock/random」程度に絞る。
   ネットワーク・FS・任意 socket は **貸さない**。能力ごとに別 interface に切り、付与を明示的にする。
5. **init と per-request を契約で分ける（Tenet 4）.** 高コスト初期化（regex/スキーマ）を置く init
   export と、ホットな per-request export を分離する。request 側・response 側は対称に。
6. **header-only と body-transform を契約レベルで分離（Fork 6）.** ボディに触れないフィルタは
   `stream<u8>` をゼロコピーでバイパスできるよう、契約で「ボディ非接触」を表現する。ボディ変換は
   `stream<u8>` で流しながら変換（stream splicing は 0.3.x 先送り、当面は中間コピー許容）。
7. **resource で所有権と寿命を表す.** request/response/body/host-handle は `resource` 化して
   借用・破棄を型で管理する。フィルタにメモリ実体を渡さない（capability 境界）。

## ワークフロー

### 1. スコープと前提を 1 段落で

何の契約を作る/変えるか、誰が consumer（フィルタ作者）か、どの能力境界に触れるか、互換性要件
（既存フィルタを壊すか）を明文化する。設計 tenets（`CLAUDE.md`）の該当 Fork と矛盾しないか確認。

### 2. 形を 2 回設計する

重要面（filter ワールド本体・host-API スライス）は `design-an-interface` で 3+ の radically different
案を出し、depth（小さい契約で大きな振る舞いを隠す）・誤用しにくさ（deny-by-default が易しい道か）・
実装効率（header-only fast path / zero-copy bypass を許すか）で比較する。

### 3. WIT を書く

- `wit/` に world / interface / types を置く。命名は `CONTEXT.md` 語彙に合わせる。
- `package plecto:filter@x.y.z;` で **semver を付ける**。破壊的変更は major、後方互換な追加は minor。
- 変更前に必ず現行の正文（`wit/`、`plecto:filter@0.3.0`）を Read する。
- 例（概念スケッチ — 現行契約そのものではない。実型は設計で確定する）:
  ```wit
  package plecto:filter@0.1.0;

  interface types {
    use wasi:http/types@0.3.0.{ /* request/response 型を再利用 */ };
    variant decision {
      continue,
      modified,            // 書換は host が拾える形で（resource 経由）
      short-circuit(response),
    }
  }

  interface host-kv {        // 一能力＝一 interface（deny-by-default で個別付与）
    get: func(key: string) -> option<list<u8>>;
    set: func(key: string, val: list<u8>, ttl-ms: option<u64>);
  }

  world filter {
    import host-kv;          // 付与された能力だけを import
    // import host-counter; import host-log; ...  ← 必要なものだけ
    export init: func();                 // 高コスト初期化（once）
    export on-request: func(/* req resource */) -> decision;   // hot
    export on-response: func(/* res resource */) -> decision;  // hot
  }
  ```

### 4. バインディング生成 → 適合テスト

`wit-bindgen`/`jco types` で型を出し、`tdd-workflow` Phase 1 の WIT-conformance（wasmtime に
component をロードして契約適合と振る舞いを検証）を更新する。host が要件を厳しくしたら、**全 in-tree
フィルタの conformance が新要件を pin して green** になるまで出さない。

### 5. 変更を記録

契約は hard-to-reverse なので、独自ワールド採用・能力スライス・semver bump 等は `plecto-adr-writer`
で ADR 化する（Fork 形式が綺麗にマップする）。

## wasm32-wasip2 → P3 移行（現時点の計画）

- 現行ビルドは **zero-WASI の `wasm32-unknown-unknown`**（ADR 000010、wit-component で component 化）。
  body / `stream<u8>` を入れる段階で `wasm32-wasip2` へ移行し、P3 WIT を見据える（`wasm32-wasip3` は
  rustc 正式ターゲット未満、ゲスト toolchain 対応進行中）。確定したら追従する。
- async バインディングの DX が実用に達しないなら、ヘッダ系フィルタを当面 **同期契約**に留める
  二段構えを許容（Fork 1 再検討条件）。契約にこの分岐余地を残す。
- Preview 3 timeframe で `wasi:http` に composable filter の標準ワールドが入り要件を満たすなら、
  独自ワールドをその上の薄い拡張へ縮小する（Fork 2 再検討条件）。

## やってはいけない

- host と component で WIT バージョンを食い違わせる。
- deny-by-default を崩して「とりあえず全部入りの host-API」を 1 interface で貸す。
- decision を bool / int フラグで表す。
- per-request export に重い初期化を混ぜる。
- 契約を変えて conformance テストを更新しない。
- 確定していない仕様（P3 の未確定機能）を「確定事実」として契約前提に固める。

## Sources（取得日 2026-06、着手時に再確認）

| # | Title | URL | Tier |
|---|-------|-----|------|
| 1 | Component Model — WIT, worlds, packages | https://component-model.bytecodealliance.org/ | S |
| 2 | wit-bindgen | https://github.com/bytecodealliance/wit-bindgen | S |
| 3 | wasi-http (worlds: service / middleware) | https://github.com/WebAssembly/wasi-http | S |
| 4 | jco (JS component tooling) | https://github.com/bytecodealliance/jco | S |
| 5 | Project design tenets — Fork 1/2/6 (see `CLAUDE.md`) | (in-repo) | S |
