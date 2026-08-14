# Hardening ガイド（運用硬化ガイド）

Plecto Proxy を単一インスタンス以上の構成で運用するための運用ガイド。まず押さえるべき事実はひとつ:
**Plecto Proxy のホスト保持状態はすべてノードローカルである。** native fast path に gossip も共有ストアも
クロスインスタンスの合意もない —— 各 `plecto` プロセスは自分が処理したリクエストしか知らない。これは
欠落ではなく意図的な設計境界であり（[ADR 000053](ADR/000053.md)）、外部の協調サービスに暗黙に依存する
代わりに、コアをセルフホスト可能な単一バイナリのまま保つ（[ADR 000008](ADR/000008.md)）ための判断である。

## 「ノードローカル」が指すもの

| 状態 | 場所 | ADR |
| --- | --- | --- |
| native L7 rate limiter（per-route / per-client-IP token bucket） | `plecto-server` fast path | [33](ADR/000033.md) |
| `host-ratelimit` / `host-kv` / `host-counter`（per-filter capability） | `plecto-host` | [26](ADR/000026.md) |
| redb state backend | `plecto-host`（単一プロセス設計） | [41](ADR/000041.md) |
| TLS 1.3 session ticket 鍵 | `plecto-server` | [52](ADR/000052.md) |

これらはいずれもレプリカ間で共有されない。あるインスタンス A 上のカウンタ・バケット・チケット鍵は
インスタンス B からは見えない。

## 前段プロキシ配下でのクライアント同一性

以下で扱う per-client の主張——per-client-IP token bucket、`source_ip` Maglev hashing、アクセスログの
`client` フィールド——は、いずれも「Plecto Proxy がクライアントだと信じているアドレス」の質を超えられない。
前段に別のプロキシやロードバランサを置くと、既定ではそのアドレスは前段のものになる。edge proxy として
Plecto Proxy は受信した転送ヘッダを剥がし、接続 peer から自前の値を発行し直すからである
（[ADR 000018](ADR/000018.md) / [ADR 000022](ADR/000022.md)）——受信 `X-Forwarded-For` はクライアントが
自由に書ける文字列にすぎない。

**第一選択は PROXY protocol v2。** 前段がこれを喋れるなら、`[listen.proxy_protocol]`
（[ADR 000057](ADR/000057.md)）がクライアントアドレスを **HTTP の下**で復元する——TLS ハンドシェイクの前、
ヘッダを 1 バイトも解釈する前である。リクエストの中身が一切影響を及ぼせないため、二つの答えのうち強い方であり、
使える構成では常にこちらを採る。

**第二の答えが `[listen.trusted_proxy]`**——PROXY v2 を喋れない L7 前段のための形である
（[ADR 000103](ADR/000103.md)）:

```toml
[listen.trusted_proxy]
trusted = ["10.0.0.0/8"]   # 前段プロキシの CIDR。単一ホストは "10.1.2.3/32"
```

対象になるのは、Plecto Proxy が既に解決済みのアドレス——接続 peer、または両セクションを宣言した場合は
PROXY v2 で復元されたアドレス——が `trusted` に属するリクエストだけである。そのリクエストでは受信
`X-Forwarded-For` を**右から**読み、宣言済みホップを落とし、どの宣言済みプロキシも保証していない最初の
アドレスをクライアントとする。それ以外はすべて peer に倒れる——ヘッダが無い・不正・全要素が宣言済み、
および CIDR の外から来たすべてのリクエスト。復元源は `X-Forwarded-For` の 1 family のみで、残りの
client-IP ヘッダ family は従来どおり剥がされる。scheme はワイヤの真実のままなので、受信
`X-Forwarded-Proto` は尊重されない。

下流から見た挙動は変わらない。Plecto Proxy は復元の有無にかかわらず自前の `X-Forwarded-For` /
`X-Real-IP` / `X-Forwarded-Proto` を発行し続けるので、フィルタも upstream も権威ある値をちょうど 1 つ
だけ見る。復元が決めるのは「誰がクライアントか」であって、「何を転送するか」ではない。

運用上の注意が二つ。`trusted` は信頼の付与であり、しかも非対称である——狭すぎれば復元が起きないだけだが、
広すぎればその範囲に到達できる者が自分でクライアントアドレスを名乗れてしまう。迷ったら狭く取ること。
そしてモードの切り替えは **reload ではなく restart** である——`[listen.trusted_proxy]` は起動時に
固定されるため、`SIGHUP` だけでは何も変わらない。切り替えると per-client のキー（バケット・ハッシュ
割り当て）が別のキー空間へ移り、カウンタは引き継がれない。

## マルチレプリカ構成でのレートリミット

**第一推奨: 二層を併用する**（[ADR 000061](ADR/000061.md)）。以下で説明する native token bucket は
**local floor**——外部呼び出しゼロで各レプリカの前段に立つ即時 flood 遮断であり、バーストが WASM CPU を
消費したり共有バックエンドへ届いたりする前に落とす。その上に重ねる
[`filter-ratelimit-redis`](../plecto/examples/filters/filter-ratelimit-redis) が **global 層**であり、
貸与された `outbound-tcp` capability（[ADR 000060](ADR/000060.md)）経由で RESP 互換ストア（Redis /
Valkey 等）に問い合わせ、実際のフリート全体の上限を保持する。これは同じ課題に対する業界の
local + global 併用パターンと同型——local がバーストを吸収してから global が実数を保つ。
per-replica floor はどのルートでも常時有効にし、以下の工学的近似ではなく厳密なフリート全体クォータが
要るルートには filter を追加すること。

SaaS 導入水準（[ADR 000054](ADR/000054.md)）における Plecto Proxy の標準的な配置形は、**前段 LB が N 台の
レプリカへ分配する**構成である。レートリミッタはノードローカルなので、`[route.rate_limit]` に設定する
値は**レプリカ 1 台あたり**のバケットであり、フリート全体で 1 つのバケットではない。local floor だけに
頼る場合、これには 2 つの具体的な帰結がある。

**1. 均等分配（round-robin・least-request）—— 実効レートは N 倍になる**

前段 LB がレプリカへほぼ均等にリクエストを分配するなら、あるルートに対するフリート全体の実効許容
レートはおよそ次の式になる:

```
実効レート ≈ 設定値 × N
```

レプリカ数 N によらずフリート全体の目標レート `R_target` を保ちたいなら、各レプリカの設定を次のように
逆算する:

```
設定値 = R_target / N
```

そして `N` をスケールするたびにこの値を再計算する。同じ倍率は `burst` にも適用される。per-client-IP
バケットも同様に影響を受けるが、それは**そのクライアントのリクエストが実際に複数レプリカへまたがって
着地する場合に限る**——またがらない場合は次のパターンを参照。

**2. キー単位で局所性を作る前段（consistent hashing / Maglev）—— ノードローカルがほぼグローバルに近づく**

前段 LB（あるいは Plecto Proxy 自身の weighted Maglev consistent hashing、[README](../README.ja.md) 参照、
[ADR 35](ADR/000035.md)）が、あるキー（典型的には client IP）をハッシュリングの寿命の間ずっと同じ
レプリカへ固定するなら、そのキーのリクエストは 1 ノードの 1 バケットだけが数える。この場合、ノード
ローカルなリミッタは協調なしに**事実上のグローバルリミッタ**として振る舞う。トレードオフは、スケール
アップ/ダウン時のハッシュリング churn で一部のキーが（満タンの）新しいバケットへ一時的に再割り当て
されること、そしてキー分布が偏っていると他のレプリカが空いていても 1 台だけが過負荷になり得ることで
ある——Maglev は素朴な modulo ハッシュに比べこの churn を小さくするが、ゼロにはしない。

## 本当のグローバル制限が要る場合

上記いずれの近似パターンも、協調なしの厳密なグローバル制限を与えるものではなく、あくまで工学的な近似
である。プロダクトがレプリカ数や着地先に関わらず厳密に保持すべきフリート全体のクォータ（例: テナント
ごとの絶対的な API quota）を要求するなら、それは**共有状態**であり、Plecto Proxy の配置基準は共有状態を
native fast path の外に置く（[ADR 000029](ADR/000029.md), [ADR 000053](ADR/000053.md)）。サポートされる
経路は、貸与された outbound capability 経由で外部ストアを叩く**フィルタ**であり、業界の
external global rate-limit 配置と同型——しかも Plecto Proxy ではその「サービス」自体が filter になる
（別プロセス不要、[ADR 000061](ADR/000061.md) の単一バイナリの勝ち筋）。

[`filter-ratelimit-redis`](../plecto/examples/filters/filter-ratelimit-redis) がその reference 実装
（[ADR 000061](ADR/000061.md)）: `outbound-tcp` capability（[ADR 000060](ADR/000060.md)）経由の、
一般形の fixed-window counter（`INCRBY` + 無条件の `EXPIRE ... NX`、Redis 7.0+ / Valkey）。manifest の
`[filter.config]`（`host-config` capability 経由、[ADR 000066](ADR/000066.md)）でバックエンド
host/port・window・limit・cost 取得元・route tag、そして**必須**の `on_backend_error = "deny" | "allow"` を宣言
する——既定値は無く、Redis 障害時にルートを遮断するか local floor だけの可用性優先に倒すかを運用者が
明示的に選ぶ。この filter は `isolation = "trusted"` を要求する: pooled instance が持続接続を跨リクエスト
で保持し（毎回再接続しない）、同じ eager load-time instantiate が必須設定の欠落・不正値を毎リクエスト
503 ではなく load 失敗として表面化させる（詳細は filter 自身のコードコメントと
`docs/writing-a-filter.md` を参照）。

**Demo-only であり、単独では production の主張にならない。** 現行の `filter-ratelimit-redis` は
`AUTH`・ACL・TLS のいずれも実装しておらず、trusted network 限定である。[ADR 000081](ADR/000081.md)
が定める昇格条件——ACL user + `AUTH`（または同等の認証）、TLS（または同等の検証可能な暗号化）、
manifest 直書きではない経路での資格情報配布——を満たすまでは、technical preview・学習用途の補助として
扱い、フリート全体の production クォータの根拠にはしないこと（この昇格条件は
[ADR 000107](ADR/000107.md)（現状 proposed）が 10 項目の反証可能なチェックリストとして再確定し、
cost 入力の検証を必須に加えている）。

このうち三つ目には機構が入った: **`[filter.config_files]`**（[ADR 000095](ADR/000095.md)）は
`host-config` のキーをファイルへ向け、ロード時と各リロード時に読む。よって資格情報は mount された
シークレットとして届き、ローテーションは manifest の編集ではなく `SIGHUP` で拾われる。解決は trust 鍵や
TLS 証明書と同じ fail-closed 検査群の一部で、欠落・二重宣言・非 UTF-8・サイズ超過は最初のリクエストでは
なく `plecto validate` で落ちる。配布の半分はこれで閉じたが、トランスポートの半分（backend 接続での
`AUTH` / ACL / TLS）は未了なので、上の demo-only の位置づけは変わらない。

local floor と併用するのが本ガイドが上で推奨する二層モデルである: local バケットが Redis への
round trip を払う前にバーストを吸収し、filter が通過分の実際のフリート全体の数を守る。実 N-replica
フリートでの local 単独 vs 併用の定量比較は後続の実測作業として記録されている
（[ADR 000061](ADR/000061.md) Consequences、[ADR 000056](ADR/000056.md) R6）——ハーネスが揃い次第、
数値をここにリンクする。それまでは、併用形を「推奨アーキテクチャ」として扱い、まだ「実測済みの主張」
としては扱わないこと。

## fairness / enforcement の主張はノードローカルのスコープ

rate limit の **fairness**（あるキーが他のキーを飢餓させない）や **enforcement**（許容スループットが
設定レートへ収束する）についてのベンチマークや README の主張は、いずれも**単一ノード**での挙動を
記述したものである。マルチレプリカ・フリート全体の集約挙動については何も述べていない——フリート全体を
論じるには上記の式を適用すること。単一ノードでの実測は
[performance/README.md](../performance/README.md#host-enforced-rate-limiting) を参照。

## 関連 ADR

- [ADR 000053](ADR/000053.md) —— 全ホスト状態をノードローカルと宣言する決定。本ガイドはその運用面。
- [ADR 000033](ADR/000033.md)・[ADR 000026](ADR/000026.md)・[ADR 000041](ADR/000041.md)・
  [ADR 000052](ADR/000052.md) —— 本ガイドが扱うノードローカル状態そのもの。
- [ADR 000057](ADR/000057.md)・[ADR 000103](ADR/000103.md) —— 前段配下でクライアント同一性を復元する
  二つの手段。[ADR 000018](ADR/000018.md) / [ADR 000022](ADR/000022.md) の edge 既定を限定する。
- [ADR 000061](ADR/000061.md) —— local floor × global filter の二層レートリミットモデルと、本ガイドが
  推奨する `filter-ratelimit-redis` reference filter。
- [ADR 000060](ADR/000060.md) —— reference filter が RESP 互換ストアへ到達する際に使う `outbound-tcp`
  capability。
- [ADR 000066](ADR/000066.md) —— reference filter が業務設定（バックエンド・window・limit・
  `on_backend_error` 等）を読む `host-config` capability。
- [ADR 000029](ADR/000029.md) —— 役割駆動の配置基準（共有・グローバル状態は native の外）。
- [ADR 000081](ADR/000081.md) —— 現行の輸送・認証を demo-only と宣言し、`filter-ratelimit-redis` の
  production 昇格条件（AUTH/ACL/TLS）を定める決定。
