# plecto binary — 用語集

`plecto` バイナリと operator CLI（validate / conformance / new-filter / dev / schema）を持つ
エンドユーザ入口のクレート。データプレーン・コントロールプレーン・wasmtime ホストの実体は
ライブラリ 3 クレート（`plecto-server` / `plecto-control` / `plecto-host`）にあり、本クレートは
その上の薄い CLI 層に徹する（`cargo install plecto` の一等導線）。全体像と他コンテキストとの
関係は [../../../CONTEXT-MAP.md](../../../CONTEXT-MAP.md)。

- **serve** — `plecto <manifest.toml> [listen_addr]`。manifest からコントロールプレーンを構築し
  fast path を起動する主経路（ADR 000013）。SIGHUP hot reload / SIGTERM graceful drain は
  ライブラリ側の実装をそのまま配線する（ADR 000008 / 000039）。
- **operator CLI** — `validate`（`nginx -t` 型の静的検証、ADR 000046）、`schema`（manifest の
  JSON Schema、ADR 000049）、`--version`（capability profile 表示、ADR 000079）、`healthz`
  （自己プローブ）、`package`（CI パッケージング）。
- **healthz（自己プローブ）** — `plecto healthz [--live] [--admin-addr] [<manifest>]`。distroless
  イメージ内から admin エンドポイントを 1 回 GET し、2xx なら exit 0・それ以外は 1（Docker が
  予約する 2 は返さない）。既定は readiness（`/readyz`）、`--live` で liveness（`/healthz`）。
- **package（ワンショットパッケージング）** — `plecto package <component> --key <pem> --out
  <dir>`。conformance ゲート → 運用鍵署名 → 署名済みローカル OCI image-layout 生成 → **digest
  のみ**を stdout へ。`dev` の内側ループから watch / dev 鍵 / manifest 書換を外した
  production/CI 形。`validate --resolve`（ローダの provenance ゲートを起動なしで実走）と対で
  「package → digest pin → resolve 検証」の CI パイプラインを閉じる。
- **Filter Dev Kit** — `conformance` / `new-filter` / `dev`（ADR 000065）。`new-filter` が書き出す
  WIT は `plecto_control::FILTER_WIT`（host の vendored 契約の re-export）で、guest テンプレートは
  このクレートの `templates/filter-template/` に vendoring（ADR 000072 / 000090）。
- **capability profile** — feature `outbound-http` / `outbound-tcp` / `fat-guest` /
  `capabilities` は下位クレートへの転送のみ。コンパイル時包含 ≠ 実行時グラント（ADR 000079）。
