---
name: bp-rust
description: |
  Rust ベストプラクティス。Plecto の fast path（native Rust）と wasmtime ホスト埋め込みの
  コード品質を保つ規約とパターン集（Edition 2024）。
  TRIGGER when: .rs ファイルを編集・作成する時、Rust コードを書く時、listener / router /
  filter-chain dispatch / host-API / wasmtime 埋め込みなどの Rust コンポーネントを実装する時。
  DO NOT TRIGGER when: テストの実行のみ、Cargo.toml の確認のみ、ファイルの読み取りのみ、
  WASM フィルタを他言語（Go/JS/Python）で書く時。
---

# Rust Best Practices (Plecto)

このスキルが発動したら、必要に応じて [reference/rust.md](reference/rust.md) を Read で読み込み、
記載されたベストプラクティス（DECREE）に従ってコードを書くこと。本体は要点のみ、詳細は reference に置く。

Plecto は二つの半身を持つ: **fast path**（接続・TLS・HTTP・ルーティング・LB・upstream の native Rust）と、
**wasmtime ホスト**（untrusted な WASM フィルタを安全に実行する埋め込み）。両者でこの規約を適用する。
wasmtime 固有の埋め込み詳細は `wasmtime-host` スキルへ、WIT 契約は `wit-contract-design` スキルへ委譲する。

## 重要原則

1. **Edition 2024**: `edition = "2024"` 必須。`unsafe extern` ブロック、RPIT lifetime capture（`+ use<>`）、
   `gen` 予約語、`if let` 一時変数のドロップ順変更などの差異に注意。
2. **thiserror でドメインエラー / anyhow は端のみ**: ライブラリ層は `#[derive(thiserror::Error)]` の
   enum で型付きエラー（`#[from]` / `#[source]` でチェーン保持）。`anyhow` は `main.rs` / バイナリ
   エントリポイントの `Result<_, anyhow::Error>` と `.with_context(...)` に限定。
3. **データプレーンで panic 禁止**: hot path（リクエスト処理・フィルタ dispatch）で
   `unwrap()` / `expect()` / `panic!` / 添字アクセスのパニックを禁止。untrusted 入力には必ず `?` か
   明示的フォールバック。パニックは worker を巻き込んで可用性を壊す（プロキシは落ちてはいけない）。
4. **借用優先・無駄 clone 禁止**: `&str` > `String`、`&[T]` > `Vec<T>` を引数に。ホット経路の
   `.clone()` は禁止。ボディは可能な限りゼロコピー（`Bytes` / `&[u8]` / stream）で扱う。
5. **pub(crate) デフォルト**: 公開 API でないものは `pub(crate)`。クレート公開面（host-API trait など）
   は意図的に最小化する（capability 境界の一部）。
6. **tokio + tracing**: 非同期ランタイムは `tokio`、ログは `tracing`（`println!` / `eprintln!` 禁止）。
   観測は wasi-otel / OTel をホスト側で集約（`wasmtime-host` 参照）。`.await` を持つ critical section で
   ロックを跨がない。
7. **match 網羅性**: `_` ワイルドカードより明示的なバリアント列挙。`decision` variant
   （`continue` / `modified` / `short-circuit`）等の将来追加をコンパイル時に検出する。
8. **lint をソースに**: `cargo clippy --all-targets --all-features -- -D warnings` と
   `cargo fmt --all -- --check` を通す。手動スタイル議論はしない。`unsafe` は最小化し、使うなら
   `// SAFETY:` コメントと不変条件を必ず添える（`#![deny(unsafe_op_in_unsafe_fn)]`）。

## 参照

完全なベストプラクティスは [reference/rust.md](reference/rust.md)。
セクション: Edition 2024 Essentials, Project Structure, Error Handling, Ownership & Borrowing,
Async & tokio, Data-plane discipline (no panic / zero-copy), wasmtime host embedding pointers,
Testing, Logging & Observability, Lints & Tooling。
