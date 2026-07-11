# Control — 用語集

「何がロードされ、いつ差し替わり、どの単位で合意するか」を司る control plane（`plecto-control`）のコンテキスト。
全体像と他コンテキストとの関係は [../../../CONTEXT-MAP.md](../../../CONTEXT-MAP.md)。本ファイルは用語集であり、
実装詳細・仕様・決定の置き場ではない（設計判断は `CLAUDE.md` と `docs/ADR/`、契約は `wit/` を参照）。

## 制御面

**Control plane**:
宣言的マニフェストの読込・filter のロード（provenance ゲート経由）・chain 駆動・無停止 reload を担う層。
fast path / extension plane の外側に立ち、「何がロードされ、いつ差し替わるか」を司る。
_Avoid_: config layer（曖昧）, control loop（別概念）

**Single-node-first（単一ノード・ファースト）**:
状態を各ノードローカル（host KV）に置き、状態共有を分散の前提にしない既定の姿勢。分散は opt-in レイヤであって
標準ではない。
_Avoid_: standalone（孤立の含意）, embedded（別軸）

## 配布・設定

**Manifest（宣言的マニフェスト）**:
フィルタを OCI digest で pin し、信頼鍵・チェーン順を宣言する単一の静的設定。「何がロードされているか」の
source of truth。
_Avoid_: config（曖昧）, descriptor

**Content pin（digest 固定）**:
フィルタを OCI content digest（sha256）で固定し、再現性とサプライチェーン整合を担保すること。署名
（authenticity）とは別レイヤの integrity。
_Avoid_: version tag（タグは非固定で再現性が壊れる）

**Config version**:
マニフェストの意味的な content-hash——表記（コメント・空白・キー順・明示デフォルト）に依らず、設定の*意味*の
同一性を表す単一の値。reload の冪等判定単位であり、監査単位であり、将来の分散合意（config consensus）が合意する単位。
_Avoid_: config hash（生テキストのハッシュと混同）, revision（連番の含意）

**Hot-reload（無停止リロード）**:
新しいフィルタ集合を並行生成し、アトミックに切替え、旧集合を drain する設定差し替え。
_Avoid_: restart, hot restart（プロセス再起動を含意）

**Reload trigger（reload 契機）**:
「マニフェストを今読み直せ」だけを伝える、内容を持たない合図。設定そのものを運ぶのではない。第一の契機は SIGHUP
（operator が編集済みマニフェストの取り込みを明示 push する慣習）。
_Avoid_: config push（設定を運ぶ含意で、xDS 的動的 push と混同）

**Dev key（開発鍵）**:
`plecto dev` のインナーループ専用、プロジェクトローカル・永続な ECDSA-P256 署名鍵（`.plecto/dev-key`）。本番の
運用者管理鍵とも、テスト専用の使い捨て `TestSigner` とも別物——検証経路は本番と同一コードのまま、trust root の
中身だけを差し替える二相化（ADR 000065）。dev manifest の `[trust]` にのみ公開鍵を注入し、本番 manifest には触れない。
_Avoid_: test key（TestSigner と混同——dev key は永続する）, signing key（本番運用鍵と区別が付かない）

## TLS 終端

**Stateless resumption（ステートレス再開）**:
自己暗号化されたセッションチケットだけで TLS セッションを再開する方式。サーバはセッションごとの状態を
一切持たず、チケットを封緘する鍵だけを保持する（ADR 000052）。0-RTT（early data）は別概念で、Plecto では
常に拒否。
_Avoid_: session cache（サーバ側にセッション状態を残す stateful 方式の含意）, 0-RTT（再開とは別の概念）

**チケット鍵（STEK / session ticket encryption key）**:
セッションチケットを封緘する鍵。既定はノードローカル・プロセス寿命（定期ローテーション、ディスクにも
manifest にも置かれない、reload では失効しない）。`[resumption]` で共有 STEK モードにオプトインできる
（ADR 000062）。
_Avoid_: cluster key（無条件共有の含意——共有は証明書束縛つきオプトインのみ）

**共有 STEK（shared STEK mode）**:
`[resumption] stek_file` によるオプトイン。全レプリカが同一の 64 バイト鍵素材ファイルから同一のチケット鍵を
決定的に導出し、前段 LB がラウンドロビンしてもチケットがレプリカ横断で再開する（ADR 000062）。ローテーション
は外部（operator）責務で、`max_age_hours` 超過・ファイル異常時は full handshake へ fail-closed 縮退する。
_Avoid_: key distribution（Plecto が鍵を配る含意——配布機構は持たない、ファイルが唯一の界面）

**証明書束縛（cert binding）**:
共有 STEK の導出鍵を、その配備が提供する cert セット（SPKI fingerprint の集合）に HKDF の info で暗号学的に
束縛すること。鍵ファイルを共有していても cert セットが異なる配備間ではチケットが越境しない（ADR 000062 (a)、
CVE-2025-23419 / 23048 の越境形の構造的遮断）。
_Avoid_: vhost isolation（設定による分離の含意——ここでは鍵導出そのものが分離する）

**Client auth（downstream mTLS / `[listen.client_auth]`）**:
listener が終端する全 TLS handshake（TCP と QUIC の両面）で、`ca_path` の trust anchor に連鎖する
client certificate を**必須**にする検証（ADR 000078）。required のみ——「要求するが未提示も通す」
optional は identity の filter 伝搬（declared deferred）とセットでなければ導入しない。共有 STEK との
併用は fail-closed（ADR 000062 (b)）。失効確認（CRL/OCSP）は本スライス外。per-node resumption は
維持するが、resume では CertificateRequest が再送されず ticket に保存された chain を復元するだけなので、
**ticket 有効期間内は証明書の期限・失効の再検証が走らない**（shared STEK 禁止はこのギャップを消さない）。
_Avoid_: mTLS listener 単体の呼称で optional 含み（Plecto の client auth は常に required）,
network policy / ext-authz（認証の確立点が異なり certificate-bound identity の代替にならない）

**Client identity（upstream mTLS / `[upstream.tls]` client_cert_path + client_key_path）**:
Plecto が upstream へ接続するとき提示する自身の証明書チェーンと秘密鍵。転送リクエストと health probe は
同一 connector を共有するので、宣言すれば両方が提示する（ADR 000078）。both-or-neither を validation で
強制し、秘密鍵は owner-only ファイル権限を要求（ADR 000062 (d) の規律を新規鍵に適用）。
_Avoid_: client cert（downstream で「検証する」側の語と紛れる——upstream 側は「提示する」identity）

## 分散（opt-in）

**Distribution (opt-in)**:
複数ノードで「設定の合意」だけを共有する任意レイヤ。foca（SWIM gossip）でメンバーシップ、openraft（Raft）で
設定/ルートを複製する。状態共有は前提にしない。既定は単一ノードで、これは標準では無効。
_Avoid_: cluster mode（状態共有を含意）, replication（部分概念）

**Config consensus（設定合意）**:
分散時に各ノードが同一の config version に合意すること。合意対象は*設定*であって*状態*ではない（状態はノード
ローカルのまま）。
_Avoid_: state replication（状態複製は対象外）, quorum（手段の一部）
