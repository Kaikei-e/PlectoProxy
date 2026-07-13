# クイックスタート — 検証済みイメージから最初のプロキシ応答まで

[English](README.md)

これは運用者向けクイックスタートです。Plecto Proxy のコンテナイメージを取得し、
**署名を検証してから**、最初のプロキシ応答を得るまでを、Docker 以外何もインストール
せずに実行します。すべてのコマンドはコピペ可能で、スクリプトに隠された手順はありま
せん。署名検証は導線の中核であり、省略可能な補足ではありません
（[ADR 000084](../ADR/000084.md) / [ADR 000087](../ADR/000087.md)）。

目標: このページを開いてから最初のプロキシ応答まで **5 分以内**。
もし時間がかかった・詰まった場合は、どこで詰まったかを
[Discussions](https://github.com/Kaikei-e/PlectoProxy/discussions) で教えてください。
初回導線の摩擦報告が、このページを改善する材料になります。

**前提:** Docker（現行バージョン。`docker buildx` は同梱されています）。

## 1. リリースタグを不変の digest に解決する

リリースは cosign により **タグではなく image digest に対して**署名されています——
タグは動き得ますが、digest は動きません。まず使いたいタグを、検証し*かつ*実行する
digest に固定します:

```bash
IMAGE=ghcr.io/kaikei-e/plecto
TAG=0.3.2   # 最新リリースを選ぶ: https://github.com/Kaikei-e/PlectoProxy/releases

DIGEST=$(docker buildx imagetools inspect "$IMAGE:$TAG" --format '{{json .Manifest.Digest}}' | tr -d '"')
echo "$DIGEST"
```

表示された digest は、該当リリースのリリースノートに記録された値と照合できます。

## 2. 署名を検証する

cosign は Sigstore 自身が公開しているコンテナから実行するので、インストールは不要
です。identity フラグは署名者をこのリポジトリの release workflow に固定します——
issuer だけでは任意の GitHub Actions workflow に一致してしまいます:

```bash
docker run --rm ghcr.io/sigstore/cosign/cosign:v3.1.1 verify "$IMAGE@$DIGEST" \
  --certificate-identity-regexp 'https://github.com/Kaikei-e/PlectoProxy/\.github/workflows/release\.yml@.*' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

成功すると検証済みクレーム（JSON 配列）が表示されます。検証に失敗した場合は
**そこで止めてください**——そのイメージを実行してはいけません。

<details>
<summary>ローカルにインストールした cosign を使いたい場合</summary>

パッケージマネージャで cosign をインストールし（`brew install cosign`、
`apk add cosign`、`pacman -S cosign`、または
[sigstore/cosign](https://github.com/sigstore/cosign/releases) の署名付きリリース
バイナリ）、同じ `cosign verify` コマンドを
`docker run --rm ghcr.io/sigstore/cosign/cosign:v3.1.1` プレフィックスなしで実行して
ください。

</details>

## 3. 検証済み digest を実行する

Plecto は 1 枚の TOML manifest で設定します。最小の manifest——8080 で listen し、
すべてを backend へ転送する——を書き出し、隣にスタンドインの backend を起動します:

```bash
mkdir -p plecto-quickstart && cd plecto-quickstart

cat > plecto.toml <<'EOF'
[listen]
addr = "0.0.0.0:8080"

[[upstream]]
name = "backend"
addresses = ["backend:80"]
[upstream.health]
path = "/"
interval_ms = 1000

[[route]]
upstream = "backend"
[route.match]
path_prefix = "/"
EOF

docker network create plecto-quickstart
docker run -d --name backend --network plecto-quickstart traefik/whoami
docker run -d --name plecto --network plecto-quickstart -p 8080:8080 \
  -v "$PWD:/etc/plecto:ro" "$IMAGE@$DIGEST"
```

プロキシが実行しているのは、タグではなく**検証した digest そのもの**です。backend
（`traefik/whoami`、極小のエコーサーバ）はあなた自身のサービスの代役であり、上記の
検証の対象では*ありません*。Plecto の供給網検証の主張は **Plecto が**ロード・実行
するものについてであり、あなたの upstream には及びません。

## 4. 最初のプロキシ応答

```bash
curl -s http://localhost:8080/
```

whoami の応答が、署名検証済みの Plecto を経由して返ってきます。これがループの全体
です: **解決 → 検証 → 実行 → 応答**。

## 後片付け

```bash
docker rm -f plecto backend
docker network rm plecto-quickstart
cd .. && rm -r plecto-quickstart
```

## 次へ

- **LB の背後に複数レプリカ** — 走らせられるマルチレプリカ reference（graceful
  drain、PROXY protocol v2、TLS シナリオ）:
  [`plecto/examples/multi-replica/`](../../plecto/examples/multi-replica/README.md)
- **フィルタを書く** — extension plane こそが本題:
  [docs/writing-a-filter.md](../writing-a-filter.md)
- **署名付き reference filter**（JWT、CORS、API-key、ext-authz）と verify-then-load
  の手順: [docs/reference-filters.md](../reference-filters.md)
- **運用**（readiness、drain、hot reload）: [docs/operations.md](../operations.md)
- **runtime capability profile** — このページで使ったのは `minimal` profile。
  `-capabilities` イメージは、それを必要とするフィルタ向けに outbound capability を
  追加します（[ADR 000079](../ADR/000079.md)）
