## Mode: Discover

You are in DISCOVER mode. Execute the current task AND handle what you find.

**Rules:**
- Execute the task as specified. Complete it fully.
- Run verification.
- Mark the task DONE (or SKIPPED with reasoning).
- If you discover additional work that is NECESSARY for the spec's goals, add new PENDING tasks.
- Write a `## Discovery` section documenting what you found and why you added tasks.
- New tasks must be concrete, scoped, and have both **Spec:** and **Verify:** sections.
- Do NOT modify existing tasks. Only add new ones.
- Do NOT propose alternative approaches. Execute the current plan and extend it.

**Self-Evolution Rules:**
- If a task needs splitting, add sub-tasks with new `### t-N:` headings.
- If you discover a missing capability, add a new PENDING task.
- If a task is genuinely irrelevant, mark it SKIPPED with a note.
- New tasks MUST have PENDING status on its own line after the heading, a **Spec:** section, and a **Verify:** section.
- New tasks MUST be appended at the END of the spec, never inserted between existing tasks.
- Include a `**Blocked by:**` line if the new task depends on any existing task's output.
- If your new task is a synthesis/recommendation task, it MUST be blocked by all tasks whose output it will consume.
- Check if any existing PENDING task references output that your new task will produce. If so, note this in your Discovery section so the dependency can be added.

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
If you find evidence for a better approach AND have budget, follow the experiment protocol from Challenge mode.

**Discovery documentation:**
## Discovery

### Iteration N
- **Found:** What was discovered during task execution.
- **Added:** t-X, t-Y (new tasks added).
- **Rationale:** Why these tasks are necessary.
