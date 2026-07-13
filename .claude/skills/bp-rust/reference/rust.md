# Rust Best Practices — DECREE (Plecto, Edition 2024)

自己完結リファレンス。`bp-rust` スキルから参照される。外部ドキュメントに依存しない。
出典は末尾の Sources を参照（取得日 2026-06）。

対象は Plecto の **fast path**（native Rust の proxy データプレーン）と **wasmtime ホスト**。
WASM フィルタを他言語で書く作業は対象外。

## Contents

1. Edition 2024 Essentials
2. Project Structure
3. Error Handling
4. Ownership & Borrowing
5. Async & tokio
6. Data-plane discipline（プロキシ固有）
7. wasmtime host embedding（要点）
8. Logging & Observability
9. Testing
10. Lints & Tooling
- Sources

---

## 1. Edition 2024 Essentials

- `Cargo.toml` は `edition = "2024"`（現リポジトリは既にこれ）。MSRV を明記する。
- 主な差異と注意点:
  - **RPIT lifetime capture**: `-> impl Trait` が in-scope の型/ライフタイムを既定で捕捉する。
    捕捉を絞るときは `+ use<'a, T>` を明示。
  - **`unsafe extern` ブロック**: `extern` ブロックは `unsafe extern { ... }` で囲む。FFI / WASM
    インポートの宣言に効く。
  - **`gen` は予約語**。識別子に使わない。
  - **`if let` 一時変数のドロップ順**が変わった。ロックガード等を `if let` 条件に持つコードを見直す。
  - `Cargo` の `[lints]` テーブルでクレート横断の lint 設定を一元化できる。
- `cargo fix --edition` で移行差分を当てられるが、上記は手で確認する。

## 2. Project Structure

- **`main.rs` は薄く**: 設定読込 → 依存接続（wasmtime Engine, redb, listener bind）→ 配線 →
  サーバ起動 → signal 待ち（graceful shutdown / drain）まで。ビジネスロジックを置かない。
- `lib.rs` でモジュール宣言。fast path と host を別モジュール（例 `fastpath/`, `host/`, `filter/`,
  `control/`）に分け、`plecto-architecture` スキルのレイヤ対応に合わせる。
- 公開 API でないものは `pub(crate)`。クレート公開面は最小化（host-API trait は capability 境界）。
- ワークスペースを使う場合、host バイナリと共有型クレート（WIT 由来の型 / 設定型）を分ける。

## 3. Error Handling

- **ライブラリ層 = `thiserror`**: クレートごとに型付き enum を `#[derive(thiserror::Error)]` で定義。
  ```rust
  #[derive(Debug, thiserror::Error)]
  pub(crate) enum FilterError {
      #[error("filter `{name}` trapped")]
      Trap { name: String, #[source] source: wasmtime::Error },
      #[error("host-API `{0}` is not granted to this filter")]
      CapabilityDenied(&'static str),
      #[error("epoch deadline exceeded")]
      DeadlineExceeded,
  }
  ```
  `#[from]` で透過変換、`#[source]` で原因チェーンを保持。`Box<dyn Error>` を公開 API に出さない。
- **バイナリ端 = `anyhow`**: `main` / 起動経路だけ `anyhow::Result<T>` を返し、境界ごとに
  `.with_context(|| format!("binding listener on {addr}"))?` を付ける。
- **`?` を使い、握り潰さない**: `let _ = ...;` で結果を捨てるのは禁止（A10 fail-open の温床）。
  リトライ・フォールバックは明示的に書く。
- フィルタ実行のエラーは「どの decision にマップするか」を必ず決める（trap → fail-closed で
  `short-circuit 5xx` か、設定で fail-open か）。暗黙にしない。

## 4. Ownership & Borrowing

- 引数は借用優先: `&str` > `String`、`&[T]` > `Vec<T>`、`impl AsRef<[u8]>` を検討。
- ホット経路の `.clone()` を禁止。必要なら `Arc<T>` で共有（設定スナップショット・ルートテーブル等は
  `Arc<Config>` を `arc-swap` 的にアトミック差し替え）。
- ボディは可能な限りゼロコピー: `bytes::Bytes` / `&[u8]` / `Stream` を引き回し、`Vec<u8>` への
  full materialize を避ける（body-untouching フィルタは stream をバイパス）。
- 文字列 → 数値などの境界変換は一度だけ行い、内部では型付きで持つ（newtype: `RouteId(u32)` 等）。

## 5. Async & tokio

- 非同期ランタイムは `tokio`（multi-thread）。worker thread ごとに wasmtime インスタンスをプール
  （`wasmtime-host` 参照）。
- **`.await` をまたいでロックを保持しない**。`std::sync::Mutex` のガードを await 越しに持つと
  デッドロック/性能劣化。共有可変状態は `tokio::sync` か lock-free 構造、または host KV(redb) に逃がす。
- CPU バウンドな処理（正規表現コンパイル等）は `spawn_blocking` か **初期化フックに追い出す**
  （Tenet 4: init と per-request の分離）。
- graceful shutdown: signal を受けたら新規受付を止め、in-flight を drain してから終了。
  hot-reload も「新インスタンス並行生成 → アトミック切替 → 旧 drain」。
- キャンセル安全性: `select!` の分岐で途中状態を壊さないよう、副作用は確定後に行う。

## 6. Data-plane discipline（プロキシ固有）

- **panic 禁止 in hot path**: `unwrap`/`expect`/`panic!`/`unreachable!`/添字パニック/`slice[a..b]`
  の境界外/`integer overflow`（debug）を hot path から排除。プロキシは1リクエストの不正で全体を
  落としてはならない。`get(i)` + `?`、`checked_*`、`try_into()` を使う。
- **untrusted 入力の前提**: クライアント由来のヘッダ・ボディ・URL、そして **フィルタの出力**も
  untrusted。長さ上限・タイムアウト・サイズ上限を必ず設ける（slowloris / 巨大ボディ / 無限 stream）。
- **リソース上限**: コネクション数・ヘッダ数/サイズ・ボディサイズ・フィルタ実行時間（epoch）・
  メモリ（Store limit）に上限を持つ。上限超過は明示的エラーにする。
- request smuggling 対策: `Content-Length` / `Transfer-Encoding` の整合は HTTP ライブラリ（hyper/quinn 系）に
  委ね、独自パースで二重解釈を作らない。

## 7. wasmtime host embedding（要点・詳細は wasmtime-host）

- `Engine` はプロセスで共有、`Linker` で host-API を **deny-by-default** に構成（明示 import のみ）。
- `Component` を `Linker::instantiate_pre` で `InstancePre` 化（型チェック・import 解決を事前実施）。
- `Config::epoch_interruption(true)` + `Store::set_epoch_deadline` + 別タスクの `Engine::increment_epoch`
  で実行時間を計量（fuel より軽量）。`Store` の `limiter` でメモリ上限。
- pooling allocator を使う場合、untrusted テナントには linear memory のゼロ化（CVE-2022-39393 教訓）と
  per-request 新規生成を組み合わせる。

## 8. Logging & Observability

- `tracing` を使う。`println!`/`eprintln!`/`log` クレート直叩きは禁止。
  `tracing::info!(route = %route_id, decision = ?d, "dispatched")` のように構造化フィールドで出す。
- 秘密（トークン・鍵・Authorization ヘッダ値・cookie）をログに出さない。マスクする。
- スパンはリクエスト境界・フィルタ境界に張り、`trace_id` を伝播（ホストが span state を管理）。
- メトリクスはホスト集約。フィルタからの telemetry はホストへバッファするだけにする。

## 9. Testing

- ユニットはテーブル駆動（[../../tdd-workflow/templates/rust_unit_test.rs.tmpl](../../tdd-workflow/templates/rust_unit_test.rs.tmpl)）。
  success / error / edge を 1 つの `Vec<Case>` に並べ `assert_eq!(got, want, "case: {}", name)`。
- I/O / async は `#[tokio::test]`。フィルタ実行は in-process wasmtime インスタンス + 手書きの
  conformance フィルタ（fixture）で WIT 契約適合を検証（`tdd-workflow` の CDC 相当）。
- 振る舞いをテストし、実装詳細をテストしない。シンボル存在だけを確認するテストは書かない。
- `cargo test --all`。決定性が要る箇所は時刻/乱数を注入可能にする。

## 10. Lints & Tooling

クレートルートまたは `Cargo.toml [lints.rust]/[lints.clippy]` に:

```rust
#![warn(clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![deny(unsafe_op_in_unsafe_fn)]
// hot path クレートでは追加で:
#![warn(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
```

CI / Phase 5 ローカルパリティ:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
cargo test --all
# 任意（あれば）: cargo audit / cargo deny
```

`pedantic`/`nursery` は誤検知もあるので、抑制する場合は `#[allow(clippy::...)]` を**局所**に付け、
理由をコメントする。クレート全体での無効化はしない。

---

## Sources（取得日 2026-06）

| # | Title | URL | Tier |
|---|-------|-----|------|
| 1 | Effective Rust — Item 4: Prefer idiomatic Error types | https://effective-rust.com/errors.html | S |
| 2 | thiserror crate docs | https://docs.rs/thiserror | S |
| 3 | anyhow crate docs | https://docs.rs/anyhow | S |
| 4 | Rust Edition Guide — 2024 | https://doc.rust-lang.org/edition-guide/rust-2024/index.html | S |
| 5 | Clippy lint list (pedantic/nursery) | https://rust-lang.github.io/rust-clippy/ | S |
| 6 | wasmtime — Config (epoch_interruption) | https://docs.wasmtime.dev/api/wasmtime/struct.Config.html | S |
| 7 | How to Design Error Types with thiserror and anyhow (2026-01) | https://oneuptime.com/blog/post/2026-01-25-error-types-thiserror-anyhow-rust/view | A |
