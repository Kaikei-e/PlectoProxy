# Plecto Proxy 原典 — デザイン原則・設計指針・アーキテクチャ方針

[English](design-principles.md) · 日本語

> **策定日**: 2026-07-06（[ADR 000056](ADR/000056.md) と同日に docs/ へ採録）
> **根拠**: `github.com/Kaikei-e/PlectoProxy` を本日 fresh clone（キャッシュなし）し、HEAD `c5274db61a9dc10775200dd296e2dd3d4e0725e2`（最新タグ `v0.1.3`）の一次情報——WIT 契約原文（`plecto/wit/world.wit`）・ADR 全55本（accepted 54 / proposed 1 相当）・`CLAUDE.md`・`CONTEXT-MAP.md`・各 crate の `CONTEXT.md`・`docs/ROADMAP.md`・`docs/hardening.ja.md`・`performance/README.md`・README——から直接策定した。
> **性格**: 本書は Plecto Proxy の設計思想を「原則（変わらないもの）」「方針（構造の選び方）」「指針（日々の判断の当て方）」の三層で一冊に定礎する原典である。個別判断の一次記録は `docs/ADR/` にあり、契約の正文は `wit/` にある。本書と ADR/WIT が食い違う場合は ADR/WIT を正とし、本書を改訂する（第7章）。英語版 [design-principles.md](design-principles.md) と同期して保守する。

---

## 第0章 使命と価値序列

**Plecto Proxy は、セルフホスト可能・プログラマブルな L7 リバースプロキシ / API ゲートウェイである。** 相補関係にある二つの構成要素——接続受付・TLS 終端・HTTP/1.1/2/3・ルーティング・LB・upstream 管理を担う **fast path**（native Rust）と、各リクエストの*判断*（認証・書換・rate limit・WAF・ポリシー）を担う **extension plane**（WASM Component Model フィルタ）——を、型付き WIT 契約で結ぶ。速度が要となる経路は native Rust のまま、リクエストのロジックはサンドボックス化された WASM コンポーネントとして走り、**ホストが明示的に貸した能力以外には何も触れられない。それを強制するのは規約ではなくサンドボックスである。**

すべての設計判断は、`CLAUDE.md` が定める次の価値序列に従属する。

**安全 × ポータビリティ × セルフホスト性 × 運用の単純さ ＞ 機能網羅性 × 強い権限 × 分散デフォルト**

左辺と右辺が衝突したとき、Plecto Proxy は常に左辺を選ぶ。この一行が、以降のすべての原則・方針・非目標の親である。使命の対象は「自分で運用し、トラフィックも秘密も自分のインフラに留めたいチーム」であり、**データ主権（data sovereignty）を第一原理**とする。到達品質の物差しは「SaaS 企業のプロダクション導入に耐える水準」——セキュリティ審査票に空欄なく答えられ、複数レプリカ運用の意味論が文書化され、供給網が検証可能であること——であり、金融・政府調達水準（FIPS 140-3 等の形式的認証）は非目標である（ADR 000054）。

---

## 第1章 デザイン原則 — 変わらないもの

以下の原則は Plecto Proxy の同一性を構成する。これらを変える提案は「Plecto Proxy の改良」ではなく「別プロジェクトの提案」として扱い、原則の改訂には必ず ADR を要する。

### P1 — 四条件の同時充足が存在理由である

ゲートウェイのカスタムロジック配置における従来の三択——設定/DSL（表現力に天井）、本体再コンパイル組込（untrusted 不可・言語固定・全体巻き込み）、別プロセス呼出（毎リクエスト往復で遅い）——は、**「プロセス内の速さ」「サンドボックスの安全」「言語の自由」「無停止差し替え」を同時に満たせない**。この四条件を同時に満たす事実上唯一の技術が WASM であり、Plecto Proxy はこの一点の上に立つ（ADR 000001）。「データプレーンのフィルタは WASM」という洞察は Envoy/proxy-wasm が約10年かけて実証した結論であり、Plecto Proxy はそれに多くを負うことを明記しつつ、proxy-wasm が到達しなかった Component Model / WIT の型付き・多言語・合成可能な基盤の上に**ネイティブに**築く。四条件のいずれかを恒常的に犠牲にする設計は、原則違反である。

### P2 — 相補関係にある二つの構成要素を、型契約で結ぶ

fast path と extension plane は上下関係でも主従関係でもなく、**相補関係（two halves）**にある。両者の境界は暗黙の慣習ではなく `plecto:filter` WIT ワールドという**型付き契約**であり、契約が境界のすべてである。fast path はチェーンを駆動する側、extension plane は駆動される側という向きだけが固定される。用語規律として「core / engine / data plane / plugin layer / middleware layer」といった曖昧語・多義語は避け、`CONTEXT-MAP.md` の統制語彙（fast path / extension plane / two halves）を用いる。

### P3 — 判断は型で運ぶ。能力の不在も型で表す

フィルタの戻り値は裸のフラグやブール値ではなく、常に WIT の `variant` である: request 側は `continue` / `modified(request-edit)` / `short-circuit(http-response)` の三値、response 側は `continue` / `modified(response-edit)`。WIT 原文のコメントにある通り **"Never a bare flag."**（Tenet 3）。

この原則は「存在」だけでなく**「不在」にも及ぶ**。契約は header-only の `filter` world と body-reading の `filter-body` world に分かれており、`on-request-body` export の**不在そのもの**が「このフィルタはボディを読まない」という機械検証可能な事実となり、ホストはそれを根拠にボディのバッファリングを丸ごとスキップする（zero-copy passthrough、ADR 000005 / 000025 / 000038）。性能最適化を運用者の注意力ではなく**契約の形から導出する**——これが Plecto Proxy の型設計の核心である。

### P4 — deny-by-default、そして fail-closed

フィルタが import できる能力は、ホストが明示的に貸した interface のみ: **host-log / host-clock / host-kv / host-counter / host-ratelimit の5つ**であり、それ以外の import は Linker に存在しない（ADR 000006）。WASI import はゼロが既定（zero-WASI guest、Tier A）で、追加の宣言なしに deny-by-default の Linker が instantiate できる形である（ADR 000055）。ランタイムが最小限の WASI 存在を前提とする fat guest（TinyGo/Go、Tier B）には、固定・最小・off-by-default のスライス——`wasi:io` / `clocks` / `random` / `cli`、および一部ランタイムの起動処理が無条件に import する空の `wasi:filesystem`——を貸せる。fs アクセスも sockets も一切なく、host のビルドとフィルタの manifest 宣言の両方が opt-in したときのみ有効（ADR 000063）。どちらか片方でも欠ければ instantiate に失敗する deny-by-default のままである。

失敗は常に閉じる側に倒す: フィルタの trap / epoch deadline 超過は素通り（fail-open）させない。unhealthy な upstream しか無ければ 503。quota 超過は拒否。バケット状態の破損は「全許可」ではなく「空＝deny + self-heal」。署名の欠落・不一致はロード前に Err。path 正規化で解釈できない迂回表現は拒否。バッファ permit のエラーも fail-closed（直近の `dfab595` に至るまで一貫）。**「安全側に倒すか迷ったら、既に答えは出ている」**が本原則の運用形である。

### P5 — 検証してからロードする。同じ規律を自分にも課す

フィルタは OCI artifact として content digest（sha256）で pin し、cosign 署名 + SBOM をロード時に検証する。SBOM は in-toto subject-digest で対象 component に束縛され、「有効署名だが無関係な SBOM」を弾く。`load` は署名済みアーティファクトしか受け取らず、raw bytes の迂回経路は構造的に存在しない。空の trust policy は「何もロードしない」であり、"allow unsigned" の escape hatch は production API に無い。

そして**フィルタに課す供給網規律は、Plecto Proxy 自身の配布物にも適用する**（ADR 000047）: `cargo-auditable` ビルド、syft による SBOM、cosign keyless 署名がタグ付きリリースごとに出荷され、CI のツールチェーンは sha256 pin、依存は `deny.toml`（cargo-deny）が CI ブロッキングで統制する。「あなたのコードが、信頼されて、ゲートウェイに載る」という約束は、自らの供給網が同じ検証に耐えて初めて成立する。

### P6 — フィルタはステートレス。状態はホストが貸し、状態はノードローカルである

「ステートレス」は精密に定義される: 禁じられるのは**可変業務状態**のインスタンス内保持であり、**不変の init 派生物**（コンパイル済み regex・構築済みスキーマ等）の常駐は許され、むしろ推奨される（ADR 000011、Tenet 4: 重い初期化は `init` フックへ、ホット経路は軽く保つ）。可変状態は host-kv（redb バック、フィルタ identity で名前空間化——他フィルタの keyspace には偽造不能に到達できない）・host-counter・host-ratelimit を通じてホストに置く。

そしてホスト保持の状態は**すべてノードローカル**である。これは実装の未熟ではなく**宣言された意味論**であり（ADR 000053）、「実効レート = 設定値 × レプリカ数 N」という帰結ごと hardening ガイドに文書化される。真にグローバルな共有状態が要る場合の受け皿は native ではなく extension plane（Fork 6: user-policy はフィルタへ）——Envoy ですら分散レートリミットを外部サービスへ外出ししているという業界の確立形に整合する配置である。

### P7 — 単一ノード・ファースト。設定は宣言的・静的、変更は無停止 reload

分散はオプトインのレイヤであって既定ではない（ADR 000008）。設定は単一の宣言的マニフェスト——フィルタを OCI digest で pin し、trust root・チェーン順・route・upstream を静的に宣言する「何がロードされているか」の source of truth——であり、xDS 的な動的 config push は採らない。変更は SIGHUP 起点の無停止 reload で、content-hash 整合・atomic な `ArcSwap`・all-or-nothing。trust root は構築時固定で reload では変えない。`plecto validate` は artifact を読まずに同じ fail-closed 検証を CI・reload 前ゲートとして提供する（ADR 000046）。

### P8 — 成熟は役割駆動。やらないことは「declined」として言語化する

機能追加の駆動軸は「競合が持っているから」ではなく「L7/API-gateway という**役割**が要求するから」である（ADR 000029）。同 ADR は native/WASM の配置基準を固定し、以後の判断（native rate-limit の床は fast path へ、WAF は extension plane へ、分散状態は外へ）はこの基準から導出されている。

やらないと決めたことは黙って放置せず、**declined として ADR に言語化する**。現に response caching・AI/LLM gateway の native 化（ADR 000043）、WAF の native 実装(ADR 000037)、native 分散状態（ADR 000053）、h2c（ADR 000015）、0-RTT（ADR 000052）が明示的 declined として記録されている。deferred（時機待ち）とは区別し、deferred には序列を付けて管理する（現行序列は ADR 000054 が mTLS を先頭に引き上げ、ADR 000056 が PROXY protocol v2 を直後に挿入）。

### P9 — 測定は誠実に。リーダーボードではなく方法論

`performance/README.md` の冒頭宣言が正典である: 目標は **"transparency about method, not a leaderboard"**。すべての数値は内部の **regression baseline** であって、容量ガイドでも他プロキシとの比較でもない。絶対値はホストとジェネレータに縛られるため、読むべきは**比・曲線の形・時定数**という相対シグナルである。core pinning でジェネレータとプロキシを分離し、warm-up を除外し、closed-loop / open-loop を区別し、ジェネレータごとの天井差を明記する（数値はセクション内・同一ジェネレータ間でのみ比較可能）。フィルタプレーンのコストは µs/req で語り、ホスト依存のパーセント表記を避ける。

### P10 — ADR-first。ただし証拠が変われば同日でも撤回する

大きな判断は実装に先立って `docs/ADR/NNNNNN.md` に書く（6桁ゼロ埋め・frontmatter・wikilink 相互参照）。ADR は装飾ではなく運用されている規律であり、その証明が ADR 000051 である: TLS crypto provider について一度「cmake 依存ゆえ aws-lc-rs は declined、ring を既定」と決定した後、`cargo tree` の実地検証で「sigstore 経由で既に aws-lc-rs がリンクされている」と前提が崩れたことが判明し、**同日中に撤回・aws-lc-rs 一本化へ再決定**した。規律の本体は「決めたから守る」ではなく「**証拠に従う**」であり、撤回は恥ではなく規律の作動である。TDD は outside-in（E2E → WIT-conformance → Unit）、RED と GREEN は別コミット。セキュリティ性質は主張ではなく**反証可能なテスト**で固定する。

### P11 — データプレーンで panic しない

untrusted な入力が worker を巻き込んで落とすことを許さない。フィルタの trap は circuit breaker（連続 trap 閾値 → 503 cooldown → half-open）で隔離し、プール枯渇は有界待ちの後 fail-closed（ADR 000012）、リソースは epoch 計量・メモリ/テーブル上限・per-filter quota で有界化する。「落ちない」ことと「fail-open しない」こと（P4）は両立させる——落とさず、かつ通さない。

### P12 — 言葉を統制する

`CONTEXT-MAP.md` と各 crate の `CONTEXT.md` は、コンテキストごとの統制語彙と **_Avoid_ 語彙**（使わない言い換え）を定義する用語集である。route を rule と呼ばない、filter を plugin と呼ばない、upstream を origin と呼ばない。用語集には実装詳細・仕様・決定を置かない（判断は ADR へ、契約は WIT へ、規約は CLAUDE.md へ）という**文書の役割分離**自体も原則である。記述言語はバイリンガル: ドキュメント散文は日本語、コード・コマンド・ライブラリ名・WIT・識別子は英語。

---

## 第2章 アーキテクチャ方針 — 構造の選び方

### 2.1 三つの境界づけられた文脈

Plecto Proxy の Rust workspace は三つの crate = 三つの文脈から成り、各文脈は自分の `CONTEXT.md` を持つ。

| 文脈 | crate | 責務 |
|---|---|---|
| **Fast path** | `plecto-server` | 接続受付・TLS 終端・HTTP/1.1/2/3・route 照合・chain 駆動・upstream 転送（ADR 000013） |
| **Extension plane / host runtime** | `plecto-host` | wasmtime 埋め込みホスト。`plecto:filter` 契約の執行・フィルタ実行モデル・能力境界（host-API） |
| **Control** | `plecto-control` | 宣言的マニフェスト・provenance ゲート経由のロード・無停止 reload・config version。「何がロードされ、いつ差し替わるか」 |

関係は三本: **Fast path → Extension plane**（per-request に chain を駆動）、**Control → Extension plane**（manifest が filter を digest pin し chain 順と trust root を宣言、reload が atomic に差し替え）、**Control → Fast path**（manifest が route と転送先を宣言し、fast path は per-request に `ConfigSnapshot` を取って route を選ぶ）。契約 `wit/` は workspace 直下に置かれ、どの crate にも属さない——契約は文脈間の共有財であって、どれかの所有物ではない。

### 2.2 契約アーキテクチャ（`plecto:filter@0.1.0`）

契約は独自ワールドとして定義し、確定方向として `wasi:http`（proxy / middleware）への型収斂を M3 で行う（ADR 000002 / 000020）。deny-by-default は型語彙と独立に維持される。現行契約の構造:

- **types**: `http-request`（header-only）・`http-response`・`request-edit` / `response-edit`（書換は差分で表現）・三種の decision variant。
- **host-API（5能力・1 interface = 1 capability）**: `host-log`（レベル付きログ）/ `host-clock`（**リクエスト開始時に一度だけ捕捉した wall-clock スナップショット**を返す——同一リクエスト内の反復呼出は同値で、TTL・rate-limit ロジックを決定的にする）/ `host-kv`（フィルタ identity で名前空間化された可変業務状態）/ `host-counter`（`wasi:keyvalue/atomics` と同形の atomic counter——多言語フィルタが既知の契約形に出会うための意図的な形合わせ）/ `host-ratelimit`（token bucket は **host-native** に留まり、refill と計数は WASM 境界を越えない。バケット仕様は manifest でホスト側が定め、フィルタは自分の limiter を自己申告で骨抜きにできない。バケット未設定は deny、backend エラーも deny）。
- **二つの world**: `filter`（header-only）と `filter-body`（+ `on-request-body`、buffer-then-decide、v1 は `list<u8>`）。`include` ではなく敢えて重複記述しているのは WIT の `use` 伝播仕様への対処であり、コメントで理由が明記される——**契約ファイル自身が設計判断の注釈を持つ**のがこのリポジトリの流儀である。
- **実験系**: `plecto:filter-streaming`（`stream<u8>`・async）は off-by-default の `streaming-body` feature に隔離され、`wasm32-wasip3` の Tier-2 到達まで既定ビルドに入らない。

契約進化の方針: 変更は additive を基本とし、body の真のストリーミング化は `list<u8>` → `stream<u8>` の差し替えとして契約に席を確保済み。ホット経路（rate limit の refill 等）は契約の外＝native に落とす——「WASM 税は判断ロジックにのみ払う」。

### 2.3 実行モデル: 信頼で分岐するライフサイクル

「ステートレス」の精密化（P6）から、インスタンス・ライフサイクルの二分岐が**必然として**導かれる（ADR 000011 / 000012）:

- **trusted**: 固定容量・遅延充填のインスタンスプールから per-request に checkout・再利用（init は一度きり、init 派生物は常駐）。枯渇は有界待ち後 fail-closed。プール全体の circuit breaker と recycle-after-N で状態蓄積と故障を有界化。
- **untrusted**: fresh-per-request instantiation。linear memory は**構成上フレッシュ**（zeroize という能動操作ではなく fresh-by-construction）。CVE-2022-39393（pooling + memory-init-cow でのスロット再利用リーク、wasmtime 2.0.2 で修正済み）の教訓を、修正済みでもなお defense-in-depth として設計に刻む。

実行時の有界化は多層である: epoch interruption（CPU 予算。fuel より軽量という wasmtime 公式の報告に基づく選択。wall-clock SLA ではないため、ブロックする host call には別機構の host-timeout を重ねる二層設計）、`StoreLimits` によるメモリ上限、table 上限、per-filter + host 全体の状態 quota（超過 fail-closed）、untrusted の init deadline 締め付け（ADR 000027）。sync な chain は `spawn_blocking` で tokio の fast path に橋渡しされ（ADR 000013）、wasmtime 46 以降はホスト側が `call_async`（fiber）で guest hook を走らせる（M3 Stage 1）。

### 2.4 fast path 方針: 照合は決定的に、耐障害は層別に

**ルーティング**は host・path-prefix・method・header（exact）・query（exact）の多軸 AND 照合で、複数一致時は specificity 順（host 指定 > 最長 path prefix > method > header 一致数 > query 一致数 > manifest 出現順）に**決定的に** 1 本を選ぶ。一致なしは 404。route は inline chain・strip_prefix・rate limit を単位として持ち、転送先は単一 upstream または weighted backends（traffic split / canary の正準プリミティブ。`weight 0` = drain）。**path は ingress で一度だけ正規化し、encode された separator / dot-escape による迂回を fail-closed で拒否する——これにより per-route フィルタが信頼できる認証境界になる**（ADR 000027）。これが fast path の設計上最も重要な安全性主張である。

**LB と耐障害**は関心事を分離して層別に積む: instance 選択（round-robin / weighted least-request P2C / weighted Maglev consistent hashing、ADR 000035。RR カーソルは reload を跨いで引き継ぐ、ADR 000024）→ active/passive health check（悲観的スタート、全滅時 503 fail-closed）→ outlier detection（health 状態機から独立した第三の軸、ADR 000032）→ per-upstream circuit breaker（health とは別関心事の concurrency cap、ADR 000028）→ 二段 timeout（per-try + overall、超過は fail-closed 504、ADR 000031）→ bounded retry（jittered exponential backoff・冪等/bodyless 限定から retriable-5xx まで、別 healthy instance へ、ADR 000023 / 000030）→ native L7 rate-limit の床（route / client-IP 粗粒度、filter に貸す host-ratelimit とは別物、ADR 000033）。**「health」「outlier」「breaker」を混ぜない**——それぞれが独立した信号と独立した回復経路を持つことが、この層構造の設計原理である。

**プロトコル方針**: HTTP/2 は TLS+ALPN 上で終端し h2c は採らない（ADR 000015）。HTTP/3 は quinn+h3 の独立 UDP listener で終端し Alt-Svc で広告、0-RTT は拒否（ADR 000016 / 000052）。upstream への再暗号化は TLS+ALPN（HTTP/2 優先・`TE: trailers` 通過で gRPC が end-to-end、custom CA、IP エンドポイント向け `sni` 検証名 override、ADR 000042 / 000050）。hostname upstream は定期再解決で各 A/AAAA レコードを LB エンドポイントに展開しコンテナ再作成に追従（ADR 000044）。WebSocket は per-route の Upgrade token allowlist（`h2c` は validation で拒否）+ activity-based idle timeout のトンネル（ADR 000048）。クライアント IP は edge モデル——受信 `X-Forwarded-*` を剥がし、実 peer から付け直す（ADR 000018 / 000022）。フィルタが触れなかったヘッダのバイト列は byte-for-byte で通す（ヘッダ・バイト等価）。

### 2.5 TLS・暗号方針

crypto provider は **aws-lc-rs に一本化**（ADR 000051。cmake declined の判断を実地検証に基づき撤回した経緯ごと記録）。post-quantum の X25519MLKEM768 鍵交換を既定で優先。TLS 1.3 の stateless session resumption はチケット鍵ローテーション付きで導入し、**0-RTT 拒否とチケット鍵のノードローカル性を不変条件**とする（ADR 000052——2025 年に相次いだ ticket-key 共有起因の脆弱性を教訓とした線引き）。証明書は宣言的 manifest の静的ファイル管理（ADR 000014）。mTLS は品質ターゲット再定義に伴い deferred 序列の先頭（ADR 000054）。

### 2.6 観測性方針: ホストが伝播し、ゲスト契約は汚さない

W3C trace context は inbound `traceparent` をプロキシ通過後も**継続**し（新規 root を切らない）、フィルタ実行ごとに 1 span を OpenTelemetry データモデルで張る。span state の管理は**ホスト側**の責務であり（ADR 000009）、OTLP のネットワーク export はホスト側 export pump（batch/retry/flush）が担う——**no-tokio のフィルタ境界を観測性のために破らない**（ADR 000040）。`wasi-otel` の guest 契約化は M3 以降に据え置き。RED metrics はホスト集約。

### 2.7 状態バックエンド方針

host state backend は単一の `[state]` 設定に束ね、production 経路は redb（単一プロセス設計の embedded KV、ADR 000041）。durability は用途で分ける: durable KV の書込は `Immediate`、ephemeral な hot 状態（counter / bucket）は `Durability::None` で fsync を省き、atomicity は単一 write txn で維持する。**「永続性の強度は一律ではなく、状態の意味に従って選ぶ」**が方針であり、破損時の挙動は常に deny + self-heal（P4）。

---

## 第3章 設計指針 — 日々の判断の当て方

### 3.1 新しい機能はどこに置くか（配置決定木）

ADR 000029 の役割駆動基準と Fork 6 から、配置は次の順で問う:

1. **それは全リクエストが通る共通の床か？**（rate-limit の床・path 正規化・inbound 上限のような、テナント非依存の防御）→ **native / fast path**。ただし粗粒度に留め、ポリシーの表現力は求めない。
2. **それは利用者ごとに異なる判断（user-policy）か？**（認証・WAF ルール・PII マスク・独自ロジック）→ **extension plane（WASM フィルタ）**。native に持ち込まない（WAF native は declined、ADR 000037）。
3. **それは共有状態を要するか？** → native には置かない。フィルタが `outbound-http` capability（deny-by-default + per-filter allowlist + IP-pin SSRF ガード、ADR 000036）経由で外部ストアに委譲する形で表現する（ADR 000053）。
4. **それはホット経路の計数・refill か？** → 契約の外＝host-native に落とし、フィルタには「参照する判断」だけを残す（host-ratelimit の設計と同型）。
5. **どれでもないか？** → 非目標リスト（第4章）に照らす。該当すれば declined ADR を書く。

### 3.2 能力（capability）を増やすときの規律

新しい host-API は「1 interface = 1 capability」で切り、deny-by-default を維持する。危険な能力は **off-by-default の feature gate に隔離してから**入れる——現行の実例は `outbound-http`（wasi:http 収斂ゲートまで既定ビルド外）、`streaming-body`（wasip3 Tier-2 まで）、`polyglot-conformance`（既定 `cargo test` に影響しない）、`fat-guest`（Tier B guest 向け最小 WASI 貸与、ADR 000063——既定 off、on でもフィルタの manifest が `wasi = "minimal"` を宣言しない限り不活性）。フィルタが自分に課された制約を自己申告で緩められる形（バケット容量の guest 指定等）は設計段階で禁じる。

### 3.3 依存を増やすときの規律

依存追加は cargo-deny（`deny.toml`、CI ブロッキング）を通ること、ビルドに cmake 級の外部ツールチェーンを持ち込まないこと（ADR 000051 の攻防で確立——ただし同 ADR は「実際に何がリンクされているかを `cargo tree` で確かめてから決める」ことも教える）、CI のツールチェーンは sha256 pin すること。crypto のような高リスク依存は default-features を絞る（sigstore はオフライン keyed verify のみに限定した前例）。

### 3.4 主張は反証可能にしてから外に出す

README や docs に書く強み・性能・対応言語は、テストか計測で反証可能な形にしてから主張する。正典例は ADR 000055: 「任意言語で書ける（polyglot）」という掲示が Rust 実例しか持たない aspirational な主張だったことを認め、MoonBit / JS / C の zero-WASI 例フィルタを**単一の共有アサーション・スイート**（`tests/polyglot.rs`）で CI 検証する形に置き換え、コミットメッセージ自体が "replace the aspirational polyglot claim with the verified per-language status" と記録した。同様に、セキュリティ性質（署名ゲート迂回不能・fail-closed・quota）は E2E テストで固定し、性能主張は regression baseline として計測手順ごと開示する（P9）。**検証できない主張は、削るか、検証を先に作る。**

### 3.5 プロセス規約（要点）

ADR は `docs/ADR/NNNNNN.md`、frontmatter + wikilink、テンプレは `template.md`。TDD は outside-in（E2E → WIT-conformance → Unit）で、RED と GREEN は別コミット（直近の striped-lock 修正 `3648508`→`486e6cf` がその実演）。仕上げに fmt / clippy（`-D warnings`）/ type / test のローカル CI パリティを必ず回す。ドキュメントの役割分離を守る: 用語は CONTEXT.md、判断は ADR、契約は WIT、規約は CLAUDE.md、運用指針は hardening ガイド、計測は performance/README。

---

## 第4章 非目標 — 意図して作らないもの

以下は怠慢ではなく**判断**であり、各項に根拠 ADR がある。解除には新しい ADR を要する。

| 非目標 | 根拠 | 備考 |
|---|---|---|
| 汎用コンピュート・プラットフォーム化（長命ステートフル実行基盤） | 創設判断 | フィルタはステートレス（P6）。スコープ肥大の警告例に学ぶ |
| Envoy 全機能クローン | ADR 000029 | 成熟は役割駆動。機能数で張り合わない |
| native な分散状態（gossip / 中央カウンタ / 共有ストア依存） | ADR 000053 | 共有状態は Fork 6 で extension plane へ |
| WAF の native 実装 | ADR 000037 | user-policy は extension plane へ |
| response caching / AI・LLM gateway の native 化 | ADR 000043 | 役割宣言の外 |
| legacy proxy-wasm ABI 互換 | ADR 000001 | Component Model ネイティブが存在意義 |
| xDS 的な動的 config push | ADR 000008 | 宣言的静的設定 + 無停止 reload（P7） |
| h2c（平文 HTTP/2） | ADR 000015 | Upgrade allowlist でも validation 拒否 |
| TLS 0-RTT | ADR 000052 | リプレイ面を持ち込まない。不変条件 |
| FIPS 140-3 等の形式的コンプライアンス認証 | ADR 000054 | 品質ターゲットは SaaS 導入水準 |
| マネージド SaaS 前提の設計 | 創設判断 | self-host / データ主権が第一原理（第0章） |

---

## 第5章 進化の条件 — 何が起きたら、何を再検討するか

原則は変えないが、方針には外部トリガで開く再検討点が明示されている。

| トリガ | 再検討対象 |
|---|---|
| `wasm32-wasip3` が Rust Tier-2 到達・wit-bindgen async 成熟 | `streaming-body` feature の既定化に向けた昇格判断（`stream<u8>` 本実装、M3 の真のストリーミング増分） |
| `wasi:http`（proxy / middleware）収斂ゲートの成立 | 型語彙の `wasi:http` 収斂実施（ADR 000020）と、`outbound-http` の既定ビルド入り判断 |
| Go（`gc`）本体が wasip2/p3 で Tier 相当の Component Model 対応に到達 | Tier B（ADR 000063、2026-07-06 判断・2026-07-08 実装）が前提とする TinyGo 限定の再訪 |
| mTLS スライス着手 | downstream / upstream の client cert 検証設計（deferred 序列の先頭、ADR 000054。現状は両向き `with_no_client_auth`） |
| リモート filter-registry 取得（wkg 境界）の需要成立 | M4 の残余——現行はオフライン image-layout が意図された既定 |
| crypto provider の代替（例: 第三者監査済み pure-Rust 実装）の成熟 | ADR 000051 の再訪。判断基準は同 ADR が確立した「実リンク検証 + ビルド DX + 保守状況」 |
| opt-in 分散合意（foca / openraft）の実需 | M5 deferred 分の着手可否。single-node first（P7）は維持したまま opt-in レイヤとして |

再検討は常に「トリガ → 個別 ADR → 実装」の順で行い、feature gate の既定化・deferred の繰上げを ADR なしに行わない。

---

## 第6章 一枚要約 — 判断に迷ったら

1. **価値序列**: 安全 × ポータビリティ × セルフホスト性 × 運用の単純さが、機能・権限・分散に勝つ。
2. **境界は契約**: fast path と extension plane の間にあるのは WIT の型だけ。判断は variant で、不在も型で。
3. **既定は拒否**: import も、失敗も、未署名も、未設定バケットも、全滅 upstream も——閉じる側に倒す。
4. **状態はホストに、ノードに**: フィルタは可変状態を持たず、ホスト状態は node-local。共有が要るならフィルタから外へ。
5. **床は native、ポリシーは WASM**: 全員が通る粗い防御は fast path、利用者ごとの判断は extension plane。
6. **主張は反証可能に**: テストか計測が付かない強みは、まだ強みではない。
7. **書いてから作り、証拠で覆す**: ADR-first。撤回は規律の作動であって失敗ではない。

---

## 第7章 正文性と改訂手続き

本書は原典（founding-level document）だが、**一次情報の序列では ADR と WIT の下位**に立つ。序列は次の通り: (1) `wit/`（契約の正文）、(2) `docs/ADR/`（判断の一次記録）、(3) 本書（原則・方針の結晶化）、(4) CLAUDE.md / CONTEXT-MAP / hardening / performance（各領域の運用正文）。上位と食い違いが見つかった場合、本書側を改訂する。

改訂手続き: 第1章（原則）の変更は必ず ADR を先行させる。第2章（方針）・第4章（非目標）・第5章（進化の条件）は、対応する ADR の accept / decline に追随して更新する。第3章（指針）はプロセス改善として CLAUDE.md と同期して更新できる。いずれの改訂でも、根拠となる HEAD のコミットハッシュを本書冒頭に記録し直すこと——**本書自身が、P10（証拠に従う）と §3.4（反証可能性）の適用対象である。**
