# OWASP Top 10:2025 — Plecto sweep reference

最終版（2025年11月発表 / 2026年1月確定）。各カテゴリの判定基準と grep シグナルを Plecto 文脈で示す。
監査時は https://owasp.org/Top10/2025/ で最新を再確認する。

> 2025 の主な変化: **A03 Software Supply Chain Failures** と **A10 Mishandling of Exceptional
> Conditions** が新設。**SSRF は A01 Broken Access Control に吸収**された（Plecto は upstream を
> 構築するプロキシなので A01/SSRF は最重要）。

---

## A01:2025 Broken Access Control（SSRF を含む）
- IDOR、縦横の権限昇格、デフォルト allow、テナント境界欠落、管理 API(tonic) の認可欠落。
- **SSRF (CWE-918)**: upstream / サブリクエスト URL が入力・設定・**フィルタ出力**由来で組み立てられ、
  内部ネットワーク・クラウド metadata（`169.254.169.254`）・loopback・非公開ホストに到達しないか。
- grep: `grep -rn "Uri::\|reqwest::\|connect(\|upstream\|resolve(\|to_socket_addrs" --include='*.rs'`
- 対策: 宛先の allowlist / スキーム制限 / DNS rebinding 対策 / 内部レンジ拒否、フィルタが宛先を
  指定できる範囲を deny-by-default に。

## A02:2025 Security Misconfiguration
- debug モード残存、verbose error をクライアントへ、過度な CORS、デフォルト認証情報。
- **wasmtime/Linker の過剰設定**: 不要な WASI/host 機能を貸す、epoch/メモリ上限が無効、
  pooling のゼロ化未設定。
- grep: `grep -rn "RUST_LOG\|debug\|CORS\|Access-Control-Allow-Origin\|wasi::\|add_to_linker" .`

## A03:2025 Software Supply Chain Failures（新）
- 未固定依存、lockfile drift、typosquat、post-install script、CI で使うツールの汚染。
- **フィルタ配布**: OCI digest を固定しているか（wkg.lock 相当）、cosign 署名/SBOM をロード時検証するか。
- grep: `grep -rn "version\|git =\|path =" Cargo.toml; ` + `cargo audit` / `npm audit`。

## A04:2025 Injection
- SQL/NoSQL/OS command/template/**log injection**/**HTTP header/CRLF injection**。
- grep: `grep -rn "format!(.*SELECT\|Command::new\|process::Command\|\\\\r\\\\n\|set_header" --include='*.rs'`
- 対策: パラメータ化、ヘッダ値の検証（CRLF 除去）、ログは構造化フィールド（値を行に直挿ししない）。

## A05:2025 Insecure Design
- rate limiting / 過負荷制御 / 不可逆操作の確認・サイズ/タイムアウト上限が**設計段階で無い**。
- Plecto: slowloris / 巨大ボディ / 無限 stream / フィルタ無限ループ（epoch 無し）への設計上の備え。

## A06:2025 Cryptographic Failures
- 弱いアルゴリズム、自前 crypto、TLS 検証無効化、秘密のログ/エラー露出、弱い乱数（`rand` の予測可能 seed）。
- grep: `grep -rn "danger_accept_invalid_certs\|set_verify\|Md5\|Sha1\|ECB\|rand::random" .`

## A07:2025 Identification & Authentication Failures
- 弱いトークン、session fixation、MFA 欠落、タイミング非依存比較欠如（トークン比較は定数時間）。
- grep: `grep -rn "token\|session\|== .*secret\|api_key" --include='*.rs' --include='*.ts'`

## A08:2025 Software or Data Integrity Failures
- 未検証 deserialization、**無署名のフィルタ/アップデートのロード**、CI 汚染。
- Plecto: マニフェストの content hash と実体の突合、cosign 検証前の instantiate を禁止。

## A09:2025 Security Logging & Monitoring Failures
- 監査ログ欠落、機微情報（Authorization/cookie/トークン/鍵）のログ出し、ログ偽装可能。
- grep: `grep -rn "tracing::\|info!\|debug!\|println!\|console.log" . | grep -iE "token|authorization|secret|password|cookie"`

## A10:2025 Mishandling of Exceptional Conditions（新）
- fail-open、例外/エラーの握り潰し、**panic で worker を巻き込む**、フィルタ trap/deadline 時に
  fail-open でリクエストが素通り。
- grep: `grep -rn "unwrap()\|expect(\|let _ =\|catch {}\|\.ok();" --include='*.rs' --include='*.ts'`
- 対策: untrusted 入力で panic させない、trap → 明示的な fail-closed（or 設定された）decision にマップ。
