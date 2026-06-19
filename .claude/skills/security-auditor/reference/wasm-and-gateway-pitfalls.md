# WASM sandbox / gateway / language pitfalls — Plecto reference

Plecto 固有の二軸（§A WASM サンドボックス・capability、§B L7 プロキシ/ゲートウェイ）と、§C 言語別
落とし穴（Rust / JS-WASM）。各項目に grep レシピを添える。確証が低いものは Info 扱いにする。

---

## §A. WASM sandbox / capability boundary

untrusted な WASM をプロセス内で動かす前提の監査。詳細は `wasmtime-host` スキルと設計 tenets（Fork 3/7、`CLAUDE.md`）。

- [ ] **deny-by-default の Linker** — フィルタに余計な host 機能を貸していないか。WASI 全部 import、
      outbound HTTP/FS/socket の付与を疑う。能力は最小スライスで個別付与か。
      `grep -rn "add_to_linker\|Linker::\|wasi::\|add_to_linker_get_host\|preview" --include='*.rs'`
- [ ] **計量と上限** — `epoch_interruption` + `set_epoch_deadline` が untrusted 実行に必ず効くか。
      `Store` の `ResourceLimiter`（linear memory / table / instance 上限）が設定されているか。無いと DoS。
      `grep -rn "epoch\|set_epoch_deadline\|increment_epoch\|limiter\|fuel\|ResourceLimiter" --include='*.rs'`
- [ ] **CVE-2022-39393（pooling 漏洩）** — pooling allocator + memory-init-cow で slot 再利用時に前
      インスタンスの初期ヒープが漏れうる。最新 wasmtime か、untrusted は per-request 生成か、
      deallocation 時に linear memory をゼロ化しているか。
      `grep -rn "Pooling\|InstanceAllocationStrategy\|memory_init_cow\|allocation_strategy" --include='*.rs'`
- [ ] **provenance / 署名** — フィルタ component を cosign 署名 / SBOM / content-hash 検証してから
      instantiate しているか。検証前ロードは A08。fail-closed か。
      `grep -rn "Component::from\|verify\|cosign\|sha256\|digest\|wkg" --include='*.rs'`
- [ ] **trap / deadline の扱い** — フィルタ trap・epoch 超過が fail-open（素通り）になっていないか。
      明示的な fail-closed（or 設定された）decision にマップされるか（A10）。
- [ ] **ホストコールのブロッキング** — host 関数内で無制限の外部 I/O を待つと epoch では起こせない。
      タイムアウト機構があるか。
- [ ] **境界のデータ検証** — フィルタが返す書換ヘッダ/ボディ/decision を untrusted として再検証
      （ヘッダ CRLF、サイズ、ステータス範囲）しているか。

## §B. L7 proxy / gateway

- [ ] **SSRF（A01 / CWE-918）** — upstream・サブリクエスト・ヘルスチェックの宛先が入力/設定/フィルタ
      出力由来で内部ネットワーク・metadata(`169.254.169.254`)・loopback・非公開ホストへ到達しないか。
      allowlist / スキーム制限 / 内部レンジ拒否 / DNS rebinding 対策。
      `grep -rn "Uri::\|connect(\|to_socket_addrs\|resolve\|reqwest::\|hyper::client" --include='*.rs'`
- [ ] **TLS 終端** — 証明書検証無効化、弱い ciphers/プロトコル、SNI/ALPN の扱い、秘密鍵保護。
      `grep -rn "danger_accept_invalid\|set_verify\|VERIFY_NONE\|rustls\|TlsAcceptor\|min_protocol" --include='*.rs'`
- [ ] **request smuggling / splitting (CWE-444)** — `Content-Length` と `Transfer-Encoding` の二重解釈、
      フィルタによるヘッダ書換が境界をまたいで密輸を生まないか。フレーミングは HTTP ライブラリに委ね、
      独自パースで二重解釈を作らない。
      `grep -rn "Transfer-Encoding\|Content-Length\|content_length\|chunked\|raw header" -i --include='*.rs'`
- [ ] **header injection / 正規化不整合** — クライアント/フィルタ由来ヘッダの CRLF・重複・大文字小文字
      の正規化が fast path と filter で食い違わないか（rate-limit/WAF bypass の温床）。
- [ ] **rate-limit / WAF bypass** — カウンタが正規化後の信頼できるキーで動くか。short-circuit を
      迂回する経路（別ルート、エンコーディング差、ヘッダ偽装）が無いか。グローバル制御は host-native か。
- [ ] **過負荷 / DoS** — 接続数・ヘッダ数/サイズ・ボディサイズ・アイドルタイムアウト・フィルタ実行時間に
      上限があるか（slowloris / 巨大ボディ / 無限 stream）。

## §C. Language pitfalls

### Rust（fast path / host）
- `unwrap()` / `expect()` / `panic!` / `unreachable!` / 添字 `[i]` パニック on untrusted input
  → worker 巻き込み（A10）。`grep -rn "unwrap()\|expect(\|panic!\|unreachable!\|\[0\]\|\.\.\]" --include='*.rs'`
- `unsafe {` ブロックの不変条件・`// SAFETY:` の有無。FFI/WASM 境界。`grep -rn "unsafe " --include='*.rs'`
- 生 SQL を `format!` で組む（使うなら）。`Command` への shell 経由実行・引数注入。
  `grep -rn "format!(.*SELECT\|Command::new\|sh -c\|shell" --include='*.rs'`
- 弱い乱数 `rand::random` を秘密生成に使用。整数オーバーフロー（release で wrap）。
- `.await` を跨ぐロック保持（デッドロック/性能）、`let _ =` での結果握り潰し（fail-open）。

### JavaScript / TS（tooling / WASM filter）
- `eval(` / `Function(` / 動的 import で untrusted を実行。`grep -rn "eval(\|new Function" --include='*.ts' --include='*.js'`
- prototype pollution（`__proto__` / `constructor` をキーに含む untrusted オブジェクトの merge）。
- `console.log` に秘密を出す（フィルタは host log import に寄せる）。
- フィルタ内のモジュールスコープ可変状態（per-request 状態漏れ・プール再利用衝突）。
- 巨大依存のバンドル（攻撃面・サイズ）。`JSON.parse` 結果を unknown で検証せず信頼。

---

## Sources（取得日 2026-06）
| # | Title | URL |
|---|---|---|
| 1 | OWASP Top 10:2025 | https://owasp.org/Top10/2025/ |
| 2 | OWASP ASVS 5.0 | https://asvs.dev/ |
| 3 | CWE-918 SSRF / CWE-444 Request Smuggling | https://cwe.mitre.org/ |
| 4 | CVE-2022-39393 (wasmtime pooling) | https://github.com/bytecodealliance/wasmtime/security/advisories/GHSA-wh6w-3828-g9qf |
| 5 | OWASP Secure Code Review Cheat Sheet | https://cheatsheetseries.owasp.org/cheatsheets/Secure_Code_Review_Cheat_Sheet.html |
