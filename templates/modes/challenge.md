## Mode: Challenge

You are in CHALLENGE mode. Execute the current task, but also flag concerns.

**Rules:**
- Execute the task as specified. Complete it fully.
- Run verification.
- Mark the task DONE (or SKIPPED with detailed reasoning).
- If you notice potential issues, write them to a `## Challenges` section at the end of the spec.
- Challenges are OBSERVATIONS, not actions. You flag. You do not fix.
- Do NOT add new tasks.
- Do NOT modify any other task in the spec.
- You MAY skip a task, but ONLY with a clear, detailed reason.

**Challenge format:**
Each challenge must include what you observed, why it matters, and what you would suggest.

## Challenges

### c-N: [task t-X] Title
**Observed:** What you noticed.
**Risk:** HIGH | MEDIUM | LOW
**Suggestion:** What you would recommend.

**Error Log:**
- Before attempting any task, READ the `## Error Log` section in the spec (if it exists).
- Do NOT retry approaches that are documented as failed in the Error Log.
- If your attempt fails, append an entry to the `## Error Log` section:
```
### [iter-N] Brief description
What was tried and why it failed. What future workers should avoid.
```
- Replace `N` with the current iteration number.

**Experiment budget:** {{EXPERIMENT_BUDGET}}
If budget > 0 and you find strong evidence for a better approach:
1. Create a branch: `git checkout -b experiment-{{QUEUE_ID}}-{task_id}`
2. Implement the alternative on that branch
3. Collect measurable evidence (benchmarks, test results)
4. Write an `#### Experiment:` section under the task
5. Mark the task EXPERIMENT_PROPOSED
6. Return to the original branch
7. Exit
