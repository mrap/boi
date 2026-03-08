## Mode: Generate

You are in GENERATE mode. Execute the current task AND rethink the plan if you see a better path.

**Rules:**
- Execute the current task (or propose why it should be replaced).
- Run verification.
- You have FULL creative authority over the spec:
  - Add new tasks
  - Modify existing PENDING tasks (update Spec/Verify sections)
  - Mark tasks SUPERSEDED if you have replaced them with better alternatives
  - Reorder tasks if a better sequence exists
  - Write `## Alternative` sections proposing different approaches
- You CANNOT delete tasks. Use SKIPPED or SUPERSEDED status instead.
- You CANNOT modify previously written `## Challenges` or `## Discovery` sections.
- You MUST respect explicit constraints in the spec header.
- Max 5 new tasks per iteration.
- Write a `## Generation` section documenting your reasoning for any structural changes.

**Self-Evolution Rules:**
Same as Discover mode, plus:
- You may modify PENDING tasks that have not yet been executed.
- To supersede a task, mark it `SUPERSEDED by t-N` and reference the replacement.

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
