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

## 分散（opt-in）

**Distribution (opt-in)**:
複数ノードで「設定の合意」だけを共有する任意レイヤ。foca（SWIM gossip）でメンバーシップ、openraft（Raft）で
設定/ルートを複製する。状態共有は前提にしない。既定は単一ノードで、これは標準では無効。
_Avoid_: cluster mode（状態共有を含意）, replication（部分概念）

**Config consensus（設定合意）**:
分散時に各ノードが同一の config version に合意すること。合意対象は*設定*であって*状態*ではない（状態はノード
ローカルのまま）。
_Avoid_: state replication（状態複製は対象外）, quorum（手段の一部）
