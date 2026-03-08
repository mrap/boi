# Goal Alignment (Generate Mode Only)

This check validates that a Generate-mode spec's implementation actually achieves the stated goals. It runs ONLY for specs with `**Mode:** generate` or `[Generate]` in the title.

## Instructions

You are validating whether the work done in this Generate spec actually achieves the Goal and satisfies the Success Criteria. This is different from code quality checks. This is about *what was built* matching *what was asked for*.

### Step 1: Extract the Goal

Re-read the `## Goal` section of the spec. Summarize the core objective in one sentence.

### Step 2: Check Success Criteria

Re-read the `## Success Criteria` section. For EACH checkbox item:

1. **Identify** the criterion text.
2. **Search** for a completed task (DONE status) that directly addresses this criterion.
3. **Evaluate** whether the implementation actually satisfies the criterion, not just touches the topic.
4. **Run verify** commands associated with related tasks if they exist.
5. **Classify** as:
   - **MET**: A DONE task fully satisfies this criterion with evidence.
   - **PARTIALLY_MET**: A DONE task addresses the topic but does not fully satisfy the criterion.
   - **UNMET**: No DONE task addresses this criterion, or the work does not satisfy it.

### Step 3: Check Constraints

Re-read the `## Constraints` section. For EACH constraint:

1. **Identify** the constraint text.
2. **Review** all completed tasks and their implementations.
3. **Classify** as:
   - **RESPECTED**: The implementation follows this constraint.
   - **VIOLATED**: The implementation breaks this constraint.

### Step 4: Check Cohesion

Evaluate whether the completed tasks form a coherent solution:

- Do the tasks work together, or are they disconnected pieces?
- Is there a clear path from the tasks to the Goal?
- Are there obvious gaps between tasks that would prevent the Goal from being achieved?

### Step 5: Generate Findings

For each finding, assign a severity:

| Finding Type | Severity |
|---|---|
| Unmet Success Criteria | HIGH |
| Constraint violation | HIGH |
| Partially met Success Criteria (< 50% addressed) | HIGH |
| Partially met Success Criteria (>= 50% addressed) | MEDIUM |
| Tasks that do not integrate with each other | MEDIUM |
| Missing normal-usage edge cases | MEDIUM |
| Polish issues (formatting, naming, minor UX) | LOW |

### Step 6: Generate Corrective Tasks

For each HIGH severity finding, generate a `[CRITIC]` PENDING task to address it:

```markdown
### t-N: [CRITIC] Address unmet criterion: "<criterion text>"
PENDING

**Spec:** <What needs to be done to satisfy this criterion. Be specific.>

**Verify:** <How to verify the criterion is met. Reference the original Success Criteria checkbox.>
```

## Output Format

Write your findings as a `## Goal Alignment Report` section appended to the spec:

```markdown
## Goal Alignment Report

### Goal Summary
<One sentence summary of the goal>

### Success Criteria Assessment
| # | Criterion | Status | Evidence | Task(s) |
|---|-----------|--------|----------|---------|
| 1 | <text> | MET | <brief evidence> | t-3 |
| 2 | <text> | UNMET | <why unmet> | --- |
| 3 | <text> | PARTIALLY_MET | <what's missing> | t-5 |

**Criteria met: X / Y**

### Constraint Assessment
| Constraint | Status | Notes |
|------------|--------|-------|
| <text> | RESPECTED | --- |
| <text> | VIOLATED | <how violated> |

### Cohesion Assessment
<Brief assessment of whether tasks form a coherent solution>

### Findings
- [HIGH] Criterion 2 is unmet: "<criterion text>". No task addresses <what>.
- [MEDIUM] Tasks t-3 and t-7 do not integrate: <why>.
```

If ALL criteria are MET and ALL constraints are RESPECTED, append `## Critic Approved` instead of generating corrective tasks.
