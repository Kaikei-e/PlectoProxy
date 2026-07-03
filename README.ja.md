<div align="center">

# Plecto

**セルフホスト可能・プログラマブルな L7 リバースプロキシ / API ゲートウェイ — Rust 製、WebAssembly で拡張する。**

[![CI](https://github.com/Kaikei-e/Plecto/actions/workflows/ci.yml/badge.svg)](https://github.com/Kaikei-e/Plecto/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust edition 2024](https://img.shields.io/badge/rust-edition%202024-orange.svg)](https://doc.rust-lang.org/edition-guide/)
[![Status: early development](https://img.shields.io/badge/status-early%20development-yellow.svg)](#ロードマップ)

[English](README.md) · 日本語

</div>

---

Plecto は、**相補関係にある二つの構成要素**を型付き [WIT](https://component-model.bytecodealliance.org/) 契約で**結ぶ**:

- **fast path**（native Rust） — 接続受付・TLS 終端・HTTP/1.1・2・3・ルーティング・ロードバランシング・upstream 管理。
- **extension plane**（WebAssembly Component Model フィルタ） — 各リクエストの*判断*（認証・ヘッダ/ボディ書換・rate limit・WAF・ポリシー）。**任意の言語**で書き、`plecto:filter` 契約で差し込み、**無停止で差し替え**る。

速度が要となる経路は native Rust のまま。リクエストのロジックはサンドボックス化された WASM コンポーネントとして走り、**ホストが明示的に貸した能力以外には何も触れられない** —— それを強制するのは規約ではなくサンドボックスである。

> [!WARNING]
> **現状: 初期開発段階。** 設計は確定済み（49 本の ADR・うち 48 が accepted）で、基盤は end-to-end で動く: `plecto:filter` 契約・フィルタをロードして実行する wasmtime ホスト・そして **fast path** —— **HTTP/1.1・HTTP/2（ALPN）・HTTP/3（QUIC）** と **TLS** を終端し、host・path-prefix・method・header・query を specificity 順で **routing**（weighted な **traffic split / canary** つき）し、ルートの filter chain をヘッダ **と** リクエスト body に対して回し、クライアント IP を edge モデルで伝播し、**healthy な upstream instance へロードバランシングする** —— round-robin・**weighted least-request（power-of-two-choices）**・**weighted Maglev consistent hashing** から選べ、active/passive **health check**・**outlier detection**・per-upstream の **circuit breaker**・二段（per-try ＋ overall）**timeout**・jittered **retry**・native L7 **rate-limit** の床が支える。upstream への経路は **TLS+ALPN で再暗号化**でき（gRPC/HTTP-2 パススルー・custom CA）、hostname upstream は DNS を **定期的に再解決**してコンテナの再作成に追従する。per-route の **HTTP/1.1 `Upgrade` token allowlist** が WebSocket トンネルを end-to-end で成立させる。セキュリティ堅牢化（[ADR 000027](docs/ADR/000027.md)）により route 選択は信頼できる認証境界になり（path は ingress で正規化し、encode された迂回は fail-closed で拒否）、host 保持の状態は per-filter quota で縛り、inbound のリソース上限を強制する。出荷バイナリには SIGHUP hot reload・graceful shutdown・OTLP トレース export・operator CLI（`plecto validate` / `schema` / `--version`）が配線済みで、`v0.1.0` タグには署名付きアーティファクトの release パイプライン（cosign ＋ SBOM）自体も切られている。テスト一式は green で CI に載っている —— 読める・動かせる・フィルタを書ける基盤である。[ロードマップ](#ロードマップ)参照。

## なぜ Plecto か

ゲートウェイは必ず「**カスタムロジックをどこに置くか**」にぶつかる。従来の答えにはそれぞれトレードオフがある:

| アプローチ | プロセス内の速さ | サンドボックス | 言語自由 | 無停止差替 |
| --- | :---: | :---: | :---: | :---: |
| 設定 / DSL | ✅ | ✅ | ❌ | ✅ |
| 本体に再コンパイル組込 | ✅ | ❌ | ❌ | ❌ |
| 別プロセス（`ext_proc`・サイドカー） | ❌ | ✅ | ✅ | ✅ |
| **WASM フィルタ — Plecto** | ✅ | ✅ | ✅ | ✅ |

データプレーンのフィルタを WASM で動かすという発想は **Envoy と proxy-wasm が切り拓いた**もの。proxy-wasm は初期の WASM ABI（v0.2.1）を対象としており、その後 **Component Model と WIT** が型付き・多言語・合成可能な基盤として成熟し、Plecto はその上にネイティブに築く。**Cloudflare の Pingora** のような高性能 Rust プロキシは native なデータ経路の速さを示す。Plecto の焦点は **その速さと Component Model の extension plane を組み合わせる**こと ―― 自分で運用し、トラフィックも秘密も自分のインフラに留めたいチームのために、**データ主権**を第一原理として据える。

根拠と却下した代替案は [ADR 000001](docs/ADR/000001.md) を参照。

## 設計テネット

> 安全 × ポータビリティ × セルフホスト性 × 運用の単純さ **＞** 機能網羅性 × 強い権限 × 分散デフォルト。

- **deny-by-default capability** — フィルタはホストが貸した host-API（KV・counter・metrics・log・clock・random）以外に到達できない。任意の outbound・FS・socket は貸与されない限り不可。Component Model サンドボックスが強制する。
- **判断は型で** — フィルタは `decision` variant を返す: `continue` / `modified` / `short-circuit`。曖昧なフラグや暗黙の副作用にしない。
- **init と per-request を分離** — 高コスト初期化（regex compile・スキーマ構築）は `init` フックへ、per-request のホット経路は軽く保つ。
- **フィルタはステートレス** — rate limit・セッション・キャッシュ等の状態はホスト KV に置く。だからフィルタはプール再利用・スケール・無停止差替が綺麗に決まる。
- **fail-closed** — フィルタの trap や deadline 超過で素通り（fail-open）させない。
- **single-node first** — 一台で仕事は完結する。分散（メンバーシップ・設定合意）はオプトイン。
- **データプレーンで panic 禁止** — たった一つの不正リクエストが worker を巻き込んではならない。

## アーキテクチャ

Plecto は速い **native の高速道路** ＋ **あなた自身のコードが走る検問所** という構成: 高速道路（native
Rust）が接続受付・TLS 終端・HTTP・ルーティング・LB を担い、**extension plane** が各リクエストをあなたの
*フィルタ*——小さな sandbox 化された WASM プログラム——に渡し、それが検査して3つの判断のいずれかを返す。
ポリシーはこの判断に宿る。

```mermaid
flowchart LR
    client(["クライアント"])
    upstream(["upstream サービス"])

    subgraph fast["fast path · native Rust"]
        direction TB
        edge["接続受付 · TLS · HTTP/1·2·3"]
        route["route 照合 · load balance"]
        edge --> route
    end

    subgraph ext["extension plane · あなたのフィルタ（sandbox WASM）"]
        direction TB
        inspect["各リクエストを検査<br/>ヘッダ、必要なら body も"]
        decide{"判断"}
        inspect --> decide
    end

    state[("host 保持の状態とサービス<br/>rate-limit · KV · counter · log · clock")]

    client -->|"1 · リクエスト"| edge
    route -->|"2 · filter chain を実行"| inspect
    decide -->|"3 · continue / modify → 転送"| upstream
    decide -.->|"3 · reject＝その場で応答<br/>401 / 403 / 429 — upstream に届かない"| client
    upstream -->|"4 · レスポンス（戻りで filter が改変可）"| client
    decide <-->|"貸与された capability のみ呼べる"| state
```

**continue**（素通し）・**modify**（ヘッダ/body を書換えて通す）・**reject**（*その場で* `401/403/429` を
返す＝**upstream に届かない**）——これがメンタルモデルの全て。フィルタは **stateless**：覚えておくべきものは
host 側にあり、**明示的に貸与された host サービスだけ**を呼べる（deny-by-default）。

フィルタは署名済み WASM component で、**同じ** component を「どれだけ信頼するか」で2通りに走らせられる——
これが性能の最大レバー：

```mermaid
flowchart TB
    wasm["フィルタ＝署名済み WASM component 1つ<br/>（任意の言語で書ける）"]
    verify["署名検証してからロード<br/>署名不正 → 拒否（fail-closed）"]
    profile{"どれだけ信頼する？"}

    pooled["trusted → pooled<br/>一度だけ構築・インスタンス再利用<br/>速いホット経路（~2 µs / req）"]
    fresh["untrusted → リクエスト毎に fresh<br/>毎回作り直し＋ゼロ化<br/>強い隔離（~12倍遅い）"]

    guards["全インスタンス常時:<br/>時間上限 · メモリ上限<br/>trap / timeout で fail-closed"]

    wasm --> verify --> profile
    profile -->|trusted| pooled
    profile -->|untrusted| fresh
    pooled --> guards
    fresh --> guards
```

**判断の指針:** ユーザー固有のロジック・ポリシー・WAF・認証・書換 → WASM フィルタ。TLS・ルーティング・LB・コネクションプール・グローバルカウンタ → native Rust —— [ADR 000029](docs/ADR/000029.md) が固定した「役割駆動」の配置基準で、native は横断的な関心事にのみ育ち、per-request のポリシーには育たない。WASM 税（データコピー＋ホストコール）はリクエスト判断ロジックにのみ課し、速い経路には課さない——pooled フィルタで **~2 µs/req** と実測（[performance](performance/README.md)）。

## いま gateway ができること

native fast path は「動くプロキシ」をとうに越えて成熟している。実装済みかつ CI green な機能のスナップショット（各行は決定 ADR にリンク）:

| 関心事 | いま |
| --- | --- |
| **Edge & HTTP** | HTTP/1.1・HTTP/2（ALPN）・HTTP/3（QUIC、Alt-Svc 広告）。TLS 終端＋SNI 証明書選択、manifest 宣言、fail-closed — [ADR 13–16](docs/ADR/000013.md) |
| **Routing & upgrade** | host・path-prefix・method・header・query の照合を **specificity 順** で解決。weighted **traffic split / canary**。ingress 正規化で path を fail-closed な認証境界に。per-route の **HTTP/1.1 `Upgrade`** トンネリングで WebSocket（`h2c` は拒否） — [34](docs/ADR/000034.md) · [48](docs/ADR/000048.md) |
| **Load balancing & upstream** | per-upstream の **round-robin**（既定）・**weighted least-request**（P2C）・**weighted Maglev**。active＋passive health check、outlier detection、circuit breaker、二段 timeout、jittered retry。per-upstream **TLS+ALPN 再暗号化**（gRPC 対応）と **定期 DNS 再解決** — [17](docs/ADR/000017.md) · [35](docs/ADR/000035.md) · [42](docs/ADR/000042.md) · [44](docs/ADR/000044.md) |
| **Rate limiting** | native L7 token-bucket の床（**route** 単位 / **client-IP** 単位）。加えてフィルタに貸す per-filter の `host-ratelimit` — [33](docs/ADR/000033.md) |
| **Extension plane** | `plecto:filter` chain をヘッダ **と**、opt-in したフィルタには body にも回す（header-only なフィルタは zero-copy）。型付き `decision`。trusted **pooled** / untrusted **fresh**。deny-by-default の host-API ＋ per-filter / host-wide quota。feature-gated の **outbound HTTP**（SSRF ガード付き） — [1](docs/ADR/000001.md) · [25](docs/ADR/000025.md) · [38](docs/ADR/000038.md) |
| **Client IP** | edge モデル伝播 —— chain 実行の前に実 peer から `X-Forwarded-For` / `X-Real-IP` を付け直す — [18](docs/ADR/000018.md) |
| **Supply chain & ops** | cosign ＋ SBOM 検証済みのフィルタロード。無停止 SIGHUP reload ＋ graceful shutdown を出荷バイナリに配線。W3C トレース伝播・RED メトリクス・OTLP export。`plecto validate` / `schema` / `--version`。Plecto 自身のバイナリと container image も同じ署名付きアーティファクト規律に従う — [6](docs/ADR/000006.md) · [39](docs/ADR/000039.md) · [46](docs/ADR/000046.md) · [47](docs/ADR/000047.md) |

## フィルタ契約

Plecto の中核は `plecto:filter` WIT ワールド — Plecto 固有の語彙（型付き `decision`、init/per-request フック、deny-by-default な host-API）を持ちつつ、polyglot 互換のため標準型を再利用する独自ワールドである。

```wit
package plecto:filter@0.1.0;

interface types {
  // request 側フィルタの型付き戻り値。決して裸のフラグにしない。
  variant request-decision {
    %continue,                       // 次のフィルタへそのまま渡す
    modified(request-edit),          // edit を適用して継続
    short-circuit(http-response),    // チェーンを止め、ここで応答を合成する
  }
}

// deny-by-default: 能力ごとに 1 interface。フィルタは貸与されたものだけを import する。
interface host-kv      { get: func(key: string) -> option<list<u8>>; set: func(key: string, value: list<u8>); /* … */ }
interface host-counter { increment: func(key: string, delta: s64) -> s64; /* アトミックな名前付き counter */ }
interface host-log     { log: func(level: level, message: string); }
// host-ratelimit は token bucket をホストネイティブに保つ —— ホット経路の refill/カウントは WASM 境界を
// 跨がない。bucket 仕様（capacity/refill）は manifest で host 設定。フィルタは (key, cost) だけを渡すので、
// untrusted フィルタは自分の制限を緩められない（ADR 000005 / 000026）。

// 基本契約: header-only なフィルタ（auth・rate-limit・WAF・rewrite）はこの world を対象にする。host は
// `on-request-body` の「不在」を、body を buffer せず素通しする合図として読む —— body に触れないフィルタは
// zero-copy のまま（ADR 000038）。
world filter {
  import host-log;  import host-clock;  import host-kv;  import host-counter;  import host-ratelimit;
  export init: func();                                                // 重い・instance ごと一度
  export on-request:  func(req: http-request)  -> request-decision;   // ホット経路（ヘッダ）
  export on-response: func(resp: http-response) -> response-decision; // ホット経路（ヘッダ）
}

// body を読む契約: `filter` ＋ `on-request-body`。この export の「存在」こそが、host に body を buffer させ
// このフックを走らせる合図になる（buffer-then-decide、ADR 000025）。
world filter-body {
  import host-log;  import host-clock;  import host-kv;  import host-counter;  import host-ratelimit;
  export init: func();
  export on-request:      func(req: http-request)  -> request-decision;
  export on-request-body: func(body: list<u8>)     -> request-body-decision;  // buffer 済み body hook
  export on-response:     func(resp: http-response) -> response-decision;
}
```

> v0.1.0 は当初 **sync・header-only** だったが、**request 側の body hook**（`on-request-body`。v1 は body を buffer 済みの `list<u8>` で受ける、[ADR 000025](docs/ADR/000025.md)）が end-to-end で動くようになり、`filter-body` を対象にしたフィルタはヘッダだけでなく body も変換・short-circuit できる。**実験的・feature-gated** な `stream<u8>` body ワールド（[ADR 000020](docs/ADR/000020.md)）と `wasi:http` 型の再利用が次で、いずれも P3 ゲスト toolchain 待ちで gated。

## フィルタを書く

フィルタはワールドを実装したコンポーネントにすぎない。同梱の例（`examples/filters/filter-quickstart`、Rust）:

```rust
wit_bindgen::generate!({ path: "../../../wit", world: "filter" });

struct FilterQuickstart;

impl Guest for FilterQuickstart {
    fn init() {}

    fn on_request(_req: HttpRequest) -> RequestDecision {
        RequestDecision::Continue
    }

    fn on_response(_resp: HttpResponse) -> ResponseDecision {
        // このフィルタが目に見える形でやる唯一のこと: ヘッダを1つ付け足して、
        // `curl -i` で WASM フィルタが応答に触れたことを見せる。
        ResponseDecision::Modified(ResponseEdit {
            set_status: None,
            set_headers: vec![Header { name: "x-plecto".into(), value: "hello-from-wasm".into() }],
            remove_headers: vec![],
        })
    }
}

export!(FilterQuickstart);
```

これは基本の `filter` world を対象にしている —— header-only なので host は body を素通しする。body が要る
フィルタ（`POST` の認証・WAF・body 書換）は `filter-body` を対象にし、export を1つ追加する。実用例フィルタは
[`filter-apikey`](plecto/examples/filters/filter-apikey)（header-only）、`filter-body` の例は
[`filter-hello`](plecto/examples/filters/filter-hello)（host 自身の conformance fixture）を参照。

契約が WIT なので、**WASM コンポーネントへコンパイルできる言語ならどれでもフィルタを書ける** — Rust・Go（TinyGo）・JavaScript/TypeScript（`jco`）・Python（`componentize-py`）。polyglot フィルタ SDK は[ロードマップ](#ロードマップ)に載っている。

scaffold・ビルド・manifest フィールドリファレンス・署名・ローカルテストまでの完全な手引きは [**フィルタを書く（Writing a filter）**](docs/writing-a-filter.md) にある。契約を vendor 済みで、コピーしてすぐ使える雛形は [`examples/filters/filter-template`](plecto/examples/filters/filter-template)。

## 試す

ツールチェーンと WASM ターゲットは [`plecto/rust-toolchain.toml`](plecto/rust-toolchain.toml) にピン留め
してあるので、[`rustup`](https://rustup.rs/) が初回ビルド時に自動で用意する（ツールチェーン外では一度だけ
`rustup target add wasm32-unknown-unknown`）。

```bash
cd plecto
cargo test --all   # 例フィルタを WASM コンポーネントへコンパイルし、wasmtime ホストにロードして
                    # 契約を end-to-end で検証する
```

例コンポーネントは `plecto:filter/*` のみを import し、WASI・network・filesystem には一切アクセスしない
—— テストはこれで、フィルタが**貸与された能力だけ**に到達し、型付き `decision` が実コンポーネント越しに
往復することを実証する。

### デモを動かす

ユースケース別の自己完結デモが `examples/<name>/` に 9 つあり、どれも**本番ロードパス**（署名＋オフライン OCI レイアウト＋検証＋ロード、fail-closed）を組んで起動時に貼り付け用の `curl` コマンドを表示する。学習パスの完全版は [`examples/README.md`](plecto/examples/README.md)。手早い地図はこちら:

```bash
cd plecto
./examples/try.sh <name>                      # ガイド付きツアー: 起動・curl・後片付けまで自動（または `all`）
cargo run -p plecto-server --example <name>   # 自分で叩くなら直接起動、Ctrl-C で停止
```

| `<name>` | 見せるもの |
| --- | --- |
| `quickstart` | 5 分の hello —— 署名済み WASM フィルタが応答ヘッダを1つ付与。まずここから。 |
| `wasm-auth` | 実用フィルタ —— 署名済み API キー認証、host KV、型付き decision。 |
| `load-balancing` | 3 instance への round-robin、active health check、fail-closed な eject。 |
| `filter-chain` | continue / modify / short-circuit / host-native rate limit を組み合わせる。 |
| `tls-http` | 同一ポートで HTTP/1.1・HTTP/2（ALPN）・HTTP/3 の TLS 終端。 |
| `hot-reload` | SIGHUP による無停止の config swap。壊れた編集は fail-closed のまま。 |
| `canary` | 90/10 の weighted traffic split、header-match routing、SIGHUP drain/promote。 |
| `resilience` | per-try timeout＋retry・circuit breaker・outlier ejection が curl から見える。 |
| `production` | 実 `plecto` バイナリが本物の deploy dir を serve（ターミナル 2 枚）。 |

ベンチマーク・ハーネス（`bench-server` / `swap-bench`）はデモではなく [`bench/harnesses/`](bench/) 配下にあり、[performance](performance/README.md) の数値を生む。

## ロードマップ

Plecto は ADR ファーストで、マイルストーン単位に作る。着地済みの項目・次にやること・決定 ADR まで含めた詳細は [`docs/ROADMAP.md`](docs/ROADMAP.md)（英語）にあり、ここではスナップショットだけ:

| マイルストーン | 状態 | 内容 |
| --- | --- | --- |
| **M0** — 基盤 | ✅ 完了 | `plecto:filter@0.1.0` 契約、wasmtime ホスト、能力境界、CI |
| **M1** — フィルタランタイムの堅牢化 | ✅ 着地 | trusted pool / untrusted fresh-per-request、redb KV、host-native レート制限、quota |
| **M2** — データ経路（fast path） | 🚧 成熟中 | HTTP/1–3 ＋ TLS、routing / LB / resilience、upstream TLS ＋ 定期 DNS 再解決、WebSocket トンネリング |
| **M3** — async & ボディ | 🚧 Stage 1–2 着地 | wasmtime-46 async、header/body world 分割、buffer-then-decide の body hook。`stream<u8>` は実験的 |
| **M4** — provenance & 無停止リロード | ✅ 着地 | OCI ＋ cosign ＋ SBOM のフィルタロード、SIGHUP reload ＋ graceful shutdown、Plecto 自身の署名付き release |
| **M5** — 可観測性 & オプトイン分散 | 🚧 大半着地 | W3C トレース伝播・RED メトリクス・OTLP export は着地、オプトインの設定合意は deferred |
| **M6** — polyglot SDK & リファレンスフィルタ | 🚧 outbound 着地 | SSRF ガード付き outbound HTTP（feature-gated）。Go/JS/Python SDK とリファレンスフィルタは未着手 |

## リポジトリ構成

```
.
├── plecto/                    # Rust workspace（native 側）
│   ├── wit/world.wit          # plecto:filter 契約（contract-first）
│   ├── deny.toml              # cargo-deny サプライチェーン方針（CI ブロッキング）
│   ├── crates/
│   │   ├── host/              # wasmtime 埋め込み: Linker, InstancePre, host-API（+ CONTEXT.md）
│   │   ├── control/           # control plane: manifest, OCI load, chain, reload, TLS/QUIC（+ CONTEXT.md）
│   │   └── server/            # fast path: HTTP/1.1·2（hyper）+ HTTP/3（quinn）, routing, LB, upstream（+ CONTEXT.md）
│   └── examples/              # 動かせるデモ + 例フィルタ guest — 地図は examples/README.md（DX 入口）
│       ├── README.md          # 学習パス（quickstart → リアルなユースケース）
│       ├── <use-case>/        # デモ 9 種: cargo run -p plecto-server --example <name>
│       └── filters/           # 例 plecto:filter guest（独立 workspace・build.rs が component 化）
│           ├── filter-quickstart/ # 最簡スターター（応答に 1 ヘッダ付与）
│           ├── filter-apikey/ # API キー認証ゲート（実用例）
│           ├── filter-hello/  # host 自身の conformance fixture
│           ├── filter-template/ # コピー雛形（WIT を vendor 済み）
│           ├── filter-streaming/ # 実験的 stream<u8> フィルタ（feature-gated）
│           └── filter-extauthz/ # outbound HTTP で ext_authz（feature-gated）
├── bench/                     # ベンチ・ハーネス + runbook（k6/oha; harnesses/, filters/, perf/）
├── performance/              # ベンチ結果の write-up（performance/README.md）
├── docs/ADR/                  # Architecture Decision Records（000001–000049）
├── CHANGELOG.md               # Keep a Changelog + pre-1.0 バージョニング方針
├── CLAUDE.md                  # プロジェクト規約・設計要約
├── CONTEXT-MAP.md             # ドメイン用語集の地図（コンテキスト分割）
└── Dockerfile                 # リファレンスの multi-stage build（distroless runtime）
```

## 設計判断（ADR）

Plecto は重要な設計判断をすべて ADR に、Fork 形式（*判断 / 根拠 / 再検討条件*）で記録している。49 本（accepted 48・proposed 1）すべては [`docs/ADR/`](docs/ADR/) にあり、起点は [ADR 000001](docs/ADR/000001.md)（相補的な二つの構成要素）。各 ADR は土台にした判断へ相互リンクしている。

## コントリビュート

コントリビュートは deliberate に扱う: **PR を出す前に issue か [Discussion](https://github.com/Kaikei-e/Plecto/discussions) で方針を合意してほしい**（事前合意のない PR は close されることがある）。Plecto は outside-in TDD（E2E → WIT-conformance → unit）に従い、load-bearing な判断を ADR に記録する。完全な手引き（注意を要する領域・DCO sign-off を含む）は [CONTRIBUTING.md](CONTRIBUTING.md)（規約は [CLAUDE.md](CLAUDE.md)）参照。PR 前のローカル CI パリティ:

```bash
cd plecto
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```

（リポジトリ root からは `just check` でも可。）

## ライセンス

**Apache License, Version 2.0** — [LICENSE](LICENSE) を参照。Apache-2.0 の特許付与条項はインフラ・プロジェクトに適し、Envoy・Linkerd・containerd でも採用されている。

## 先行研究 & 謝辞

Plecto は [Envoy](https://www.envoyproxy.io/) / [proxy-wasm](https://github.com/proxy-wasm)、[Cloudflare Pingora](https://github.com/cloudflare/pingora)、[Bytecode Alliance](https://bytecodealliance.org/)（[wasmtime](https://wasmtime.dev/)、[WIT と Component Model](https://component-model.bytecodealliance.org/)）の肩の上に立っている。
