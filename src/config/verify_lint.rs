//! Verify-gate lint pass — catches the 9 Tier A verify-command antipatterns
//! before a spec is dispatched. Pure synchronous regex pass over the typed
//! [`Spec`], no I/O, sub-millisecond per spec.
//!
//! The rule catalogue lives inline below (see `RULES`) — no external
//! rulebook file to cross-reference.
//!
//! R7 was added 2026-05-29 after the very next dispatched spec broke on a
//! new antipattern (`cargo check --lib 2>&1 > /tmp/c.log` — wrong redirect
//! order, stderr never reaches the file).
//!
//! R8 was added 2026-06-01 after S5hqge98k's `strip-e2e-test-budget` task
//! was false-failed by following R6's PREVIOUS fix_hint (`|| echo 0`).
//!
//! R9 was added 2026-06-01 after S846zx0gd's `rename-cost-to-tokens-module`
//! task false-failed on `grep -q PATTERN system/harness/src/` — grep against
//! a directory without `-r`/`-R` exits 2 ("Is a directory") regardless of
//! contents. Fourth verify-gate authoring footgun this session that
//! propose_adjustment caught after burning worker time re-running the task.
//!
//! The rules and their fix hints live inline in `RULES` below. Promote to
//! TOML only if tuning demand justifies the extra indirection later.
//!
//! Public surface:
//! - [`lint`] — `pub fn lint(spec: &Spec) -> Vec<Finding>`. Empty Vec means
//!   the spec passes; non-empty means one [`Finding`] per rule hit per
//!   verification.
//! - [`Finding`] — one lint hit: rule id, location, snippet, fix hint.

use std::sync::LazyLock;

use regex::Regex;

use crate::config::Spec;
use crate::types::context::Verification;

/// One lint hit: identifies which rule fired, where, and how to fix it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Stable rule identifier — `R1`..`R6`.
    pub rule_id: String,
    /// Task ref the finding came from, or `None` for spec-level verifications.
    pub task_ref: Option<String>,
    /// Name of the offending verification, if the author supplied one.
    pub verification_name: Option<String>,
    /// The offending command string.
    pub command_snippet: String,
    /// Concrete fix suggestion drawn from the rule definition.
    pub fix_hint: String,
}

/// A single lint rule — its stable id, its detector (a `fn` so each rule can
/// run more than a single regex match where needed), and the fix hint shown
/// to the operator on a hit.
struct Rule {
    /// Stable rule id (`R1`..`R6`).
    id: &'static str,
    /// `true` => rule fires on this command string.
    fires: fn(&str) -> bool,
    /// Fix hint emitted alongside every finding for this rule.
    fix_hint: &'static str,
}

// ─── R1 ───────────────────────────────────────────────────────────────────
// Non-coreutil binary (`cargo `, `npm `, `node `, `npx `, `just `, `gh `,
// `python3 `) appears in the command BEFORE any `export PATH=` substring
// in the same command string. macOS Homebrew installs to `/opt/homebrew/bin`,
// which is not on the default `sh -c` PATH used by `boi`'s worker shell, so
// every spec must export PATH before invoking a non-coreutil binary.

const R1_BINARIES: &[&str] = &[
    "cargo ", "npm ", "node ", "npx ", "just ", "gh ", "python3 ",
];
const R1_PATH_EXPORT: &str = "export PATH=";

fn r1_fires(cmd: &str) -> bool {
    // Index of the FIRST non-coreutil binary occurrence anywhere in `cmd`.
    let binary_pos = R1_BINARIES
        .iter()
        .filter_map(|needle| cmd.find(needle))
        .min();
    let Some(binary_pos) = binary_pos else {
        return false;
    };
    // Rule fires unless `export PATH=` precedes the binary somewhere in the
    // command string. (We do not parse shell — substring order is enough
    // for the "did the author remember to export" check.)
    match cmd.find(R1_PATH_EXPORT) {
        Some(export_pos) => export_pos > binary_pos,
        None => true,
    }
}

// ─── R2 ───────────────────────────────────────────────────────────────────
// `wc -l` without `tr -d ' '` (or `awk '{print $1}'`) in the same command.
// macOS BSD wc pads its numeric output with whitespace, which breaks string
// comparisons against `"0"`, `"1"`, etc.

fn r2_fires(cmd: &str) -> bool {
    if !cmd.contains("wc -l") {
        return false;
    }
    !cmd.contains("tr -d ' '") && !cmd.contains("awk '{print $1}'")
}

// ─── R3 ───────────────────────────────────────────────────────────────────
// `grep -q -v` literal substring — the inverted-flag pair never does what
// the author meant. The correct shape is `! grep -q PATTERN file`.

fn r3_fires(cmd: &str) -> bool {
    cmd.contains("grep -q -v")
}

// ─── R4 ───────────────────────────────────────────────────────────────────
// `python3 -c "..."` whose argument contains a literal newline AND the line
// after the newline starts with whitespace. Python rejects with
// IndentationError when `-c` receives an indented continuation; the fix is
// a `python3 - <<'PYEOF' ... PYEOF` heredoc.

// `expect` is reachable only if the inline regex source is malformed; the
// unit tests below execute the LazyLock initializer, so a typo would surface
// at `cargo test` rather than at runtime.
#[allow(clippy::expect_used)]
static R4_RE: LazyLock<Regex> = LazyLock::new(|| {
    // `python3 -c "` then any chars excluding the closing quote, then a
    // literal newline, then a whitespace start of next line. `[^"]` is a
    // negated class so it matches across newlines by default.
    Regex::new(r#"python3\s+-c\s+"[^"]*\n[ \t]"#).expect("R4 regex compiles")
});

fn r4_fires(cmd: &str) -> bool {
    R4_RE.is_match(cmd)
}

// ─── R5 ───────────────────────────────────────────────────────────────────
// Empty verify command. The string is `""` or trims to empty.

fn r5_fires(cmd: &str) -> bool {
    cmd.trim().is_empty()
}

// ─── R6 ───────────────────────────────────────────────────────────────────
// `grep -c PATTERN file && ...` chain — the bug that motivated the spec.
// `grep -c` exits 1 on zero matches even though it prints `0`, so the `&&`
// short-circuits and the right-hand side never runs.
//
// The recommended safe form is `! grep -q PATTERN file` (boolean check, no
// counting) — see R6's `fix_hint`. The previously-recommended pattern
// `$(... 2>/dev/null || echo 0)` was itself broken and is now caught by R8.

// See R4_RE for the same `expect`-safety rationale.
#[allow(clippy::expect_used)]
static R6_RE: LazyLock<Regex> = LazyLock::new(|| {
    // `grep -c ` then any chars excluding `|`, `(`, `)`, `&` (lazy), then `&&`.
    // The negated class skips command substitutions (`$(... )`) and pipes;
    // those forms — if used in the recommended way — are valid. R8 catches
    // the still-broken `$(grep -c ... || echo 0)` form specifically.
    Regex::new(r"grep -c [^|()&]*?&&").expect("R6 regex compiles")
});

fn r6_fires(cmd: &str) -> bool {
    R6_RE.is_match(cmd)
}

// ─── R7 ───────────────────────────────────────────────────────────────────
// `2>&1 > FILE` (without a pipe between them) — wrong redirect order. Bash
// processes redirects left-to-right: `2>&1` first dups stderr to current fd1
// (which is still the TTY), then `> FILE` redirects only fd1 to the file.
// Stderr still points at the TTY, so the file ends up empty for stderr-only
// output (e.g. cargo's "Finished" status line). The fix swaps the order:
// `> FILE 2>&1` — redirect stdout to the file first, then dup stderr to that
// fd. Added 2026-05-29 after a propose_adjustment phase diagnosed this exact
// bug in the spec that authored this lint module.

// See R4_RE for the same `expect`-safety rationale.
#[allow(clippy::expect_used)]
static R7_RE: LazyLock<Regex> = LazyLock::new(|| {
    // `2>&1` then whitespace then `>` (optionally `>>`) then a non-`&` char.
    // The negated class excludes `2>&1 >&3`-style fd-dup forms, which are
    // intentional and not the antipattern this rule targets. The pipe form
    // `2>&1 | tee FILE` does not match because `\s+>` requires `>` to be the
    // first non-whitespace after `2>&1`.
    Regex::new(r"2>&1\s+>>?[^&]").expect("R7 regex compiles")
});

fn r7_fires(cmd: &str) -> bool {
    R7_RE.is_match(cmd)
}

// ─── R8 ───────────────────────────────────────────────────────────────────
// `grep -c PATTERN file ... || echo 0` — the "fix" that R6's old fix_hint
// recommended is itself broken. `grep -c` ALWAYS prints its count to stdout
// (even `0` on no match) AND exits 1 on zero matches. The `|| echo 0` fires
// on grep's non-zero exit and APPENDS another `0`, so `$()` captures `"0\n0"`
// (embedded newline). `test "$count" -eq "0"` then errors with
// `"integer expression expected"`. The verify can ONLY pass when the file
// is missing entirely (grep exits 2 without emitting; only echo runs).
//
// The correct shapes are:
//   - `! grep -q PATTERN file`   (boolean — preferred)
//   - `count=$(grep -c PATTERN file 2>/dev/null); test "${count:-0}" -eq "0"`
//     (drop the `|| echo 0` — grep already emits `0`)
//
// Added 2026-06-01 after S5hqge98k's `strip-e2e-test-budget` task was
// false-failed by the previous-iteration fix_hint of R6.

#[allow(clippy::expect_used)]
static R8_RE: LazyLock<Regex> = LazyLock::new(|| {
    // `grep -c ` + lazy any-chars + `||` + whitespace + `echo`. The pattern
    // also matches the `2>/dev/null` variant since `2>/dev/null` falls in
    // the lazy-match. Anchored to `echo` so an unrelated `||` doesn't fire.
    Regex::new(r"grep -c .*?\|\|\s*echo").expect("R8 regex compiles")
});

fn r8_fires(cmd: &str) -> bool {
    R8_RE.is_match(cmd)
}

// ─── R9 ───────────────────────────────────────────────────────────────────
// `grep PATTERN dir/` (no `-r` / `-R` / `--recursive`) — POSIX grep refuses
// to descend into a directory without explicit recursion and exits 2
// ("Is a directory") regardless of file contents. The verify ALWAYS fails,
// no matter what the source files look like.
//
// The fix is either to add `-r` (`grep -rq PATTERN dir/`) or to point at a
// specific file (`grep -q PATTERN dir/specific_file.rs`).
//
// Implementation note: regex alone is awkward here because we need both a
// trailing-slash-arg presence check AND an `-r`/`-R` absence check across
// shell-separated grep invocations. Two-pass: split on shell separators,
// inspect each piece that contains `grep` for a recursive flag (token-level)
// and a trailing-slash path (token-level). No regex.

fn r9_fires(cmd: &str) -> bool {
    for piece in cmd.split(['&', '|', ';']) {
        let piece = piece.trim();
        // Only consider pieces that contain a grep invocation. `contains`
        // is loose but the false-positive set (`pgrep`, `bzgrep`, ...) is
        // negligible inside a verify-gate context.
        if !piece.contains("grep") {
            continue;
        }

        // Does this grep have a recursive flag? Short-flag clusters
        // (`-rq`, `-Rn`, `-rcl`, ...) contain `r` or `R`; the long form
        // is `--recursive`. We only inspect tokens starting with `-`.
        let mut has_recursive = false;
        for tok in piece.split_whitespace() {
            if tok == "--recursive" {
                has_recursive = true;
                break;
            }
            if let Some(rest) = tok.strip_prefix("-") {
                // Skip long options other than --recursive (handled above).
                if rest.starts_with('-') {
                    continue;
                }
                if rest.contains('r') || rest.contains('R') {
                    has_recursive = true;
                    break;
                }
            }
        }
        if has_recursive {
            continue;
        }

        // Look for a positional arg ending in `/`. Strip trailing shell
        // punctuation first so command-substitution wrappers like
        // `$(grep PAT dir/)` still match — without the strip, the token
        // would end in `)` and the check would miss it.
        for tok in piece.split_whitespace() {
            let trimmed = tok.trim_end_matches([')', ',', '"', '\'']);
            if trimmed.ends_with('/') && trimmed != "/" {
                return true;
            }
        }
    }
    false
}

// ─── Rule registry ────────────────────────────────────────────────────────

const RULES: &[Rule] = &[
    Rule {
        id: "R1",
        fires: r1_fires,
        fix_hint: r#"prepend export PATH="/opt/homebrew/bin:/usr/bin:/bin:$PATH" && "#,
    },
    Rule {
        id: "R2",
        fires: r2_fires,
        fix_hint: r"wrap with $(... | wc -l | tr -d ' ')",
    },
    Rule {
        id: "R3",
        fires: r3_fires,
        fix_hint: "use ! grep -q PATTERN file",
    },
    Rule {
        id: "R4",
        fires: r4_fires,
        fix_hint: "use python3 - <<'PYEOF' ... PYEOF heredoc instead",
    },
    Rule {
        id: "R5",
        fires: r5_fires,
        fix_hint: "a verify must run something — drop the verification entry or write a real check",
    },
    Rule {
        id: "R6",
        fires: r6_fires,
        fix_hint: "use ! grep -q PATTERN file (boolean check, no counting)",
    },
    Rule {
        id: "R7",
        fires: r7_fires,
        fix_hint: "swap redirect order: > FILE 2>&1 (redirect stdout to file first, then dup stderr to that fd)",
    },
    Rule {
        id: "R8",
        fires: r8_fires,
        fix_hint: "use ! grep -q PATTERN file — `grep -c ... || echo 0` captures \"0\\n0\" (grep prints 0 then echo prints 0); test errors with \"integer expression expected\"",
    },
    Rule {
        id: "R9",
        fires: r9_fires,
        fix_hint: "add -r (`grep -rq PATTERN dir/`) or target a specific file (`grep -q PATTERN dir/file`) — directory grep without recursion exits 2 unconditionally",
    },
];

/// Max length of `command_snippet` in a [`Finding`]. Keeps the rendered
/// dispatch-error block readable — anything longer wraps and obscures the
/// rule id + fix hint. The snippet is for orientation; the operator opens
/// the spec file to see the full command.
const SNIPPET_MAX: usize = 120;

/// Truncate the command to a single line of at most [`SNIPPET_MAX`] chars,
/// adding `…` if anything was elided. Newlines become spaces so multi-line
/// commands render as a single snippet line.
fn snippet(cmd: &str) -> String {
    let one_line: String = cmd
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    if one_line.chars().count() <= SNIPPET_MAX {
        one_line
    } else {
        let truncated: String = one_line.chars().take(SNIPPET_MAX).collect();
        format!("{truncated}…")
    }
}

/// Apply every rule to one verification's command string, returning a
/// [`Finding`] per rule that fires (a single command can violate more than
/// one rule).
fn lint_command(
    cmd: &str,
    task_ref: Option<&str>,
    verification_name: Option<&str>,
) -> Vec<Finding> {
    RULES
        .iter()
        .filter(|rule| (rule.fires)(cmd))
        .map(|rule| Finding {
            rule_id: rule.id.to_string(),
            task_ref: task_ref.map(str::to_string),
            verification_name: verification_name.map(str::to_string),
            command_snippet: snippet(cmd),
            fix_hint: rule.fix_hint.to_string(),
        })
        .collect()
}

/// Pull the `(name, command)` out of a [`Verification`] if it is the
/// `Command` variant; the `Intent` variant is LLM-judged and out of scope.
fn as_command(v: &Verification) -> Option<(Option<&str>, &str)> {
    match v {
        Verification::Command { name, command } => Some((name.as_deref(), command.as_str())),
        Verification::Intent { .. } => None,
    }
}

/// Scan every `Command` verification in the spec — spec-level (`task_ref =
/// None`) and per-task — and return all findings in walk order. An empty Vec
/// means the spec passes the lint.
pub fn lint(spec: &Spec) -> Vec<Finding> {
    let mut findings = Vec::new();

    // Spec-level verifications first.
    for v in &spec.contract.verifications {
        if let Some((name, cmd)) = as_command(v) {
            findings.extend(lint_command(cmd, None, name));
        }
    }

    // Then per-task verifications, in declared order.
    for task in &spec.tasks {
        let task_ref = task.task_ref.as_deref();
        for v in &task.verifications {
            if let Some((name, cmd)) = as_command(v) {
                findings.extend(lint_command(cmd, task_ref, name));
            }
        }
    }

    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::spec::{Delivery, Spec, TaskDef};
    use crate::types::context::{SpecContract, Verification};
    use std::path::PathBuf;

    /// Build a `Spec` whose single task has one `Command` verification with
    /// the supplied command string. Everything else is filler — the lint only
    /// looks at `command` strings.
    fn spec_with_task_command(cmd: &str) -> Spec {
        Spec {
            title: "lint-fixture".into(),
            pipeline: "standard".into(),
            delivery: Delivery::Merge,
            contract: SpecContract {
                scope: "verify-lint test".into(),
                workspace: PathBuf::from("/tmp/fixture"),
                base_branch: "v2".into(),
                exclusions: vec![],
                verifications: vec![],
                must_emit: vec![],
            },
            tasks: vec![TaskDef {
                task_ref: Some("t1".into()),
                behavior: "fixture task".into(),
                blocked_by: vec![],
                verifications: vec![Verification::Command {
                    name: Some("v1".into()),
                    command: cmd.into(),
                }],
            }],
            authored_decisions: vec![],
            skills: vec![],
        }
    }

    /// Helper: did the lint fire the named rule on at least one finding?
    fn fired(findings: &[Finding], rule: &str) -> bool {
        findings.iter().any(|f| f.rule_id == rule)
    }

    // ─── R1 ──────────────────────────────────────────────────────────────
    // Non-coreutil binary (cargo/npm/node/npx/just/gh/python3) without an
    // `export PATH=` substring earlier in the same command.

    #[test]
    fn test_r1_fail_bare_cargo_command() {
        let spec = spec_with_task_command("cargo check");
        let findings = lint(&spec);
        assert!(
            fired(&findings, "R1"),
            "R1 should fire on a bare `cargo` command without a PATH export, got {findings:?}"
        );
    }

    #[test]
    fn test_r1_pass_path_exported_first() {
        let spec = spec_with_task_command(
            "export PATH=\"/opt/homebrew/bin:/usr/bin:/bin:$PATH\" && cargo check",
        );
        let findings = lint(&spec);
        assert!(
            !fired(&findings, "R1"),
            "R1 must NOT fire when `export PATH=` precedes the binary, got {findings:?}"
        );
    }

    // ─── R2 ──────────────────────────────────────────────────────────────
    // `wc -l` not followed (in the same command) by `tr -d ' '` or
    // `awk '{print $1}'` — macOS BSD wc pads its output with whitespace.

    #[test]
    fn test_r2_fail_wc_l_unstripped() {
        let spec = spec_with_task_command("cat foo.txt | wc -l");
        let findings = lint(&spec);
        assert!(
            fired(&findings, "R2"),
            "R2 should fire on bare `wc -l` without `tr -d ' '`, got {findings:?}"
        );
    }

    #[test]
    fn test_r2_pass_wc_l_with_tr() {
        let spec = spec_with_task_command("cat foo.txt | wc -l | tr -d ' '");
        let findings = lint(&spec);
        assert!(
            !fired(&findings, "R2"),
            "R2 must NOT fire when `tr -d ' '` strips wc output, got {findings:?}"
        );
    }

    // ─── R3 ──────────────────────────────────────────────────────────────
    // `grep -q -v` literal substring — the inverted-flag pair never does
    // what the author meant. The correct shape is `! grep -q PATTERN file`.

    #[test]
    fn test_r3_fail_grep_q_minus_v() {
        let spec = spec_with_task_command("grep -q -v PATTERN file");
        let findings = lint(&spec);
        assert!(
            fired(&findings, "R3"),
            "R3 should fire on `grep -q -v`, got {findings:?}"
        );
    }

    #[test]
    fn test_r3_pass_negated_grep_q() {
        let spec = spec_with_task_command("! grep -q PATTERN file");
        let findings = lint(&spec);
        assert!(
            !fired(&findings, "R3"),
            "R3 must NOT fire on the negated form `! grep -q ...`, got {findings:?}"
        );
    }

    // ─── R4 ──────────────────────────────────────────────────────────────
    // `python3 -c "..."` whose argument contains a newline AND the next
    // line starts with whitespace — Python rejects indented continuations
    // with IndentationError when -c is the source.

    #[test]
    fn test_r4_fail_python3_dash_c_indented_newline() {
        // Literal newline followed by indented continuation inside -c.
        let spec = spec_with_task_command("python3 -c \"import sys\n    sys.exit(0)\"");
        let findings = lint(&spec);
        assert!(
            fired(&findings, "R4"),
            "R4 should fire on python3 -c with newline + indented continuation, got {findings:?}"
        );
    }

    #[test]
    fn test_r4_pass_python3_heredoc() {
        // The recommended replacement: a heredoc instead of -c.
        let spec = spec_with_task_command("python3 - <<'PYEOF'\nimport sys\nsys.exit(0)\nPYEOF");
        let findings = lint(&spec);
        assert!(
            !fired(&findings, "R4"),
            "R4 must NOT fire on a python3 heredoc, got {findings:?}"
        );
    }

    // ─── R5 ──────────────────────────────────────────────────────────────
    // Empty verify command. An empty string (or whitespace-only) verify is
    // never meaningful — drop the entry or write a real check.

    #[test]
    fn test_r5_fail_empty_command() {
        let spec = spec_with_task_command("");
        let findings = lint(&spec);
        assert!(
            fired(&findings, "R5"),
            "R5 should fire on an empty verify command, got {findings:?}"
        );
    }

    #[test]
    fn test_r5_pass_real_command() {
        let spec = spec_with_task_command("echo ok");
        let findings = lint(&spec);
        assert!(
            !fired(&findings, "R5"),
            "R5 must NOT fire on a non-empty verify command, got {findings:?}"
        );
    }

    // ─── R6 ──────────────────────────────────────────────────────────────
    // `grep -c PATTERN file && ...` — grep -c exits 1 on zero matches even
    // though it prints `0`, so the && short-circuits. This is the actual bug
    // that motivated the whole spec (incident: 2026-05-29).

    #[test]
    fn test_r6_fail_grep_c_and_chain() {
        let spec = spec_with_task_command("grep -c FOO bar && echo ok");
        let findings = lint(&spec);
        assert!(
            fired(&findings, "R6"),
            "R6 should fire on `grep -c ... && ...`, got {findings:?}"
        );
    }

    #[test]
    fn test_r6_pass_negated_boolean_grep() {
        // Recommended replacement — boolean check, no counting.
        let spec = spec_with_task_command("! grep -q FOO bar");
        let findings = lint(&spec);
        assert!(
            !fired(&findings, "R6"),
            "R6 must NOT fire on the recommended `! grep -q` form, got {findings:?}"
        );
    }

    // ─── R7 ──────────────────────────────────────────────────────────────
    // `2>&1 > FILE` (without a pipe between them) — wrong redirect order, so
    // stderr never reaches the file. The fix is `> FILE 2>&1`. Added 2026-05-29
    // after this exact bug killed the spec that produced this lint.

    #[test]
    fn test_r7_fail_redirect_after_stderr_dup() {
        let spec =
            spec_with_task_command("cargo check --lib 2>&1 > /tmp/c.log && grep -q ok /tmp/c.log");
        let findings = lint(&spec);
        assert!(
            fired(&findings, "R7"),
            "R7 should fire on `2>&1 > FILE` (wrong redirect order), got {findings:?}"
        );
    }

    #[test]
    fn test_r7_fail_redirect_append_after_stderr_dup() {
        // The same bug shape with `>>` (append) instead of `>` (truncate).
        let spec =
            spec_with_task_command("cargo check --lib 2>&1 >> /tmp/c.log && grep -q ok /tmp/c.log");
        let findings = lint(&spec);
        assert!(
            fired(&findings, "R7"),
            "R7 should fire on `2>&1 >> FILE` too, got {findings:?}"
        );
    }

    #[test]
    fn test_r7_pass_correct_redirect_order() {
        // The recommended replacement: stdout to file first, then dup stderr
        // to that fd. Now the file captures both streams.
        let spec =
            spec_with_task_command("cargo check --lib > /tmp/c.log 2>&1 && grep -q ok /tmp/c.log");
        let findings = lint(&spec);
        assert!(
            !fired(&findings, "R7"),
            "R7 must NOT fire on `> FILE 2>&1`, got {findings:?}"
        );
    }

    #[test]
    fn test_r7_pass_pipe_between_dup_and_command() {
        // `2>&1 | tee FILE` is fine — the pipe merges and captures both.
        let spec = spec_with_task_command("cargo check --lib 2>&1 | tee /tmp/c.log");
        let findings = lint(&spec);
        assert!(
            !fired(&findings, "R7"),
            "R7 must NOT fire when a pipe separates `2>&1` and the next stage, got {findings:?}"
        );
    }

    #[test]
    fn test_r7_pass_fd_dup_after_stderr_dup() {
        // `2>&1 >&3` is an intentional fd-dup form, not the redirect-order
        // bug. R7's regex requires a non-`&` char after `>` to exclude this.
        let spec = spec_with_task_command("cmd 2>&1 >&3");
        let findings = lint(&spec);
        assert!(
            !fired(&findings, "R7"),
            "R7 must NOT fire on `2>&1 >&3` fd-dup form, got {findings:?}"
        );
    }

    // ─── R8 ──────────────────────────────────────────────────────────────
    // `grep -c PATTERN file ... || echo 0` — the broken "fix" that R6's
    // previous fix_hint mandated. False-failed three of my own dispatched
    // specs this session before I caught it.

    #[test]
    fn test_r8_fail_grep_c_or_echo_zero() {
        let spec = spec_with_task_command(
            "count=$(grep -c FOO bar 2>/dev/null || echo 0); test \"$count\" -eq \"0\"",
        );
        let findings = lint(&spec);
        assert!(
            fired(&findings, "R8"),
            "R8 should fire on `grep -c ... || echo 0` (broken old R6 fix), got {findings:?}"
        );
    }

    #[test]
    fn test_r8_fail_grep_c_or_echo_zero_no_devnull() {
        // Same bug without `2>/dev/null` — must still fire.
        let spec =
            spec_with_task_command("count=$(grep -c FOO bar || echo 0); test \"$count\" -eq \"0\"");
        let findings = lint(&spec);
        assert!(
            fired(&findings, "R8"),
            "R8 should fire even without 2>/dev/null suppressor, got {findings:?}"
        );
    }

    #[test]
    fn test_r8_pass_boolean_negated_grep() {
        let spec = spec_with_task_command("! grep -q FOO bar");
        let findings = lint(&spec);
        assert!(
            !fired(&findings, "R8"),
            "R8 must NOT fire on the recommended `! grep -q` form, got {findings:?}"
        );
    }

    #[test]
    fn test_r8_pass_grep_c_without_or_echo() {
        // Drop the `|| echo 0` — grep already emits its count to stdout.
        let spec = spec_with_task_command(
            "count=$(grep -c FOO bar 2>/dev/null); test \"${count:-0}\" -eq \"0\"",
        );
        let findings = lint(&spec);
        assert!(
            !fired(&findings, "R8"),
            "R8 must NOT fire when the bogus `|| echo 0` is removed, got {findings:?}"
        );
    }

    // ─── R9 ──────────────────────────────────────────────────────────────
    // `grep PATTERN dir/` (no -r/-R/--recursive) — grep refuses to descend
    // into a directory and exits 2 ("Is a directory"). Fourth verify-gate
    // self-bite of the 2026-06-01 session — added to prevent the fifth.

    #[test]
    fn test_r9_fail_grep_q_against_directory() {
        let spec = spec_with_task_command("grep -q 'pub mod tokens' system/harness/src/");
        let findings = lint(&spec);
        assert!(
            fired(&findings, "R9"),
            "R9 should fire on `grep -q PATTERN dir/` (no -r), got {findings:?}"
        );
    }

    #[test]
    fn test_r9_fail_grep_c_against_directory() {
        let spec = spec_with_task_command("count=$(grep -c PATTERN src/); test \"$count\" -gt 0");
        let findings = lint(&spec);
        assert!(
            fired(&findings, "R9"),
            "R9 should fire on `grep -c PATTERN dir/` (no -r), got {findings:?}"
        );
    }

    #[test]
    fn test_r9_pass_recursive_short_flag() {
        let spec = spec_with_task_command("grep -rq PATTERN system/harness/src/");
        let findings = lint(&spec);
        assert!(
            !fired(&findings, "R9"),
            "R9 must NOT fire when -r is in a short-flag cluster, got {findings:?}"
        );
    }

    // `capital_R` names the grep `-R` flag under test — meaningful, so allow
    // the non-snake-case rather than rename it to a lossy `_r`.
    #[allow(non_snake_case)]
    #[test]
    fn test_r9_pass_recursive_capital_R() {
        let spec = spec_with_task_command("grep -R PATTERN dir/");
        let findings = lint(&spec);
        assert!(
            !fired(&findings, "R9"),
            "R9 must NOT fire on `grep -R`, got {findings:?}"
        );
    }

    #[test]
    fn test_r9_pass_recursive_long_flag() {
        let spec = spec_with_task_command("grep --recursive PATTERN dir/");
        let findings = lint(&spec);
        assert!(
            !fired(&findings, "R9"),
            "R9 must NOT fire on `grep --recursive`, got {findings:?}"
        );
    }

    #[test]
    fn test_r9_pass_specific_file() {
        // Target a file, not a directory — no trailing slash.
        let spec = spec_with_task_command("grep -q 'pub mod tokens' system/harness/src/lib.rs");
        let findings = lint(&spec);
        assert!(
            !fired(&findings, "R9"),
            "R9 must NOT fire when targeting a specific file, got {findings:?}"
        );
    }

    #[test]
    fn test_r9_pass_command_without_grep() {
        let spec = spec_with_task_command("ls system/harness/src/");
        let findings = lint(&spec);
        assert!(
            !fired(&findings, "R9"),
            "R9 must NOT fire on commands without grep, got {findings:?}"
        );
    }

    // ─── Walk coverage ───────────────────────────────────────────────────
    // The lint must visit BOTH spec-level and per-task verifications, and
    // SKIP the `Intent` variant (LLM-judged, out of scope).

    #[test]
    fn test_walks_spec_level_verifications() {
        let mut spec = spec_with_task_command("echo ok");
        // Inject a spec-level offender (bare `cargo`) into the contract.
        spec.contract.verifications.push(Verification::Command {
            name: Some("contract-cargo".into()),
            command: "cargo check".into(),
        });
        let findings = lint(&spec);
        let r1 = findings.iter().find(|f| f.rule_id == "R1").expect("R1");
        assert!(
            r1.task_ref.is_none(),
            "spec-level finding must have task_ref = None, got {:?}",
            r1.task_ref
        );
        assert_eq!(r1.verification_name.as_deref(), Some("contract-cargo"));
    }

    #[test]
    fn test_intent_variant_is_skipped() {
        let mut spec = spec_with_task_command("echo ok");
        // An `Intent` verification with text that WOULD trigger R1 if it
        // were checked. The lint must skip it.
        spec.tasks[0].verifications.push(Verification::Intent {
            name: Some("intent-skip".into()),
            intent: "cargo check passes".into(),
        });
        let findings = lint(&spec);
        assert!(
            findings.is_empty(),
            "Intent verifications must be skipped; got {findings:?}"
        );
    }
}
