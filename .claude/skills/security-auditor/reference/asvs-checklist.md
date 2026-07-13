# OWASP ASVS 5.0 — deep-check reference (auth / crypto / session / input)

ASVS 5.0.0（2025年5月、https://asvs.dev/）から Plecto に効く要件を抜粋。各 finding に要件 ID を付ける
（章番号は版で動くので、参照時に asvs.dev で確認）。345 要件・17章（V6 = Authentication）のうち、
ゲートウェイ/ホストに直結する観点だけを掃く。

## 認証（Authentication）
- [ ] トークン/鍵の比較は**定数時間**（タイミング攻撃を防ぐ）。`==` での秘密比較を疑う。
- [ ] 認証フィルタの失敗は fail-closed（`short-circuit 401/403`）。trap/deadline でも素通りしない。
- [ ] 認証情報（bearer/api-key/mTLS）の検証ロジックが正しい issuer/audience/exp を見ているか。
- [ ] ブルートフォース耐性: 認証失敗にレート制限/バックオフ。

## セッション / トークン
- [ ] セッション/トークンは十分なエントロピー（CSPRNG）。`rand::random` の弱い seed を疑う。
- [ ] 失効・期限・回転がある。session fixation を防ぐ（認証後に再発行）。
- [ ] Cookie 属性（Secure/HttpOnly/SameSite）を Plecto が設定/転送する場合の正しさ。

## 暗号（Cryptography）
- [ ] TLS は新しいプロトコル/ciphers のみ。証明書検証を無効化していない（`danger_accept_invalid_certs` 等）。
- [ ] 秘密鍵・証明書はメモリ/ファイルで適切に保護、ログ/エラーに出さない。
- [ ] 自前 crypto を書いていない。既存の監査済みライブラリ（rustls 等）を使う。
- [ ] 乱数は CSPRNG（`getrandom`/`rand::rngs::OsRng`）。予測可能な seed を使わない。

## 入力検証 / 出力エンコーディング
- [ ] すべての untrusted 入力（クライアント、**フィルタ出力**、upstream）にサイズ/長さ/形式の上限。
- [ ] ヘッダ値に CRLF/制御文字が混入しない（header/response splitting 防止）。
- [ ] URL 構築は検証済み・正規化済みの値からのみ（SSRF 連動）。

## エラー処理 / ロギング（ASVS の error handling 章）
- [ ] クライアントに internal error 詳細（スタック/パス/バージョン）を返さない。
- [ ] 監査ログに認証・認可・拒否イベントを残す。機微情報はマスク。
- [ ] fail-open を作らない（A10 連動）。

## 設定 / ハードニング
- [ ] deny-by-default（host-API・CORS・ルート）。
- [ ] リソース上限（接続/ヘッダ/ボディ/実行時間/メモリ）が untrusted パスに効く。

> 注: ASVS はレベル（L1/L2/L3）で要求が変わる。Plecto はマルチテナント untrusted 実行を含むので、
> 認証/暗号/分離まわりは L2 相当以上を目安に判定する。
