# Full-review report template

日本語で埋め、識別子・パス・コマンドは英語のまま。

```markdown
## Full Review: <short title>

### Verdict
**<merge-ready | fix-then-merge | do-not-merge>** — <一文の理由>

### Scope
- **Target**: <merge SHA / PR # / base...head>
- **Spec**: <PR 要求の要約 + 参照 ADR>
- **Blast radius**: <N files; ハイライト: CI / host / server / control / …>
- **Lanes run**: correctness, security, tests
- **Depth**: standard | deep

### Deterministic gates
| Gate | Result | Note |
|------|--------|------|
| fmt | pass/fail/skip | |
| clippy | pass/fail/skip | |
| tests (scoped) | pass/fail/skip | |
| other | | |

### Findings

#### F1. <title> — Critical|High|Medium|Low|Needs evidence
- **Lane**: correctness|security|tests
- **Evidence**: `<path>:<lines>` or command output
- **Impact**: …
- **Remediation**: …
- **Confidence**: high|medium|low

（severity 降順。Finding ゼロなら "No evidence-backed findings."）

### Test / CI honesty
- 削除・弱体化・欠落ネガティブ: …
- CI 緩和の有無: …

### Out-of-scope / surprise edits
- <PR 説明に無いが入っている変更。無ければ "None material.">

### Open questions (human)
1. …

### Moderator notes
- レーン間の不一致と採否: …
- 追加で回すべきスキル: security-auditor / …（または無し）
```
