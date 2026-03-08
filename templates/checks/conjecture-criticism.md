# Conjecture & Criticism

Validates that completed work explored alternatives before committing to an approach. Based on David Deutsch's epistemology: knowledge is created through conjecture and criticism, not authority or induction. Every recommendation is a conjecture that deserves genuine criticism.

This check applies to ALL specs, not just generate-mode. Any task that chose an approach over alternatives should show evidence of that reasoning.

## Checklist

- [ ] For each task that made a design or implementation choice, at least one alternative was explicitly considered and rejected with reasoning
- [ ] Rejected alternatives are documented with the specific flaw or trade-off that eliminated them (not just "X is better")
- [ ] No single-option recommendations without acknowledging what was NOT considered
- [ ] When multiple viable approaches exist, the spec shows evidence of independent evaluation (not just "I picked the first thing that works")
- [ ] Key trade-offs are stated explicitly: what does the chosen approach sacrifice, and why is that acceptable?
- [ ] For generate-mode specs: the output includes a "what this is NOT" or "alternatives considered" section

## Examples of Violations

### Unjustified choice (HIGH severity)
Task says: "Used SQLite for storage"
No mention of alternatives (flat files, PostgreSQL, in-memory), no reasoning for why SQLite wins.
Fix: Add reasoning — "SQLite over flat files because X, over Postgres because Y"

### False dichotomy (MEDIUM severity)
Task says: "Chose polling over webhooks because webhooks are complex"
Only two options considered. What about file watchers, event streams, hybrid approaches?
Fix: Broaden the option space before eliminating.

### Rubber-stamp recommendation (HIGH severity)
Generate-mode spec produces a recommendation with no runner-up, no killed conjectures, no trade-offs.
The output reads as "here's the answer" with no evidence of adversarial thinking.
Fix: Add "Alternatives Considered" section with at least 2 eliminated options and specific reasons.

### Cosmetic criticism only (MEDIUM severity)
Alternatives are listed but dismissed with vague reasoning: "not as good", "less suitable", "suboptimal".
Fix: State the SPECIFIC flaw. "Redis adds operational complexity (separate process, persistence config) that isn't justified for <1MB of data."
