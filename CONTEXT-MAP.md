# Plecto — Context Map

Plecto は、**相補関係にある二つの構成要素**（native-Rust の fast path / WASM の extension plane）を
WIT 型契約で結ぶ、セルフホスト可能な L7 リバースプロキシ / API ゲートウェイ。用語集はコンテキストごとに分割し、
本ファイルはその地図——どのコンテキストがどこにあり、どう関係するか——と、全体に跨る語彙だけを持つ。実装詳細・
仕様・決定は置かない（設計判断は `CLAUDE.md` と `docs/ADR/`、契約は `wit/` を参照）。

## アーキテクチャ全体（cross-cutting）

**Fast path**:
接続受付・TLS 終端・HTTP/1.1/2/3・ルーティング・LB・upstream 管理を担う native-Rust 側の構成要素。
チェーンを駆動する側。
_Avoid_: core, engine（曖昧）, data plane（多義）

**Extension plane**:
各リクエストの判断（認証・書換・rate limit・WAF・ポリシー）を担う WASM フィルタの実行基盤。
fast path から WIT 契約越しに駆動される側。
_Avoid_: plugin layer, middleware layer

**Two halves（相補関係にある二つの構成要素）**:
fast path と extension plane の対。相補関係にあり、両者を WIT 型契約で結ぶ。
_Avoid_: 二つの半身（身体比喩で生硬・"two halves" の直訳調）, より合わせる糸（比喩過多）

## Contexts

- [Extension plane / host runtime](./plecto/crates/host/CONTEXT.md) — `plecto:filter` 契約・フィルタ実行モデル・
  能力境界（host-API）。wasmtime 埋め込みホスト（`plecto-host`）。
- [Control](./plecto/crates/control/CONTEXT.md) — 宣言的マニフェスト・無停止 reload・単一ノード／分散 opt-in・
  config version（`plecto-control`）。
- **Fast path** — 接続／TLS／HTTP／routing／LB／upstream。**未実装（クレート未作成）**。fast-path server に
  着手した時点で `plecto/crates/<fastpath>/CONTEXT.md` を起こす。

## Relationships

- **Fast path → Extension plane**: fast path が各リクエストを `plecto:filter` 契約越しに filter chain へ駆動する。
- **Control → Extension plane**: マニフェストが filter を OCI digest で pin し、chain 順と trust root を宣言する。
  reload が filter set + chain をアトミックに差し替える。trust root は構築時固定で、reload では変えない。
- **Control → Fast path**（将来）: route 照合・named chain は fast-path server 到来時に Control が司る。
