---
name: bp-typescript
description: >-
  TypeScript / JavaScript ベストプラクティス。型安全性とコード品質を保つ規約。
  Plecto では (1) Node ツーリング・統合テスト・wasm-bindgen の node_pkg 周り、
  (2) jco / componentize-js で書く JS/TS 製 WASM フィルタ、の二用途に効く。
when_to_use: >-
  .ts / .mts / .js / .mjs ファイルを編集・作成する時、Node スクリプトや統合テストを書く時、
  JS/TS で WASM フィルタ（plecto:filter）を実装する時。テスト実行・読み取りのみの作業や
  Rust / Go / Python の作業時は不要。
paths:
  - "**/*.ts"
  - "**/*.mts"
  - "**/*.tsx"
  - "**/*.js"
  - "**/*.mjs"
---

# TypeScript / JS Best Practices (Plecto)

このスキルが発動したら、必要に応じて [reference/typescript.md](reference/typescript.md) を Read で
読み込み、記載されたベストプラクティス（DECREE）に従ってコードを書くこと。

Plecto は Rust が主役で、TS/JS は周辺だが二箇所で効く:
- **Node ツーリング層**: `node_pkg/`（wasm-bindgen 出力）の利用、`test_node.js` 系の統合テスト、ビルド/CI スクリプト。
- **JS/TS 製フィルタ**: `jco` の `componentize-js`（StarlingMonkey ベース）で JS を WASM コンポーネント化し、
  `plecto:filter` ワールドを実装する経路。WIT 契約は `wit-contract-design` スキル参照。

## 重要原則

1. **strict: true + noUncheckedIndexedAccess**: 必須設定。弱めない。`exactOptionalPropertyTypes` も推奨。
2. **境界では unknown**: 外部データ・リクエスト/レスポンス・ホスト KV から来る値は `unknown` で受け、
   型ガードで narrowing。`any` は最小限。
3. **型ガード > 型アサーション**: `as` より type predicate (`value is T`)。`!` 非 null アサーション禁止。
4. **satisfies でリテラル推論保持**: `Record<...>` 等で型チェックしつつリテラル型を維持。
5. **verbatimModuleSyntax**: `import type { T }` で型のみインポートを明示。WASM コンポーネント化では
   ESM が前提なので CJS 混在を避ける。
6. **判別共用体 + exhaustiveness**: tagged union（`decision` の `continue`/`modified`/`short-circuit` を
   ミラーする等）+ `satisfies never` で網羅性チェック。
7. **境界バリデーション**: 信頼できない入力は **Zod**（or valibot）でスキーマ検証。WASM フィルタ内では
   バンドルサイズに注意し、軽量バリデータ or 手書き型ガードを選ぶ。
8. **WASM フィルタの規律**: フィルタはステートレス（モジュールスコープに per-request 状態を持たない）。
   重い初期化（正規表現コンパイル・スキーマ構築）は初期化フックへ。ホスト機能はホストが貸した
   import 経由でのみ呼ぶ（deny-by-default）。

## 参照

完全なベストプラクティスは [reference/typescript.md](reference/typescript.md)。
セクション: Strict Configuration, Type Safety at Boundaries, Discriminated Unions, Error Handling,
Async Patterns, Validation, Module Design (ESM), Node tooling, JS→WASM filter authoring (jco)。
