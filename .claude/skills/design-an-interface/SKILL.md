---
name: design-an-interface
description: Generate multiple radically different interface designs for a module using parallel sub-agents, then compare. Use when designing an API, a Rust trait, a host-API surface, or the `plecto:filter` WIT world; when exploring interface options or comparing module shapes; or when the user says "design it twice" / 「インターフェース設計」「WIT を設計」「二回設計して」.
---

# Design an Interface

Based on "Design It Twice" from "A Philosophy of Software Design": your first idea is unlikely to be the best. Generate multiple radically different designs, then compare.

In Plecto this is the tool for the highest-leverage interface decisions: the **`plecto:filter` WIT world** (the contract between the native fast path and untrusted WASM filters) and the **host-API capability surface**. These are among Plecto's still-open design questions (see `CLAUDE.md`) — use this skill before freezing either. For WIT-specific concerns (resources, `stream<u8>` bodies, `wasm32-wasip2`→P3 migration), pair it with the `wit-contract-design` skill.

## Workflow

### 1. Gather Requirements

Before designing, understand:

- [ ] What problem does this module solve?
- [ ] Who are the callers? (other modules, external users, tests, **untrusted filter authors**)
- [ ] What are the key operations?
- [ ] Any constraints? (performance, capability/security boundary, compatibility, existing patterns)
- [ ] What should be hidden inside vs exposed?

Ask: "What does this module need to do? Who will use it?"

### 2. Generate Designs (Parallel Sub-Agents)

Spawn 3+ sub-agents simultaneously using the **Agent tool** (`subagent_type=general-purpose`, or `fork` to inherit this conversation's context). Each must produce a **radically different** approach.

```
Prompt template for each sub-agent:

Design an interface for: [module description]

Requirements: [gathered requirements]

Constraints for this design: [assign a different constraint to each agent]
- Agent 1: "Minimize method count - aim for 1-3 methods max"
- Agent 2: "Maximize flexibility - support many use cases"
- Agent 3: "Optimize for the most common case"
- Agent 4: "Take inspiration from [specific paradigm/library — e.g. Envoy proxy-wasm, wasi:http/middleware, Pingora's life-of-a-request callbacks]"

Output format:
1. Interface signature (types/methods)
2. Usage example (how caller uses it)
3. What this design hides internally
4. Trade-offs of this approach
```

### 3. Present Designs

Show each design with:

1. **Interface signature** - types, methods, params
2. **Usage examples** - how callers actually use it in practice
3. **What it hides** - complexity kept internal

Present designs sequentially so user can absorb each approach before comparison.

### 4. Compare Designs

After showing all designs, compare them on:

- **Interface simplicity**: fewer methods, simpler params
- **General-purpose vs specialized**: flexibility vs focus
- **Implementation efficiency**: does shape allow efficient internals? (for filters: does it allow the header-only fast path and zero-copy body bypass?)
- **Depth**: small interface hiding significant complexity (good) vs large interface with thin implementation (bad)
- **Ease of correct use** vs **ease of misuse** (for a capability surface, misuse = a filter reaching something it shouldn't — deny-by-default must be the easy path)

Discuss trade-offs in prose, not tables. Highlight where designs diverge most.

### 5. Synthesize

Often the best design combines insights from multiple options. Ask:

- "Which design best fits your primary use case?"
- "Any elements from other designs worth incorporating?"

## Evaluation Criteria

From "A Philosophy of Software Design":

**Interface simplicity**: Fewer methods, simpler params = easier to learn and use correctly.

**General-purpose**: Can handle future use cases without changes. But beware over-generalization.

**Implementation efficiency**: Does interface shape allow efficient implementation? Or force awkward internals?

**Depth**: Small interface hiding significant complexity = deep module (good). Large interface with thin implementation = shallow module (avoid).

## Anti-Patterns

- Don't let sub-agents produce similar designs - enforce radical difference
- Don't skip comparison - the value is in contrast
- Don't implement - this is purely about interface shape
- Don't evaluate based on implementation effort
