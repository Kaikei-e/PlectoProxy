# 運用ガイド

Plecto Proxy をフリート（複数レプリカ + 前段 LB）で動かすためのガイド。前段ロードバランサが依拠できる
shutdown / readiness の契約と、それを調整する設定を扱う。[hardening ガイド](hardening.ja.md)
（複数インスタンス時の状態のセマンティクス）の姉妹編で、本ページはプロセスライフサイクルを扱う。

## Graceful shutdown: 契約

`SIGTERM` / `SIGINT` を受けた `plecto` プロセスは、**この順序で**次のシーケンスを実行する
（[ADR 000039](ADR/000039.md), [ADR 000059](ADR/000059.md)）:

1. **`/readyz` が `503 draining` になる** — 即座に、他の何よりも先に。新規接続はまだ受け付け、
   通常どおり処理される。
2. **readiness 猶予が経過する**（`[listen.drain] readiness_grace_ms`、既定 `0`）。前段 LB が
   503 を観測してレプリカをローテーションから外すのに要する時間。既定 `0` ではこのステップは
   潰れ、drain が即座に始まる。
3. **drain 開始。** リスナーは accept を止める。開いている全接続に「in-flight の作業を完走して
   閉じよ」と伝える: HTTP/1.1 は keep-alive 停止、HTTP/2 と HTTP/3 は GOAWAY 送出
   （h3 クライアントの拒否されたリクエストは `H3_REQUEST_REJECTED` で reset され、別レプリカへ
   安全に再試行できる）。Upgrade トンネル（WebSocket）は閉じられる — 長寿命トンネルに drain を
   無期限に待たせない。
4. **drain window がステップ 3 を有界にする**（`[listen.drain] window_ms`、既定 `30000`）。
   TCP リクエスト・h3 リクエスト・トンネル、全経路が同じ一つの window を共有する。満了時に
   まだ開いているものは切断される（fail-closed）。
5. プロセスは `0` で exit する。

`/healthz`（liveness）はこの間ずっと `200` のまま: drain 中のプロセスは意図して終了しつつある
のであって故障ではなく、liveness probe が再起動をかけたら drain が台無しになる。LB の
ローテーション判定は `/readyz` に、再起動監視は `/healthz` に向けること。

```toml
[listen.drain]
readiness_grace_ms = 5000   # ≥ LB の health check 間隔 × unhealthy 閾値
window_ms = 30000           # in-flight の作業に許す完走時間
```

両エンドポイントは admin リスナー（`[observability] admin_addr`、既定オフ）上にある。
LB 背後での無瞬断ローリングデプロイには admin_addr の設定が前提になる。なお `[listen.drain]` と
`[observability]` は起動時に固定される——変更は reload ではなく restart
（[reload と restart の使い分け](#reload-と-restart-の使い分け) 参照）。

## コンテナ内部からのプローブ: `plecto healthz`

公式イメージは distroless — シェルも curl も無い — ため、Docker/Compose の `healthcheck:` は
外部コマンドに頼れない。`plecto healthz` はその自己プローブで、manifest の
`[observability] admin_addr` を読み（Compose 側にアドレスを二重記載しない）、有界の HTTP/1.1
GET を 1 回行い、2xx なら exit `0`、それ以外は `1` を返す — Docker の healthcheck 契約が予約
する `2` は決して返さない。既定のプローブ先は `/readyz`（Compose の
`depends_on: condition: service_healthy` は起動順ゲート = readiness 意味論のため）。再起動監視
向けには `--live` で `/healthz` を、manifest を読ませたくない場合は `--admin-addr <host:port>`
を使う。

```yaml
# distroless: exec-array 形式のみ — 文字列の test: はイメージに無い /bin/sh を要求してしまう
healthcheck:
  test: ["CMD", "/usr/local/bin/plecto", "healthz", "/etc/plecto/manifest.toml"]
  interval: 30s
  timeout: 5s
  retries: 3
```

Kubernetes では admin エンドポイントへ直接 `httpGet` プローブを使うのが良い（kubelet がコンテナ
外からプローブするため、イメージ内で何かを実行する必要がない）— 下表の通り `/readyz` /
`/healthz` に向ける。

## `readiness_grace_ms` の決め方

原則: **最初の readiness チェック失敗から、LB が実際にレプリカを外すまでの時間を猶予が覆う
こと。** drain 開始時点で LB がまだこのレプリカにルーティングしていたら、そのクライアントは
接続拒否を見る — 契約が防ごうとしている瞬断そのもの。

| 前段 | 設定値 |
| --- | --- |
| LB なし（直接クライアント・単一インスタンス） | `0`（既定）。誰も `/readyz` を見ていないので、猶予は shutdown を遅らせるだけ。 |
| Kubernetes | Pod の readiness probe `periodSeconds × failureThreshold` 以上。readinessProbe は `/readyz`、livenessProbe は `/healthz` に向ける。 |
| Active health check（interval × 連続失敗回数） | その積以上（フロント LB が失敗後に保持する hold-down があれば加算）。 |
| Passive health check（interval × unhealthy 閾値） | その積以上。 |
| DNS ベースのルーティング | レコード TTL 以上。TTL が分単位なら、先にレコードを消してからシグナルを送る運用を推奨。 |

**ローテーションからの除去は `SIGTERM` より前に順序付けられていない。** Kubernetes ではこの二つ
は同時に起きる: kubelet が Pod の graceful shutdown を開始するのと同じタイミングで、control
plane はその Pod を Service の EndpointSlice から外すかどうかを評価し、その更新は各データプレーン
へ atomic にではなく eventually に伝播する。つまりレプリカは `SIGTERM` を受け取った後もなお新規
接続を渡されうる。上の probe 由来の下限が安全な選択のまま変わらないのは、まさにそのため —
`readiness_grace_ms` は「除去が伝播しきる時間」を覆う必要があり、既に終わっている前提は置けない。

`window_ms` は別の関心事: **既に受け付けた**作業にどれだけ完走を許すかの上限。最も遅い正当な
リクエストに合わせる（既定 30 秒は per-try upstream timeout の既定に整合）。

そのうえでスーパーバイザ側を確認すること。**その kill 猶予は `readiness_grace_ms + window_ms`
より大きくなければならない** — そして既定では余裕が無い: Kubernetes の
`terminationGracePeriodSeconds` の既定 30 秒は既定の `0 + 30000` とちょうど等しく、`SIGKILL` は
window が切れた後ではなく切れると同時に届く。Docker はさらに厳しい。`docker stop` は `SIGTERM`
を送り、デーモン既定で Linux コンテナは 10 秒の猶予の後に `SIGKILL` を送る（コンテナ単位では
`--stop-timeout`）。Compose の `stop_grace_period` も既定 10 秒。つまり 30 秒の window はそもそも
完走できず、プロセスは drain の途中で kill され、接続は drain 自身の条件で閉じられるのではなく
切断される。`readiness_grace_ms + window_ms` をコンテナの stop 猶予より下げるか、stop 猶予を
それより上げるかのどちらかにすること。再作成は古いコンテナを stop するので、`docker compose up`
にも `docker compose stop` と同じ猶予が効く。

`window_ms = 8000` と既定の `readiness_grace_ms = 0` なら、上の healthcheck と同じサービスに:

```yaml
stop_grace_period: 12s   # > readiness_grace_ms + window_ms
```

## drain（とトンネル）を観測する

admin `/metrics` は RED シグナルに加えて次を出す:

- `plecto_requests_in_flight` — 現在処理中のリクエスト。drain はこれの完走を待つ。
- `plecto_tunnels_active` — 現在開いている Upgrade トンネル（[ADR 000048](ADR/000048.md)）。
  各トンネルは生存期間中ずっと circuit breaker permit と LB pick を専有するので、リクエスト量に
  見合わず breaker が開く / least-request が偏るときに最初に見るべきゲージ。drain が何本の
  トンネルを切ることになるかもここでわかる。
- `plecto_tunnel_bytes_down_total` / `plecto_tunnel_bytes_up_total` — トンネルが中継した
  バイト数（down = upstream → client、up = client → upstream）。各トンネルの close 時に加算。

## アクセスログ: フィールド契約

アクセスログは opt-in で、既定では**無効**。`[observability] access_log` で有効にする
（`[observability]` の他の項目と同じく起動時固定——有効化は restart）:

```toml
[observability]
access_log = true
```

有効にすると、リクエストごとに `plecto::access` ターゲットへ `tracing` イベントが 1 本出て、
バイナリの JSON サブスクライバがそれを 1 行として描画する。イベントのフィールドはその行の
**top-level** に——`timestamp` / `level` / `target` と同じ深さに——並ぶ。取り込み層はネストした
オブジェクトを展開せずに、そのまま型付きスロットへ写せる。

```json
{"timestamp":"...","level":"INFO","client":"203.0.113.7","scheme":"https","method":"GET","authority":"api.example.com","path":"/v1/items","status":200,"duration_ms":12,"trace_id":"4bf92f3577b34da6a3ce929d0e0e4736","span_id":"00f067aa0ba902b7","message":"access","target":"plecto::access"}
```

> **平坦化前のリリースからの移行:** 同じフィールドはかつて `fields` オブジェクトの中にあった
> （`fields.method` / `fields.status` …）。名前は変わっておらず、ネストが無くなっただけ。
> 取り込み設定を行の直下へ向け直すこと。

| フィールド | 型 | 意味 |
| --- | --- | --- |
| `client` | string | このトランザクションを帰属させるアドレス。接続 peer、または宣言済み `[listen.trusted_proxy]` 配下ならその proxy が名指ししたクライアント（[ADR 000103](ADR/000103.md)）。再発行される `X-Forwarded-For` と per-client rate limit が使うアドレスと同一なので、ログと強制は一致する。 |
| `scheme` | string | `http` / `https`。ワイヤから取る値であり、inbound の `X-Forwarded-Proto` は決して尊重しない。 |
| `method` | string | 受信したままのリクエストメソッド。 |
| `authority` | string | リクエストの host authority。 |
| `path` | string | リクエストパス。**クエリ文字列は落とす**。 |
| `status` | number | クライアントへ返したステータス。プロキシが応答できなかった転送エラーは `502` として記録する。 |
| `duration_ms` | number | トランザクション開始から応答ヘッダまでのミリ秒（整数）。 |
| `trace_id` | string | W3C trace id（小文字 hex 32 桁）。呼び出し元が `traceparent` を送っていればその値、なければ Plecto が採番した値。 |
| `span_id` | string | Plecto 自身の request span の W3C span id（小文字 hex 16 桁）。upstream へ伝播するのもこの id。 |
| `response_body_inspection_skipped` | string（該当時のみ） | `on-response-body` フィルタを宣言したルートで、そのフィルタが見られなかったレスポンスの理由（[ADR 000098](ADR/000098.md)）: `streaming-content-type` / `content-encoding` / `partial-content` / `over-cap`。それ以外のトランザクション（該当フィルタを持たないルートを含む）ではキー自体が**出ない**（`null` にはならない——取り込みマッピングを null 値に固定しないこと）。 |

この行が守る性質は 2 つ:

- **秘密を持たない。** `Authorization` も `Cookie` も——そもそもヘッダ値を一切——出さず、path は
  クエリ文字列を落として出す。したがってアクセスログをトラフィック本体より低信頼な宛先へ送ること
  自体は、それだけでは開示にならない。
- **`trace_id` / `span_id` はサンプリングの有無にかかわらず常に出る。** 下流でサンプリングされて
  残った何かとこの行を結合でき、サンプリングされなかったトランザクションについては、この行が
  唯一の手がかりになる。遅いリクエストとそのトレースを結ぶのは両側の `trace_id`。

**このフィールド集合は契約であり、manifest スキーマと同じ扱いをする。** フィールドの追加・改名・
削除は公開インターフェースの変更であり、そこに記された pre-1.0 バージョニング方針のもとで
[`CHANGELOG.md`](../CHANGELOG.md) の **Changed** に移行注記つきで載る。取り込み設定は行の順序や
全体の形ではなく、上表のフィールド名に対して固定すること。

## トレース export: `otlp_endpoint`

`[observability] otlp_endpoint` は collector の **base URL** であり、exporter がそこへ `/v1/traces`
を付与する（`OTEL_EXPORTER_OTLP_ENDPOINT` と同じセマンティクス）。`[observability]` の他の項目と
同じく起動時に読まれるので、変更は restart。ポートとパスは省略できる。

**export は平文 OTLP/HTTP のみ。** Plecto が実装しているのは exporter 仕様のそのサブセットであり、
そこではスキームが輸送セキュリティの唯一の決定因なので、値は `http://` の base URL でなければ
ならない。それ以外のスキームは `plecto validate` でも起動でも fail-closed で落ちる——清潔に起動して
何も export しない、という結末にはならない（[ADR 000111](ADR/000111.md)）。TLS のみの collector へ
出したいときは、プロキシと同居する OpenTelemetry Collector（agent パターン）を立て、`otlp_endpoint`
を平文 `http://` でそこへ向け、TLS は collector に張らせる——export の資格情報の置き場所も本来そこ。

```toml
[observability]
otlp_endpoint = "http://127.0.0.1:4318"  # ローカル collector が以降の TLS を張る
```

## 宣言したレスポンスヘッダ: どの応答に乗るか

ルートは、常に付けたいレスポンスヘッダをフィルタ無しで宣言できる:

```toml
[[route]]
upstream = "app"
[route.match]
path_prefix = "/"
[route.headers]
set = { "X-Content-Type-Options" = "nosniff", "Referrer-Policy" = "no-referrer" }
remove = ["server"]
```

どちらのキーも省略できるが、ブロックには少なくとも一方が要る。**値はリテラルのみ**——条件分岐も、
リクエスト値の補間も、パターンも無い。値がリクエストによって変わるヘッダは per-request の判断であり、
それはフィルタの仕事である（[ADR 000029](ADR/000029.md) · [ADR 000100](ADR/000100.md)）。

**この宣言は「フロア」であって提案ではない。** レスポンスフィルタチェーンの後、圧縮の前に適用される:

- `set` は upstream やフィルタが付けた同名ヘッダを**置き換える**（複数あればすべて）。`remove` は
  その名前を丸ごと落とす。`remove` が先に走るので、両方に載せた名前は最終的に set される。
- ヘッダ名は大文字小文字を区別せずに照合する——`X-Frame-Options` と `x-frame-options` は同じ宣言で
  あり、ひとつの `set` に両方書くと曖昧として拒否される。
- **そのルートが返す全応答**に乗る。想定しづらい経路も含む: フィルタの `replace`、フィルタの
  short-circuit、チェーンの fail-closed 5xx、native レートリミットの 429、転送側の 502 / 503 / 504。
  「壊れたときに消えないこと」が価値そのものであるセキュリティヘッダは、壊れたときにも消えない。

**穴はひとつ、意図的なもの。** route が決まる**前**に返る応答には宣言が乗らない。取ってくる route が
無いからである。該当するのは no-route の **404** と、パス正規化の **400**（曖昧またはルート脱出を
する request target を ingress で拒否したもの）。これらにもヘッダが要る場合は別の場所で終端するか、
穴として受け入れること——listener 単位の宣言は提供していない。

**検証は fail-closed。** 不正なヘッダ名・値は、リクエスト時に黙って落とされるのではなく
`plecto validate`・起動・reload を落とす。hop-by-hop ヘッダ（`connection` / `transfer-encoding` /
`upgrade` / `te` / `trailer` / `keep-alive` / `proxy-connection` / `proxy-authorization` /
`proxy-authenticate`）や `content-length` を名指しした場合も同じ: 接続管理はトランスポートのもので
あり、長さは Plecto が実際に送る body のものである——宣言された長さは response desync の材料になる。

## リクエストタイムアウト: 実際に効く値はどれか

転送されるリクエストは二つの bound で区切られ、どちらも既定値は upstream が宣言する:

| ノブ | 何を区切るか | 既定 | `0` の意味 |
| --- | --- | --- | --- |
| `request_timeout_ms`（per-try） | 1 回の試行が応答**ヘッダ**に到達するまで。ヘッダ到達後の body はデッドライン無しでストリームする | `30000` | per-try 無効——long-poll / streaming のオプトアウト |
| `overall_timeout_ms` | トランザクション全体: 全試行**＋**その間の backoff | `0` | overall 無し |

route は自分のトラフィックについてどちらも上書きできる。レイテンシ特性の異なる route が、`[[upstream]]`
を複製せずに同一 upstream を共有できる（複製すると health プローバも複製され、ひとつのバックエンドに
対して circuit breaker の状態が分裂する）:

```toml
[[upstream]]
name = "app"
addresses = ["127.0.0.1:9000"]
request_timeout_ms = 5000     # この upstream 宛の全 route が継承する既定
[upstream.health]
path = "/healthz"

[[route]]                     # 5000 を継承
upstream = "app"
[route.match]
path_prefix = "/api"

[[route]]                     # 同じ upstream、より長い予算
upstream = "app"
[route.match]
path_prefix = "/images/resize"
[route.timeouts]
request_timeout_ms = 30000
overall_timeout_ms = 45000

[[route]]                     # 同じ upstream、per-try は掛けない
upstream = "app"
[route.match]
path_prefix = "/events"
[route.timeouts]
request_timeout_ms = 0
```

**解決順は規則ひとつ: route が宣言していればその値、していなければ upstream の値**——ノブごとに独立
して適用される。`overall_timeout_ms` だけを書いた route は、per-try については upstream の値で動く。

**「書かない」と「`0` と書く」は別物である。** 書かなければ upstream の値を取り、`0` はその route に
ついてその bound を無効化する。`[route.timeouts] request_timeout_ms = 0` は、短い upstream 既定から
streaming route を外すための書き方であって、ブロックを省略するのとは意味が違う。

**二つの bound は同時に効き、厳しい方が勝つ。** 各試行は per-try の値**と** overall 予算の残量の、
小さい方で区切られる。overall 予算は試行と backoff が消費するたびに縮む。per-try より小さい
`overall_timeout_ms` は拒否しない——runtime が小さい方を適用するだけなので、単一試行が途中で切られる
ことはある。

超過はいずれも fail-closed の **504**。どちらだったかは fault マーカーで分かる:
per-try 超過は `x-plecto-fault: upstream-timeout`、overall 超過は `request-timeout`。

意図的にやっていないことが二つある。**解決後の値をレンダリングする出力は無い**——ある route が何秒で
動いているかは、その `[route.timeouts]` と upstream 側のフィールドを読んで判断する。そして
`max_retries` / `[upstream.circuit_breaker]` / `[upstream.outlier_detection]` は per-upstream の
ままである。これらは「このバックエンドにどれだけ負荷をかけてよいか」「このバックエンドは壊れているか」
というバックエンドの性質であって、この route の時間予算ではない
（[ADR 000102](ADR/000102.md)）。

## アップグレード: 独立した二つのバージョン系列

Plecto は**二つのバージョン系列**を持ち、**両者は独立に動く**:

| 系列 | 何のバージョンか | どこで見えるか |
| --- | --- | --- |
| バイナリ / イメージ / ライブラリクレート | プロキシ自身: manifest スキーマ・CLI・データプレーン・ホスト | `plecto --version`、イメージタグ、クレートのバージョン |
| `plecto:filter@<version>` | プロキシとフィルタの間の WIT 契約 | `plecto --version` の `filter contracts:` 行と、起動時のフィルタごとの 1 行 |

**プロキシのバンプにフィルタの再ビルドが必要になることは決してない。** ホストはサポートする全
契約版をロードし続けるので、古い契約版に対して作られたフィルタはプロキシのアップグレードを
またいで動き続ける——セキュリティ修正を含むパッチアップグレードも同様。取ってよい。

別の問いに答える 2 つのコマンド:

```console
$ plecto --version
plecto 0.11.1 (profile: minimal)
filter contracts: plecto:filter@0.1.0, plecto:filter@0.2.0, plecto:filter@0.3.0, plecto:filter@0.4.0
```

これは**このバイナリが受け付ける集合**。**手元のフィルタ**が実際に何にバインドしたかは別の問い
で、起動時（および reload のたび）にフィルタごとの 1 行が答える:

```json
{"timestamp":"...","level":"INFO","filter":"hello","contract":"plecto:filter@0.3.0","isolation":"trusted","message":"filter loaded","target":"plecto_control"}
```

アップグレード前に、そこに出ている `contract` がすべて新バイナリの `filter contracts:` に残って
いることを確認する。残っていれば——リリースノートがある契約版の廃止を宣言していない限り残る
——そのアップグレードにフィルタ側の作業は一切要らない。major 契約版は最低 2 リリース系列は
ロード可能なまま維持され、その廃止は単独の ADR で宣言される（互換ポリシーは
[ADR 000085](ADR/000085.md)）。黙って消えることはない。

## CI プリフライト: `plecto validate --resolve`

manifest の編集やフィルタ digest の更新は、reload 時ではなく CI で落ちるべきもの。`plecto validate
<manifest.toml>` は artifact を要しない全 fail-closed 起動時検査（strict parse・route / upstream /
TLS の検査）を実行し、何も変異しない（state ファイルも作られない）ので、本番 manifest に対して
どこで実行しても安全。`--resolve` はそれを artifact 層まで拡張する: 各 `[[filter]]` の OCI layout
を実際に解決し、pin された digest を照合し、ローダの provenance ゲート（信頼鍵による component /
SBOM 署名検証 + SBOM↔component binding）を実走する — serving なし・wasmtime なし・状態無変更の
まま（[ADR 000094](ADR/000094.md)）。

このゲートはローダが起動時と `SIGHUP` 時に呼ぶ関数そのものであって再実装ではないため、CI の
green と実ロードの green は artifact 層で乖離しえない。契約は exit code: すべてロード可能なら
`0`、そうでなければ非 0（成功時はフィルタごとに `filter <id> OK: artifact verified (<digest>)`
を 1 行出す）。

```bash
plecto validate manifest.toml            # 静的: 設定のみ
plecto validate --resolve manifest.toml  # + digest pin・署名・SBOM binding
```

意図して load 時に残しているものが 2 つある: 契約バージョン対応と trusted `init()` の挙動は
compile / instantiate を要し、validate の「何も変異しない」契約を壊すため — どちらも実ロード時に
fail-closed のまま検出される。この検査に供給する authoring 側パイプライン（`plecto conformance` →
`plecto package` → 印字された digest を pin）は [writing a filter §5](writing-a-filter.md) を参照。
0.8.0 以降、下層の conformance ゲートは case ごとに五値の verdict を付け、`pass` と `na` 以外——
component が import する capability を実行側が貸せなかった `environment` を含む——はすべて
exit code を非 0 に保つ。

## reload と restart の使い分け

設定変更にこの機構は一切不要: `SIGHUP` がマニフェストを再読込し、fail-closed で原子的に
スワップする — 接続影響ゼロ（[ADR 000039](ADR/000039.md)）。shutdown シーケンスに頼るのは
**バイナリまたはホスト**が入れ替わるとき（デプロイ・ノード drain）だけで、その不可視化は
ローリングレプリカ + 本 readiness 契約の仕事。

**マニフェストが指すファイルのローテーションも設定変更である。** reload ゲートはパスだけでなく
参照ファイルの**中身**を digest するので、`[[tls]]` の証明書・鍵、`[upstream.tls]` の CA や
client identity、`[resumption]` の STEK、`[filter.config_files]` のシークレットを**その場で**
上書きして `SIGHUP` を送れば、マニフェストを一切編集せずに再ビルド＆スワップされる。certbot の
deploy hook は「配置（あるいは更新そのもの）＋ `kill -HUP`」だけで済む。これを支えるのは二層構成で、
公開素材（証明書・CA バンドル）はログに出る config version に乗り、秘密素材（秘密鍵・STEK・
設定ファイルの値）は**意図的にログへ出さない**別の fingerprint に乗る——低エントロピーな秘密の
digest がログに出れば、それはオフライン総当たりのオラクルになるため。reload ではなく restart が
必要なのは `[trust]` と `[state]` の 2 つで、どちらも `SIGHUP` が fail-closed で拒否する。
もう一群、**拒否されず起動時固定**のものがある: `[listen]` のリスナー半分（`addr` /
`advertised_port` / `proxy_protocol` / `trusted_proxy` / `drain`——reload が消費する
`[listen.client_auth]` だけが例外）と `[observability]` 全体はプロセス起動時に取り込まれる。
これらのセクションだけを編集しても config version は変わらないため、`SIGHUP` は
「unchanged」をログして何も差し替えない——ここは restart を予定に入れること。
