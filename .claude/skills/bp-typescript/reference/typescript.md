# TypeScript / JavaScript Best Practices — DECREE (Plecto)

自己完結リファレンス。`bp-typescript` スキルから参照される。出典は末尾（取得日 2026-06）。

二用途: **Node ツーリング/統合テスト** と **JS/TS 製 WASM フィルタ（jco / componentize-js）**。

---

## 1. Strict Configuration

`tsconfig.json` の最低ライン（緩めない）:

```jsonc
{
  "compilerOptions": {
    "strict": true,
    "noUncheckedIndexedAccess": true,
    "exactOptionalPropertyTypes": true,
    "verbatimModuleSyntax": true,
    "module": "ESNext",
    "moduleResolution": "Bundler",
    "target": "ES2023",
    "noEmit": true,            // 型チェック専用。出力は tsc 以外（esbuild/jco）に任せる
    "skipLibCheck": true
  }
}
```

- WASM コンポーネント化は **ESM 前提**。`"type": "module"` を `package.json` に設定し、CJS を混ぜない。

## 2. Type Safety at Boundaries

- 外部から来る値（HTTP body/headers、host KV の戻り、JSON.parse 結果）は **`unknown`** で受ける。
- **型ガード（type predicate）優先**:
  ```ts
  function isAuthHeader(v: unknown): v is { scheme: string; token: string } {
    return typeof v === "object" && v !== null
      && "scheme" in v && typeof (v as Record<string, unknown>).scheme === "string"
      && "token" in v && typeof (v as Record<string, unknown>).token === "string";
  }
  ```
- `as`（型アサーション）と `!`（非 null アサーション）を避ける。どうしても要るなら局所＋コメント。
- `satisfies` でリテラル推論を保ちつつ型を満たす:
  ```ts
  const ROUTES = { api: "/v1", health: "/healthz" } satisfies Record<string, `/${string}`>;
  ```

## 3. Discriminated Unions & Exhaustiveness

- フィルタの判断はホスト側 `decision` variant をミラーする tagged union で表現:
  ```ts
  type Decision =
    | { tag: "continue" }
    | { tag: "modified"; headers: Header[] }
    | { tag: "short-circuit"; status: number; body?: Uint8Array };

  function handle(d: Decision): void {
    switch (d.tag) {
      case "continue": return;
      case "modified": return applyHeaders(d.headers);
      case "short-circuit": return respond(d.status, d.body);
      default: { const _exhaustive: never = d; return _exhaustive; }
    }
  }
  ```

## 4. Error Handling

- `throw` するのは `Error` のサブクラスのみ（文字列を throw しない）。原因は `cause` で連結
  （`new FilterError("denied", { cause: err })`）。
- 想定済みの失敗は例外でなく結果型（`Result<T, E>` 風 union）で表現してもよい。境界で一貫させる。
- WASM フィルタ内の未捕捉例外は trap になりホスト側で fail-closed 判断に落ちる。**意図した失敗は
  例外でなく `short-circuit` decision で返す**。

## 5. Async Patterns

- `async`/`await` を使い、Promise を浮かせない（`no-floating-promises`）。
- 並行は `Promise.all` / `Promise.allSettled`。逐次が必要な箇所を `for await` で明示。
- Node 側はタイムアウト・AbortController を付ける（外部 fetch、子プロセス）。

## 6. Validation

- 信頼できない入力は **Zod**（or valibot）でスキーマ検証し、型とバリデーションを一元化:
  ```ts
  import { z } from "zod";
  const Config = z.object({ rate: z.number().int().positive(), header: z.string() });
  type Config = z.infer<typeof Config>;
  const cfg = Config.parse(rawUnknown); // 失敗は例外 → 初期化フックで弾く
  ```
- **WASM フィルタ内ではバンドルサイズに注意**。Zod は重い。per-request パスでは初期化済みスキーマを
  使い回すか、軽量な手書き型ガードに切り替える。重い構築は初期化フックで一度だけ。

## 7. Module Design (ESM)

- `import type { T } from "..."` で型専用インポートを明示（`verbatimModuleSyntax`）。
- 公開面を小さく。バレル（index.ts 再エクスポート）はツリーシェイク阻害になりうるので濫用しない。
- 副作用のあるトップレベルコードを避ける（フィルタは初期化フックで明示初期化）。

## 8. Node tooling / 統合テスト

- 統合テスト（`test_node.js` 系）は wasm-bindgen 出力（`node_pkg/`）をロードして実際の値で検証する。
  単体は Vitest（[../../tdd-workflow/templates/typescript_component_test.ts.tmpl](../../tdd-workflow/templates/typescript_component_test.ts.tmpl)）。
- ハードコードした絶対パス・ポートを避け、環境変数でパラメータ化。
- CI/ローカルパリティ（`tdd-workflow` Phase 5）:
  ```bash
  npm ci            # or pnpm install --frozen-lockfile
  npx tsc --noEmit  # 型チェック
  npx biome check . # or eslint（採用したリンタ）
  node --test       # or npx vitest run
  ```

## 9. JS→WASM filter authoring (jco / componentize-js)

- **contract-first**: 先に `plecto:filter` の WIT を確定し、`jco types`（or `wit-bindgen`）で
  TS 型を生成してから実装する。ホストと **同じ WIT バージョン**をターゲットにする。
- `jco componentize`（StarlingMonkey ベース）で JS を WASM コンポーネント化。エクスポートは WIT の
  `export` 関数（init / per-request フック）に対応させる。
- フィルタは**ステートレス**: モジュールスコープに per-request の可変状態を置かない（プール再利用・
  hot-reload と衝突し、状態漏洩リスク）。状態はホスト KV（import 経由）に置く。
- ホスト機能（KV/counter/metrics/log/clock/random）は **import された関数経由でのみ**呼ぶ。
  ネットワーク・FS・時計などを勝手に使わない（deny-by-default、sandbox が強制）。
- バンドル/サイズと起動コストを意識。重い初期化は init フックへ。`console.log` はホストの log import に
  寄せる（生の stdout は環境次第で落ちる）。

---

## Sources（取得日 2026-06）

| # | Title | URL | Tier |
|---|-------|-----|------|
| 1 | TypeScript Handbook — tsconfig strictness | https://www.typescriptlang.org/tsconfig/ | S |
| 2 | jco (JavaScript Component Tooling) | https://github.com/bytecodealliance/jco | S |
| 3 | ComponentizeJS / StarlingMonkey | https://github.com/bytecodealliance/ComponentizeJS | S |
| 4 | Zod docs | https://zod.dev | S |
| 5 | Component Model — WIT & worlds | https://component-model.bytecodealliance.org/ | S |
