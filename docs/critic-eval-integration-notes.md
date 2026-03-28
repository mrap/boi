# Critic Eval Integration Notes

Generated during q-067 t-1.

---

## 1. How Critic Checks Are Loaded and Executed

### Check loading (`critic_config.py::get_active_checks`)

1. **Default checks** live in `~/boi/src/templates/checks/*.md`. The active set is controlled by `config["checks"]` (default list: `spec-integrity`, `verify-commands`, `code-quality`, `completeness`, `fleet-readiness`, `conjecture-criticism`, `goal-alignment`, `quality-scoring`).
2. **Custom checks** live in `~/.boi/critic/custom/*.md`. A custom file with the same stem as a default file **replaces** the default. New filenames are additive.
3. **Generate-mode-only checks** (`config["generate_checks"]`, e.g. `goal-alignment`) are loaded separately via `get_generate_checks()` and appended after standard checks for `generate`-mode specs.

### Prompt assembly (`critic.py::generate_critic_prompt`)

The critic prompt is built in this order:
1. Load template (`~/.boi/critic/prompt.md` override, else `templates/critic-prompt.md`).
2. Read spec content.
3. Call `get_active_checks()` to collect all active checks.
4. `_build_quality_scoring_section()` — loads `templates/checks/quality-scoring.md` and wraps it as a pre-check section (runs FIRST, before detailed checks).
5. `_build_mode_awareness_section()` — injects mode-specific rules (execute/challenge/discover/generate).
6. Iterates through checks, formatting as `### Check: {name} ({source})`.
7. Template variable substitution: `{{SPEC_CONTENT}}`, `{{CHECKS}}`, `{{QUEUE_ID}}`, `{{ITERATION}}`, `{{SPEC_PATH}}`.

The assembled prompt is written atomically to `~/.boi/queue/{queue_id}.critic-prompt.md`.

### Execution

`run_critic()` is called by daemon_ops. It generates the prompt file but does **not** launch the agent itself — the daemon spawns the configured runtime CLI (default: `claude -p`) process. The critic model reads the spec, applies checks, and modifies the spec file in place.

---

## 2. Data the Critic Has Access To

From the prompt template injection:
- **Full spec content** (all tasks with their current status — PENDING/DONE/SKIPPED/SUPERSEDED, body text, Verify sections)
- **Queue ID and iteration number** (so it knows pass 1 vs pass 2)
- **Spec path** (can reference it in its output)

From check definitions injected into the prompt:
- All check criteria (code-quality, completeness, verify-commands, etc.)
- The quality-scoring rubric (CQ/TQ/DOC/ARCH signals)

From queue entry (via `_build_mode_awareness_section`):
- **Mode** (execute/challenge/discover/generate)
- Pre-iteration task IDs (implied by mode context)

**What the critic does NOT have:**
- Raw diff of file changes (would require tooling integration)
- Contents of files modified during the iteration (unless spec tasks reference them explicitly)
- Direct access to test output or verification results

---

## 3. How Critic Decisions Are Made

### During critic run (model behavior instructed by prompt)

The quality-scoring section gates the rest:
- **Score ≥ 0.85**: Fast-approve. Skip detailed checks. Append `## Critic Approved`.
- **Score 0.50–0.84**: Standard review. Run all checks. Score informs severity.
- **Score < 0.50**: Auto-reject. Add `[CRITIC] PENDING` task.

For standard review, each check instructs the critic to look for violations and flag them with severity (HIGH/MEDIUM/LOW). If no HIGH violations found → append `## Critic Approved`.

### After critic run (`parse_critic_result`)

Reads the modified spec file looking for:
1. `^## Critic Approved` → `approved = True`
2. `^### t-\d+:.*\[CRITIC\].*\nPENDING` → count of new critic tasks
3. JSON block with `overall_quality_score` key → extract score + per-category signals

`compute_quality_gate(quality_score)` maps score to gate decision:
- `≥ 0.85` → `fast_approve`
- `< 0.50` → `auto_reject`
- `0.50–0.84` → `standard`
- `None` → `unknown`

### Looping

`should_run_critic()` controls re-runs:
- Max 2 passes for execute/challenge/discover mode.
- Max 3 passes for generate mode.
- Only triggers `on_complete` (when all tasks are DONE/SKIPPED).

---

## 4. Where a Quality Scoring Check Would Fit

### Current state

The **existing** quality-scoring mechanism (`quality-scoring.md` + `_build_quality_scoring_section()`) uses a **code-focused** rubric (18 signals across Code Quality, Test Quality, Documentation, Architecture). It scores at the iteration level, not per-task.

The **eval system rubric** (`eval/rubric.py`) uses 6 higher-level dimensions:
- `correctness` (weight 0.25)
- `completeness` (weight 0.20)
- `depth` (weight 0.15)
- `code_quality` (weight 0.15)
- `spec_adherence` (weight 0.15)
- `actionability` (weight 0.10)

These are scored 1–5 and normalized to 0.0–1.0 via `compute_weighted_score()`.

### Integration point for `output-quality.md`

The new check should be a **custom critic check** at `~/.boi/critic/custom/output-quality.md`. It will:
1. Run as a standard check (not the pre-check quality-scoring).
2. Evaluate each completed task's output against its spec using the 6 rubric dimensions.
3. Use the `build_scoring_prompt()` pattern from `prompts/pairwise.py` (single-response scoring, not pairwise — no reference output available).
4. Flag tasks where weighted score < 0.50.
5. Output scores in a parseable format.

The check file instructs the critic model (running via the configured runtime CLI) to apply the rubric inline, without calling `judge.py` directly. The rubric criteria are embedded in the check's markdown.

The key difference from the existing `quality-scoring.md`:
- `quality-scoring.md` = code artifact quality (is the code well-written?)
- `output-quality.md` = task output quality (did the worker actually solve the task well?)

### Output format integration

The critic already parses `overall_quality_score` from JSON. For the new check, per-task scores should use the format from `rubric.py`'s `parse_scores()`:
```
CORRECTNESS: N/5
COMPLETENESS: N/5
DEPTH: N/5
CODE_QUALITY: N/5
SPEC_ADHERENCE: N/5
ACTIONABILITY: N/5
```

The weighted score formula from `compute_weighted_score()`: normalize each score as `(score - 1) / 4.0`, then compute weighted average.

A task scoring below 0.50 weighted score triggers a `[CRITIC] PENDING` revision task.

---

## 5. Key Files Reference

| File | Purpose |
|------|---------|
| `~/.boi/src/lib/critic.py` | Prompt assembly, quality gate, result parsing |
| `~/.boi/src/lib/critic_config.py` | Config loading, check discovery |
| `~/.boi/src/templates/checks/quality-scoring.md` | Existing code-quality rubric (18 signals) |
| `~/.boi/critic/custom/` | User custom checks (override or additive) |
| `~/hex/projects/boi-improvements/benchmark/eval/rubric.py` | 6-dimension rubric (correctness/completeness/depth/code_quality/spec_adherence/actionability) |
| `~/hex/projects/boi-improvements/benchmark/eval/prompts/pairwise.py` | `build_scoring_prompt()` for single-response scoring |

---

## 6. Discriminative Power Concern

The spec notes that all-1.00 scores indicate a check without discriminative power. Key design principles for `output-quality.md`:

1. **Anchor to the task spec**: The model must compare the output against the specific `**Spec:**` section, not evaluate in the abstract.
2. **Require concrete justification**: "Score X because requirement Y was missing/present."
3. **Use integer 1–5 scale**: Forces distribution (unlike 0.0–1.0 floats which cluster at extremes).
4. **Weight correctness highest (0.25)**: A wrong answer can't be saved by good formatting.
5. **Test with truncated output**: A check that scores truncated-to-20% output the same as full output is broken.
