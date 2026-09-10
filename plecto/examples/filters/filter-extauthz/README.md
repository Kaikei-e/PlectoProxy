# filter-extauthz

`filter-extauthz` は `wasi:http` を用いて外部の認可サービスへ問い合わせる、`plecto:filter` の参照実装です。
`wasm32-wasip2` 向けにビルドされ、ホスト側で `outbound-http` capability を明示的に貸す必要があります。

## 設定

認可先はクライアント要求から読まず、operator-owned な `[filter.config]` の `authz-url` で固定します。
未設定・空値・URL の解析失敗・発信失敗・2xx 以外の応答はすべて 403 です。

```toml
[filter.config]
authz-url = "https://authz.internal.example/check"
```

`outbound-http` の allowlist には同じ scheme / host / port を登録してください。allowlist と SSRF 防御は
このフィルタよりホスト側で強制されます。`x-authz-url` のような要求ヘッダで認可先を指定してはいけません。
