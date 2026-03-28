# Output Quality

Evaluates whether each completed task's output actually solves what the spec asked for, using a 6-dimension rubric. This checks *did the worker do it well?*, not just *did the worker do it?*.

## When to Apply

Apply this check to every task marked DONE in the spec. For each DONE task, evaluate the output against its `**Spec:**` section.

Skip tasks that are PENDING, SKIPPED, or SUPERSEDED.

## Scoring Rubric

For each DONE task, score the following 6 dimensions on a **1–5 integer scale**. You MUST justify each score with a specific reference to what was or wasn't done.

### Dimension 1: Correctness (weight: 0.25)
*Does the output correctly solve the stated task?*

- **1** — Fundamentally wrong; major errors that invalidate the output.
- **2** — Partially correct but with significant errors or misconceptions.
- **3** — Mostly correct; minor errors that don't invalidate the core result.
- **4** — Correct with only trivial issues (typos, style).
- **5** — Fully correct; no errors detected.

### Dimension 2: Completeness (weight: 0.20)
*Does it address all parts of the spec?*

- **1** — Addresses less than 25% of the spec requirements.
- **2** — Addresses roughly 25–50%; major sections missing.
- **3** — Addresses roughly 50–75%; some sections incomplete.
- **4** — Addresses 75–95%; only minor items missing.
- **5** — Fully addresses every requirement in the spec.

### Dimension 3: Depth (weight: 0.15)
*Depth of analysis, reasoning, or implementation detail.*

- **1** — Superficial; no reasoning or analysis provided.
- **2** — Shallow; states conclusions without justification.
- **3** — Moderate depth; explains key decisions with some reasoning.
- **4** — Thorough; well-reasoned with evidence or examples.
- **5** — Exceptional depth; nuanced analysis, considers edge cases and trade-offs.

### Dimension 4: Code Quality (weight: 0.15)
*If code is present: readability, structure, error handling. If no code: clarity and structure of prose.*

- **1** — Unreadable or broken code/prose; no structure.
- **2** — Poorly structured; hard to follow; missing basic error handling.
- **3** — Adequate structure; some rough edges but functional.
- **4** — Well-structured; clean, readable, handles common errors.
- **5** — Excellent; idiomatic, modular, robust error handling, clear naming.

### Dimension 5: Spec Adherence (weight: 0.15)
*Does it follow the spec's constraints and format requirements?*

- **1** — Ignores constraints entirely (wrong language, wrong format, etc.).
- **2** — Violates multiple constraints or format requirements.
- **3** — Mostly adheres; one or two minor deviations.
- **4** — Fully adheres to constraints with trivial deviations at most.
- **5** — Perfect adherence to all constraints, format, and style requirements.

### Dimension 6: Actionability (weight: 0.10)
*Can the output be used immediately, or does it need rework?*

- **1** — Unusable as-is; requires complete rework.
- **2** — Needs significant rework before it can be used.
- **3** — Usable with moderate editing or additions.
- **4** — Usable with minor polish; ready for most purposes.
- **5** — Ready to use immediately with no changes needed.

---

## Scoring Method

For each DONE task, compute the weighted score as follows:

1. Record each dimension score as an integer 1–5.
2. Normalize each: `normalized = (score - 1) / 4.0`
3. Weighted sum: `(correctness × 0.25) + (completeness × 0.20) + (depth × 0.15) + (code_quality × 0.15) + (spec_adherence × 0.15) + (actionability × 0.10)`

A weighted score below **0.50** means the task output is inadequate.

---

## Output Format

For each DONE task, output a block like this:

```
### Output Quality: t-N — <task title>

CORRECTNESS: N/5 — <one-line justification referencing spec requirements>
COMPLETENESS: N/5 — <one-line justification>
DEPTH: N/5 — <one-line justification>
CODE_QUALITY: N/5 — <one-line justification>
SPEC_ADHERENCE: N/5 — <one-line justification>
ACTIONABILITY: N/5 — <one-line justification>

Weighted score: 0.XX

Status: PASS | FAIL
```

If Status is FAIL (weighted score < 0.50), you MUST add a `[CRITIC] PENDING` revision task to the spec:

```markdown
### t-N-quality-revision: Revise t-N output — quality score below threshold
[CRITIC] PENDING

**Spec:** The output for t-N scored X.XX (below the 0.50 threshold). Specific weaknesses: <list dimensions that scored 1 or 2 with their justifications>. Re-execute the task with attention to these gaps.

**Deps:** (none)
```

---

## Anti-Gaming Rules

These rules prevent the model from inflating scores:

1. **No score of 5 without explicit verification.** Correctness=5 requires the verify command passed or the file/artifact was confirmed to exist and be correct. "It looks right" is at most 4.
2. **Score relative to the spec, not to effort.** A task that produced a beautiful document on the wrong topic scores 1 on correctness.
3. **Missing items drag completeness down.** If the spec lists 5 requirements and 2 are missing, completeness ≤ 3.
4. **A truncated or skeletal output cannot score above 3 on depth.** Surface-level output with no reasoning is shallow.
5. **Justify every score ≥ 4.** If you cannot write a specific one-line justification for why a score is 4 or 5, lower it by 1.

---

## Example: Passing Task

Task spec required: "Create a custom critic check at `~/.boi/critic/custom/output-quality.md` that scores on 6 dimensions and flags tasks below 0.50."

Output: File exists at the correct path, contains all 6 dimension definitions with scoring guides, includes threshold logic.

```
### Output Quality: t-2 — Create quality scoring critic check

CORRECTNESS: 5/5 — File is at correct path and implements all specified requirements.
COMPLETENESS: 5/5 — All 6 dimensions present, threshold logic included, format matches spec.
DEPTH: 4/5 — Scoring guides are detailed; could have more examples of borderline cases.
CODE_QUALITY: 4/5 — Markdown is clean and well-structured; minor formatting inconsistencies.
SPEC_ADHERENCE: 5/5 — Follows critic check format exactly; correctness weighted highest.
ACTIONABILITY: 5/5 — Can be used immediately by the critic without modification.

Weighted score: 0.93

Status: PASS
```

## Example: Failing Task

Task spec required: "Document how critic checks are loaded, what data the critic has, and how decisions are made."

Output: A two-paragraph summary that mentions check loading but skips data access and decision-making.

```
### Output Quality: t-1 — Understand current critic flow

CORRECTNESS: 3/5 — Check loading described correctly; data access and decision flow omitted entirely.
COMPLETENESS: 2/5 — Only ~35% of spec requirements addressed; missing 3 of 5 required sections.
DEPTH: 2/5 — Shallow summary; no concrete code references or examples.
CODE_QUALITY: 3/5 — Prose is readable but lacks structure.
SPEC_ADHERENCE: 2/5 — Spec required 5 specific sections; only 1 present.
ACTIONABILITY: 2/5 — Missing sections must be added before this document is useful.

Weighted score: 0.34

Status: FAIL
```
