# Adversarial checklist (Critic lanes)

ロードするのは Phase 2 のレーン起動時。SKILL.md の Hard rules を優先する。

## Shared critic preamble

各 Critic Agent に必ず含める:

```
Role: Hostile Critic. Reject code that violates Spec even if it "works".
Prefer false positives over silent misses for security/contract issues.
Input: Spec + Diff (+ surrounding code you Read yourself). No Builder rationale.
Output: PASS, or a list of violations. Do not rewrite the feature.
Each violation MUST include:
  1) claim  2) evidence (path:line or command output)  3) impact  4) remediation
  5) confidence (high|medium|low)
If evidence is missing, mark Needs evidence — do not invent.
Negation-blindness: Spec "MUST NOT" / fail-closed / deny-by-default clauses are highest priority.
```

## Lane A — Correctness / contract

- [ ] Diff は Spec / PR 要求を満たすか。隣接するが未要求の振る舞いを追加していないか
- [ ] Happy path 以外: 空入力、巨大入力、順序逆転、再入、部分失敗
- [ ] エラー経路が握り潰し / 別意味へのマップになっていないか
- [ ] Plecto: filter trap・deadline が fail-open になっていないか
- [ ] Plecto: data plane で `unwrap`/`expect`/panic が untrusted 入力経路に無いか
- [ ] ADR / WIT 版契約（decision variant、header `list<u8>` 等）との矛盾
- [ ] 「全件取得してからフィルタ」等、スケールで壊れる契約違反

## Lane B — Security / supply chain

### Trust & auth

- [ ] 認可が UI/クライアント前提ではなくサーバ側で強制されているか
- [ ] クライアント供給 ID / フラグ / Host / URL を privileged 効果に繋いでいないか
- [ ] TLS / STEK / client_auth / session の状態再利用・バージョン不一致

### WASM / host

- [ ] Linker capability の拡大（WASI / outbound / FS）が deny-by-default を破っていないか
- [ ] pool 再利用でのテナント間漏洩（zeroize / isolation）
- [ ] outbound HTTP/TCP の SSRF・許可リスト迂回

### CI / deps（OWASP Secure Coding with AI §7/§10）

- [ ] `.github/workflows/**` の権限拡大、`pull_request_target`、未ピン Action
- [ ] `deny.toml` / lockfile の緩和（allow 追加、監査スキップ）
- [ ] 新規クレートの実在・メンテナ・異常に新しい crate
- [ ] rules ファイル（`CLAUDE.md`, `.cursor/rules/**`）の永続ステア変更

### Logging

- [ ] 秘密・トークン・生ヘッダのログ出し

深掘り: `.claude/skills/security-auditor/reference/`（親がパスを指示した場合のみ）。

## Lane C — Tests / CI honesty（OWASP §8）

- [ ] 削除されたテストとその理由が Spec 上正当か
- [ ] アサーション弱体化（`is_ok()` だけ、`is_some()` だけ等）
- [ ] 実装をミラーするだけのテスト（バグを仕様化）
- [ ] モックがセキュリティ境界の実コードを避けていないか
- [ ] 欠落ネガティブケース: 不正入力、認可失敗、timeout/trap、巨大 body
- [ ] CI: `continue-on-error`、path filter で保安ジョブがスキップ、必須チェック外し

## Moderator evidence gate

| 受け入れる | 捨てる / 降格する |
|---|---|
| path:line 引用 + 影響説明 | 「一般的に危険」だけの抽象論 |
| 再現コマンドと失敗出力 | Builder チャットの意図説明のみ |
| 欠落テストを具体ケースで指名 | スタイル好み・命名論争（別チャネルへ） |
| 複数レーン合意 | 単一レーン・low confidence・証拠なし |

## Severity mapping (quick)

- **Critical**: 認証回避、fail-open on trap、secret in log/CI、arbitrary code in workflow
- **High**: 契約破りの本番経路、capability 拡大、SSRF 可能な outbound
- **Medium**: 条件付き実害、テスト欠落で回帰しうる保安性質
- **Low**: 明確化・保守性。行動は正しいが読み手を誤導
