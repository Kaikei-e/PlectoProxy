# いま gateway ができること

[English](features.md)

Plecto Proxy に**実装済みかつ CI green** なものを、関心事ごとに並べたスナップショット。各行は決定 ADR に
リンクしているので、ここの主張は常に「それを生んだ判断」と「再検討条件」から 1 クリックの距離にある。
[README](../README.ja.md) は 1 段落の要約だけを持ち、本ページがその裏付け。まだ*無い*ものは
[ロードマップ](ROADMAP.md)（英語）。

| 関心事 | いま |
| --- | --- |
| **Edge & HTTP** | HTTP/1.1・HTTP/2（ALPN）・HTTP/3（QUIC、Alt-Svc 広告）。TLS 終端 ＋ SNI 証明書選択、manifest 宣言、fail-closed。**aws-lc-rs** に一本化した crypto provider の上で post-quantum の X25519MLKEM768 を既定優先、**stateless な TLS 1.3 session resumption**（ticket 鍵ローテーション、0-RTT 拒否）。**両方向の mTLS** —— `[listen.client_auth]` は終端する全ハンドシェイク（h1/h2/h3 同様）に検証済みクライアント証明書を要求し、`[upstream.tls]` は health probe を含む全 upstream 脚にクライアント identity を提示する。前段 L4 LB 配下向けの opt-in **PROXY protocol v2** 受信（trusted CIDR 必須・fail-closed） — [ADR 13–16](ADR/000013.md) · [51](ADR/000051.md) · [52](ADR/000052.md) · [57](ADR/000057.md) · [62](ADR/000062.md) · [78](ADR/000078.md) |
| **Inbound admission control** | 全体の接続上限 **＋** 送信元 IP 単位の上限を両 accept loop で強制（TCP は PROXY v2 解決後——前段 LB 自身が上限対象にならないように——QUIC は peer アドレス。IPv6 は /64 に丸め）。固定サイズのテーブルで支えるので、送信元を詐称した flood でもテーブルは成長しない。ヘッダ読取 timeout・body 読取 deadline・body buffer 予算。起動時に `RLIMIT_NOFILE` を soft→hard へ引き上げ、hard 天井が接続上限に対して不足していれば警告 — [27](ADR/000027.md) · [92](ADR/000092.md) |
| **Routing & upgrade** | host / path-prefix / method / header / query の照合を **specificity 順**で解決。weighted **traffic split / canary**。ingress の path 正規化を fail-closed な認証境界に（encode されたセパレータや dot-escape を拒否するので、route 単位のフィルタが信頼できる認証境界になる）。per-route の **HTTP/1.1 `Upgrade`** トンネリングで WebSocket（`h2c` は validation で拒否） — [27](ADR/000027.md) · [34](ADR/000034.md) · [48](ADR/000048.md) |
| **Response 圧縮** | per-route **`[route.compression]`** の opt-in（deny-by-default）: RFC 9110 の `Accept-Encoding` negotiation（gzip / br / zstd）、content-type allowlist、`no-transform` / 206 / HEAD の skip、`Vary` ＋ 弱い `ETag`、response フィルタチェーンの後段で適用 — [74](ADR/000074.md) · [75](ADR/000075.md)。**レスポンス body に secret を反射する route では有効にしないこと**（リクエスト由来の CSRF token・session nonce など）。圧縮 ＋ 反射は TLS に対する [BREACH](https://breachattack.com/) 級の攻撃面になる。該当 route はブロックを書かない。 |
| **Load balancing & upstream** | per-upstream の **round-robin**（既定）・**weighted least-request**（P2C）・**weighted Maglev** consistent hashing。active ＋ passive health check（悲観的スタート、全 unhealthy は fail-closed 503）、outlier detection、per-upstream circuit breaker、二段（per-try ＋ overall）timeout（route 側で上書き可）、有界かつ jittered な retry。per-upstream の **TLS+ALPN 再暗号化**（gRPC 対応——`TE: trailers` は透過——IP リテラルや DNS 展開先向けに検証名を固定する **`sni`** override 付き）と **定期 DNS 再解決**により、hostname upstream はコンテナの再作成に追従する — [17](ADR/000017.md) · [28](ADR/000028.md) · [30–32](ADR/000030.md) · [35](ADR/000035.md) · [42](ADR/000042.md) · [44](ADR/000044.md) · [50](ADR/000050.md) · [102](ADR/000102.md) |
| **Rate limiting** | **二層モデル**（[61](ADR/000061.md)）: native L7 token-bucket の **local floor**（**route** 単位 / **client-IP** 単位、node-local でラウンドトリップ前にバーストを吸収）＋ [`filter-ratelimit-redis`](../plecto/examples/filters/filter-ratelimit-redis)（RESP 互換ストアを outbound-TCP capability 経由で叩く **global** な reference filter）。併用が推奨形——サイジングの式と reference filter の demo-only 注記は [hardening ガイド](hardening.ja.md) — [33](ADR/000033.md) · [53](ADR/000053.md) · [60](ADR/000060.md) · [66](ADR/000066.md) · [81](ADR/000081.md) |
| **Extension plane** | `plecto:filter` chain をヘッダと、opt-in したフィルタには buffer 済みリクエスト body にも回す（header-only なフィルタは buffer 自体を飛ばす＝zero-copy）。型付き `decision`。trusted **pooled** / untrusted **fresh-per-request**。per-filter ＋ host-wide quota で縛られた deny-by-default の host-API。feature-gated の **outbound HTTP** ＋ **outbound TCP**（いずれも SSRF ガード・IP pin 付き）。feature-gated（既定 off）の **fat-guest** 最小 WASI 貸与により、zero-WASI という既定を広げずに Go/TinyGo フィルタを解禁。manifest 宣言の業務設定を貸す `host-config` capability——**`[filter.config_files]`** はロード / リロード時に値をファイルから解決するので、シークレットは manifest への直書きではなく mount で届く（キー衝突・欠落・非 UTF-8・1 MiB 超は fail-closed） — [1](ADR/000001.md) · [25](ADR/000025.md) · [36](ADR/000036.md) · [38](ADR/000038.md) · [60](ADR/000060.md) · [63](ADR/000063.md) · [66](ADR/000066.md) · [95](ADR/000095.md) |
| **Client IP** | edge モデル伝播——chain 実行の前に実 peer から `X-Forwarded-For` / `X-Real-IP` を付け直すので、クライアント申告のヘッダでフィルタを騙せない — [18](ADR/000018.md) · [22](ADR/000022.md) |
| **可観測性** | host が伝播する W3C trace context（inbound の `traceparent` をプロキシ越しに継続）、OpenTelemetry データモデル上のフィルタ実行ごとの span、admin `/metrics` の host 集約 RED メトリクス、guest 契約に一切触れない host 側 batch/retry ポンプによる OTLP ネットワーク export — [7](ADR/000007.md) · [9](ADR/000009.md) · [40](ADR/000040.md) |
| **プロセスライフサイクル** | 無停止 SIGHUP reload（content-hash 照合・アトミック・all-or-nothing・壊れた編集は fail-closed）と、ロードバランサが依拠できる graceful shutdown 契約——先に `/readyz` が落ち、設定した readiness grace が経過し、それから 1 つの有界 window で drain する。完全な契約は[運用ガイド](operations.ja.md) — [39](ADR/000039.md) · [59](ADR/000059.md) |
| **供給網** | cosign ＋ SBOM 検証つきフィルタロード（digest pin 済みオフライン OCI レイアウト、in-toto subject digest で component に束ねた SBOM、未署名ロードの抜け道なし）。フィルタの全ライフサイクルを覆う operator CLI——`conformance` → `package`（署名・レイアウト書き出し・pin すべき digest を印字）→ `validate --resolve`（loader と同一の provenance ゲートを CI pre-flight として）——に加え `new-filter` / `dev` / `healthz` / `schema` / `--version`。Plecto Proxy 自身のバイナリ・コンテナイメージ・reference filter も同じ規律に従う — [6](ADR/000006.md) · [46](ADR/000046.md) · [47](ADR/000047.md) · [64](ADR/000064.md) · [65](ADR/000065.md) · [80](ADR/000080.md) · [94](ADR/000094.md) |

## 意図的に置いていないもの

不在もまた判断であり、黙った空白ではなく記録として残している:

- **レスポンスキャッシュ**と**native な AI/LLM ゲートウェイ**は native fast path には置かない
  （[ADR 000043](ADR/000043.md)）——per-request のポリシーであり、役割駆動の配置基準
  （[ADR 000029](ADR/000029.md)）は extension plane に置く。
- **WAF** は意図的に extension plane 側（[ADR 000037](ADR/000037.md)）。
- **レプリカ横断の共有状態**は native では却下（[ADR 000053](ADR/000053.md)）。host 保持の状態は全て
  node-local で、fleet 全体の制限は外部ストアを引くフィルタとして表現する。運用面の半身は
  [hardening ガイド](hardening.ja.md)。
- **動的な設定 push**（xDS 型の control-plane プロトコル）は不採用。manifest ＋ SIGHUP が単一の
  source of truth（[ADR 000008](ADR/000008.md)）。
- **EWMA / latency-aware LB**・**ring-hash**・**header-presence / 正規表現 route match** は却下では
  なく未実装。[ロードマップ](ROADMAP.md)を参照。

## これらの主張はどう検証されるか

上の各行はリポジトリ内のテストに対応し、成立の記録は本ページの台帳ではなく **workflow が green で
あること**そのもの。何がどこで走るかは [verification.ja.md](verification.ja.md)、実測値は
[performance/](../performance/README.md)。fairness / enforcement の主張が node-local に閉じる点は
[hardening ガイド](hardening.ja.md)に明記してある。
