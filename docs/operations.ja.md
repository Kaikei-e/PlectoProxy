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
LB 背後での無瞬断ローリングデプロイには admin_addr の設定が前提になる。

## `readiness_grace_ms` の決め方

原則: **最初の readiness チェック失敗から、LB が実際にレプリカを外すまでの時間を猶予が覆う
こと。** drain 開始時点で LB がまだこのレプリカにルーティングしていたら、そのクライアントは
接続拒否を見る — 契約が防ごうとしている瞬断そのもの。

| 前段 | 設定値 |
| --- | --- |
| LB なし（直接クライアント・単一インスタンス） | `0`（既定）。誰も `/readyz` を見ていないので、猶予は shutdown を遅らせるだけ。 |
| Kubernetes | Pod の readiness probe `periodSeconds × failureThreshold` 以上。readinessProbe は `/readyz`、livenessProbe は `/healthz` に向ける。 |
| Active health check（interval × 連続失敗回数） | その積以上（フロント LB が失敗後に保持する hold-down があれば加算）。 |
| Data-plane health check（interval × unhealthy 閾値） | その積以上。 |
| DNS ベースのルーティング | レコード TTL 以上。TTL が分単位なら、先にレコードを消してからシグナルを送る運用を推奨。 |

`SIGTERM` 配送より**前に**ローテーションから外すオーケストレータ（Kubernetes は EndpointSlice
からの除去がそれ）は必要な猶予を縮めるが、その除去のトリガーも readiness probe なので、上の
probe 由来の下限が安全な選択のまま変わらない。

`window_ms` は別の関心事: **既に受け付けた**作業にどれだけ完走を許すかの上限。最も遅い正当な
リクエストに合わせる（既定 30 秒は per-try upstream timeout の既定と、スーパーバイザの一般的な
30 秒 kill 猶予に整合 — 例: Kubernetes の `terminationGracePeriodSeconds` は
`readiness_grace_ms + window_ms` より大きくすること）。

## drain（とトンネル）を観測する

admin `/metrics` は RED シグナルに加えて次を出す:

- `plecto_requests_in_flight` — 現在処理中のリクエスト。drain はこれの完走を待つ。
- `plecto_tunnels_active` — 現在開いている Upgrade トンネル（[ADR 000048](ADR/000048.md)）。
  各トンネルは生存期間中ずっと circuit breaker permit と LB pick を専有するので、リクエスト量に
  見合わず breaker が開く / least-request が偏るときに最初に見るべきゲージ。drain が何本の
  トンネルを切ることになるかもここでわかる。
- `plecto_tunnel_bytes_down_total` / `plecto_tunnel_bytes_up_total` — トンネルが中継した
  バイト数（down = upstream → client、up = client → upstream）。各トンネルの close 時に加算。

## reload と restart の使い分け

設定変更にこの機構は一切不要: `SIGHUP` がマニフェストを再読込し、fail-closed で原子的に
スワップする — 接続影響ゼロ（[ADR 000039](ADR/000039.md)）。shutdown シーケンスに頼るのは
**バイナリまたはホスト**が入れ替わるとき（デプロイ・ノード drain）だけで、その不可視化は
ローリングレプリカ + 本 readiness 契約の仕事。
