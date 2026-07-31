# インストール

[English](install.md)

Plecto Proxy の入手経路は 3 つ、provenance の強い順に並べる。初回は container 経路を取り、
[クイックスタート](quickstart/README.ja.md)に進むのが早い——下の検証手順に加えて manifest と
最初のプロキシ応答まで通しで書いてある。

## 1. コンテナイメージ（推奨）

署名を検証し、**タグではなく、いま検証した digest** を実行する
（[ADR 000084](ADR/000084.md) / [ADR 000087](ADR/000087.md)）:

```bash
IMAGE=ghcr.io/kaikei-e/plecto
TAG=0.6.2   # 最新リリースを選ぶ: https://github.com/Kaikei-e/PlectoProxy/releases
DIGEST=$(docker buildx imagetools inspect "$IMAGE:$TAG" --format '{{json .Manifest.Digest}}' | tr -d '"')

docker run --rm ghcr.io/sigstore/cosign/cosign:v3.1.1 verify "$IMAGE@$DIGEST" \
  --certificate-identity-regexp 'https://github.com/Kaikei-e/PlectoProxy/\.github/workflows/release\.yml@.*' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

イメージは distroless——shell も curl も無い——ので、Compose / Kubernetes の healthcheck から
shell out できない。そのために作られた自己プローブが `plecto healthz`。詳細は
[運用ガイド](operations.ja.md)。

## 2. 署名付きリリースバイナリ

[タグ付き release](https://github.com/Kaikei-e/PlectoProxy/releases) には
`plecto-<tag>-<target>.tar.gz` と、その `cosign` バンドル・署名済みチェックサムが並ぶ:

```bash
cosign verify-blob --bundle plecto-<tag>-<target>.tar.gz.sigstore.json \
  --certificate-identity-regexp 'https://github.com/Kaikei-e/PlectoProxy/\.github/workflows/release\.yml@.*' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  plecto-<tag>-<target>.tar.gz
```

バイナリは `cargo-auditable` でビルドされ（依存グラフがバイナリ自身に埋め込まれる）、SPDX SBOM が
同梱される（[ADR 000047](ADR/000047.md)）。リリースごとの正確なコマンドは各 release notes と
[`release.yml`](../.github/workflows/release.yml) 冒頭のコメントにある。

## 3. crates.io から

```bash
cargo install plecto
```

ゲートウェイと operator CLI をソースからビルドする
（[ADR 000090](ADR/000090.md) / [ADR 000091](ADR/000091.md)）。この経路は**リリースの provenance を
持たない**——自分でコンパイルしたので検証すべき署名も付属 SBOM も無い。デプロイには経路 1 か 2 を
選ぶこと。`cargo install` は開発と組み込み向け。

ライブラリ 3 クレートもバイナリと並んで公開されている。Plecto Proxy を「動かす」のではなく
自分のプログラムに「組み込む」ためのもの:

| クレート | 中身 |
| --- | --- |
| [`plecto-host`](https://crates.io/crates/plecto-host) | wasmtime 埋め込み: `Linker`・`InstancePre`・deny-by-default の host-API・インスタンスのライフサイクル |
| [`plecto-control`](https://crates.io/crates/plecto-control) | control plane: 宣言的 manifest・OCI アーティファクトのロード ＋ provenance ゲート・filter chain dispatch・アトミック reload |
| [`plecto-server`](https://crates.io/crates/plecto-server) | ライブラリとしての fast path: HTTP/1.1 · 2 · 3・TLS・routing・LB・upstream 管理 |

CI ジョブが各リリースの公開 API を「crates.io にある最新版」と差分検査するので、バージョンを上げ忘れた
破壊的変更は利用者のビルドではなく出荷前に落ちる。

## Runtime capability profile

prebuilt のバイナリ / イメージは 2 つの **named runtime capability profile** で出る
（[ADR 000079](ADR/000079.md)）:

| Profile | バイナリ / イメージタグ | コンパイル時に含むもの |
| --- | --- | --- |
| **minimal**（サフィックス無し・既定） | `plecto-<tag>-<target>.tar.gz` · `ghcr.io/kaikei-e/plecto:<version>` | default feature のみ——outbound のコードは一切コンパイルされない。攻撃面が最小。素のリバースプロキシ / ゲートウェイならこちら。 |
| **capabilities** | `plecto-<tag>-<target>-capabilities.tar.gz` · `ghcr.io/kaikei-e/plecto:<version>-capabilities` | `outbound-http` / `outbound-tcp` / `fat-guest` を追加。capability 依存の reference filter（JWKS を更新する JWT 認証・ext-authz・Redis 連携の global rate limit）と Go/TinyGo ゲストが必要とするもの。 |

**コンパイルして含めることは、貸与することではない。** capabilities ビルドであっても、manifest が
そのフィルタにその capability を宣言するまで何も貸さない——deny-by-default の allowlist と SSRF の床は
不変（[ADR 000036](ADR/000036.md) / [ADR 000060](ADR/000060.md)）。どの profile でビルドされたかは
`plecto --version` が表示する。

## リファレンスフィルタ

フィルタは runtime とは別に配布される。個別に cosign 署名され SPDX SBOM attestation を伴う CNCF Wasm
OCI Artifact として `ghcr.io/kaikei-e/plecto/filters/<name>` に置かれる
（[ADR 000080](ADR/000080.md)）——現在は `jwt` / `cors` / `apikey` / `extauthz`。どのフィルタがどの
runtime profile を要るか、および verify-then-load の手順は
[reference-filters.md](reference-filters.md)（英語）。

## クローンからビルドする

ツールチェーンと WASM ターゲットは [`plecto/rust-toolchain.toml`](../plecto/rust-toolchain.toml) に
ピン留めしてあるので、[`rustup`](https://rustup.rs/) が初回ビルド時に用意する（ツールチェーン外では
一度だけ `rustup target add wasm32-unknown-unknown`）。

```bash
cd plecto
cargo test --all   # 例フィルタを WASM コンポーネントへコンパイルし、wasmtime ホストにロードして
                   # 契約を end-to-end で検証する
cargo build --release -p plecto
```
