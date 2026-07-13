---
name: wasmtime-host
description: |
  Best practices for embedding wasmtime as Plecto's host: InstancePre + pooling allocator for fast
  per-worker instance reuse, epoch interruption + memory limits for metering, Linker-based
  deny-by-default host functions, Store-per-request lifecycle, async host calls, pooling
  zeroization (CVE-2022-39393), and OCI-artifact load + cosign signature verification.
when_to_use: >-
  Use when writing or reviewing the host runtime that instantiates/runs WASM filters,
  wiring the host-API into the Linker, configuring metering/limits, or loading filter
  components.
allowed-tools: Read, Glob, Grep, Write, Edit, Bash, WebSearch, WebFetch, Agent
argument-hint: "<host-runtime area to design/review>"
---

# wasmtime Host Embedding (Plecto)

ホスト本体は wasmtime を embed し、untrusted な WASM フィルタを**プロセス内で安全に・速く**実行する。
契約は `wit-contract-design`、Rust 全般は `bp-rust`、脅威レビューは `security-auditor` へ。
仕様変動が速いので、実装前に現行 wasmtime API を Web 確認する（`web-researcher`）。

## 原則（Fork 3 / 7 / 8 と Tenet 2 に直結）

1. **Engine 共有・InstancePre で事前型チェック.** `Engine` はプロセス共有。`Linker::instantiate_pre`
   で `InstancePre`（型チェック・import 解決を事前実施）を作り、worker thread ごとに保持する。残るは
   メモリ/テーブル割当てと start だけになり、インスタンス化が劇的に速くなる（公式報告で
   SpiderMonkey.wasm が約2ms→約5µs）。
2. **pooling allocator でプール再利用（Fork 3）.** `InstanceAllocationStrategy::Pooling` を使うと
   仮想メモリが事前構成され、同一モジュールの affine slot 再利用で deallocation が
   「linear memory を madvise でリセット」程度に縮む。trusted な自家製フィルタの最適点。
3. **deny-by-default な Linker（Tenet 2 / Fork 7）.** host 機能は `Linker` に**明示追加した import
   だけ**。任意 outbound HTTP・FS・socket は **足さない**。能力ごとに別関数で足し、付与は設定駆動に。
   フィルタは import できない能力には触れられない（sandbox が強制）。
4. **epoch interruption で計量（Fork 7）.** `Config::epoch_interruption(true)` + 各 `Store` に
   `set_epoch_deadline(n)` + 別タスク/タイマーが `Engine::increment_epoch()`。fuel より軽量
   （公式報告で SpiderMonkey.wasm の実行が fuel 比で約 2 倍速い）。データプレーンは決定性より低オーバーヘッドのデッドラインを
   要するので epoch を採る。fuel は決定性が要る検証用途のみ。
5. **メモリ/リソース上限.** `Store` に `ResourceLimiter` を付け linear memory・テーブル・インスタンス数の
   上限を強制。誤割当て・暴走を封じる。
6. **Store-per-request の寿命.** `Store<HostState>` にリクエスト固有の host state（lent capability の
   ハンドル、trace span、deadline）を持たせ、リクエスト境界で破棄。フィルタはステートレス（Fork 4）、
   状態は host KV(redb) 側。
7. **async host call.** WASI 0.3 native async に合わせ host 関数は async。`epoch_deadline_async_yield_and_update`
   で長時間 guest を executor に戻す。注意: epoch も fuel も「host 呼び出し内でブロックした WASM」は
   起こせない → 外部 I/O 待ちには別途タイムアウト機構を設計する。

## untrusted マルチテナント（Fork 3 再検討 / CVE-2022-39393）

- pooling allocator と copy-on-write heap image（memory-init-cow）が両方有効だと、linear memory slot
  再利用時に前インスタンスの初期ヒープが次から見えうる脆弱性（CVE-2022-39393 / RUSTSEC-2022-0075、
  Wasmtime 2.0.2 / 1.0.2 で修正済み）。**最新 wasmtime を使う**前提でも:
  - untrusted テナント・フィルタは **per-request 新規生成**に切り替える（trusted のプール再利用と分ける）。
  - pooling 利用時は **deallocation 時の linear memory ゼロ化**と memory-init-cow の挙動を明示検証する。
  - Store メモリ上限と epoch deadline を必ず併用する。
- 信頼レベルでロード経路を分岐する設計を host に持たせる（trusted=pooled / untrusted=per-request+zeroize）。

## ロードと provenance（Fork 8）

- フィルタは **OCI artifact** として配布（`application/wasm` layer、`...wasm.config.v0+json` config）。
  `wkg` で fetch/publish、`wkg.lock` 相当で content digest を固定。
- ロード時に **cosign 署名 / SBOM を検証**し、宣言的マニフェストの content hash と突き合わせてから
  instantiate する。検証失敗は fail-closed（ロードしない）。
- hot-reload は「新 `InstancePre` を並行生成 → アトミックに切替 → 旧インスタンスを drain」。

## 実装スケッチ（概念、API は実装時に確認）

```rust
let mut cfg = Config::new();
cfg.wasm_component_model(true);
cfg.epoch_interruption(true);
cfg.allocation_strategy(InstanceAllocationStrategy::Pooling(pooling_cfg)); // trusted path
let engine = Engine::new(&cfg)?;

let mut linker = Linker::<HostState>::new(&engine);
// deny-by-default: lend ONLY granted capabilities
host_kv::add_to_linker(&mut linker, |s| &mut s.kv)?;       // e.g. redb-backed
// host_log::add_to_linker(...); host_counter::add_to_linker(...);  // only if granted

let component = Component::from_binary(&engine, &verified_oci_bytes)?; // after cosign verify
let pre = linker.instantiate_pre(&component)?;             // per-worker, reused

// per request:
let mut store = Store::new(&engine, HostState::for_request(/* lent handles, span */));
store.set_epoch_deadline(REQUEST_EPOCH_DEADLINE);
store.limiter(|s| &mut s.limits);                          // memory/table/instance caps
let instance = pre.instantiate_async(&mut store).await?;
// call init once (per instance lifetime); on-request per request
```

## やってはいけない

- `Linker` に「とりあえず」WASI 全部や outbound network を足す（deny-by-default を崩す）。
- epoch / メモリ上限なしで untrusted を動かす。
- untrusted テナントにプール slot を zeroize せず再利用させる。
- 署名・hash 検証前に component を instantiate する。
- host 関数内でブロッキング I/O を無制限に待つ（epoch では起こせない、別タイムアウト必須）。
- `Engine`/`InstancePre` をリクエストごとに作り直す（プロセス/worker 共有が正しい）。

## Sources（取得日 2026-06、着手時に再確認）

| # | Title | URL | Tier |
|---|-------|-----|------|
| 1 | wasmtime — Config (epoch_interruption, allocation_strategy) | https://docs.wasmtime.dev/api/wasmtime/struct.Config.html | S |
| 2 | wasmtime — Fast Instantiation (InstancePre + pooling) | https://docs.wasmtime.dev/examples-fast-instantiation.html | S |
| 3 | wasmtime — Interrupting Execution (epoch vs fuel) | https://docs.wasmtime.dev/examples-interrupting-wasm.html | S |
| 3b | Wasmtime 1.0: Performance（2ms→5µs・epoch ≈2x vs fuel の数値の出典） | https://bytecodealliance.org/articles/wasmtime-10-performance | S |
| 4 | CVE-2022-39393 — pooling allocator data leakage | https://github.com/bytecodealliance/wasmtime/security/advisories/GHSA-wh6w-3828-g9qf | S |
| 5 | CNCF Wasm OCI Artifact layout / wkg | https://github.com/bytecodealliance/wasm-pkg-tools | S |
| 6 | Project design tenets — Fork 3/7/8 (see `CLAUDE.md`) | (in-repo) | S |
