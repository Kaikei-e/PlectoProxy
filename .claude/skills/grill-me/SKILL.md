---
name: grill-me
description: Interview the user relentlessly about a plan or design until reaching shared understanding, resolving each branch of the decision tree. Use when user wants to stress-test a plan, get grilled on their design, validate a Fork-style decision (判断/根拠/再検討条件) before committing, or mentions "grill me" / 「詰めて」「壁打ち」.
---

Interview me relentlessly about every aspect of this plan until we reach a shared understanding. Walk down each branch of the design tree, resolving dependencies between decisions one-by-one. For each question, provide your recommended answer.

Ask the questions one at a time.

If a question can be answered by exploring the codebase, explore the codebase instead.

For Plecto, lean on the project's founding design — its Tenets and Fork decisions (1–10), summarised in `CLAUDE.md`: when a decision touches one of the Forks or Tenets, frame the question as "判断 / 根拠 / 再検討条件" and check whether the proposed answer contradicts the recorded stance before accepting it.
