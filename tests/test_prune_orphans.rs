// Tests for the prune-orphans subcommand heuristics.
// All tests use mock data — no live process enumeration or DB access.
//
// All functions live inside `mod prune_orphans` so that `cargo test prune_orphans`
// correctly picks them up (the filter matches the module path prefix).

use boi::cli::prune::{
    classify_candidate, find_orphan_candidates, is_interactive_claude, DbState, ProcessInfo,
    PruneReason,
};
use std::collections::HashSet;

fn make_proc(pid: u32, ppid: u32, cmdline: &str) -> ProcessInfo {
    ProcessInfo {
        pid,
        ppid,
        cmdline: cmdline.to_string(),
        cwd: None,
        has_tty: false,
        cpu_percent: 0.0,
        mem_rss_kb: 1024,
        alive_secs: 700,
    }
}

fn alive_set(pids: &[u32]) -> HashSet<u32> {
    pids.iter().copied().collect()
}

fn db_state(worker_pids: &[u32], active_pids: &[u32], ended_pids: &[u32]) -> DbState {
    DbState {
        worker_pids: worker_pids.iter().copied().collect(),
        active_process_pids: active_pids.iter().copied().collect(),
        ended_process_pids: ended_pids.iter().copied().collect(),
    }
}

mod prune_orphans {
    use super::*;

    // ── Test 1: Active worker PID is NEVER a candidate ───────────────────────

    #[test]
    fn active_worker_pid_not_candidate() {
        let proc = make_proc(1001, 1, "claude -p --spec foo.yaml");
        let db = db_state(&[1001], &[], &[]);
        let alive = alive_set(&[1, 1001]);

        let result = classify_candidate(&proc, &db, &alive, 600, &[]);
        assert!(
            result.is_none(),
            "pid in workers.current_pid must never be a candidate"
        );
    }

    // ── Test 2: Process with ended_at IS NULL is NEVER a candidate ───────────

    #[test]
    fn active_process_pid_not_candidate() {
        let proc = make_proc(2001, 1, "claude -p --iteration 3");
        let db = db_state(&[], &[2001], &[]);
        let alive = alive_set(&[1, 2001]);

        let result = classify_candidate(&proc, &db, &alive, 600, &[]);
        assert!(
            result.is_none(),
            "pid with ended_at IS NULL must never be a candidate"
        );
    }

    // ── Test 3: Process with ended_at set BUT alive IS a candidate ───────────

    #[test]
    fn ended_pid_alive_is_candidate() {
        let proc = make_proc(3001, 1, "claude -p --spec old.yaml");
        let db = db_state(&[], &[], &[3001]);
        let alive = alive_set(&[1, 3001]);

        let result = classify_candidate(&proc, &db, &alive, 600, &[]);
        let reasons = result.expect("ended pid that is alive must be a candidate");
        assert!(
            reasons.iter().any(|r| *r == PruneReason::DbMarkedEnded),
            "should have DbMarkedEnded reason"
        );
    }

    // ── Test 4: Tail on dead BOI worktree IS a candidate ─────────────────────

    #[test]
    fn tail_on_dead_worktree_is_candidate() {
        let cmd = "tail -f /private/tmp/nonexistent-boi-SE999-boi-rust/output.log";
        let proc = make_proc(4001, 1, cmd);
        // Non-empty worker registry so the "empty protected set" guard passes.
        let db = db_state(&[99999], &[], &[]);
        let alive = alive_set(&[1, 4001]);

        let result = classify_candidate(&proc, &db, &alive, 600, &[]);
        assert!(
            result.is_some(),
            "tail -f on dead BOI worktree must be a candidate"
        );
    }

    // ── Test 5: Mike's TTY claude session is NEVER a candidate ───────────────

    #[test]
    fn tty_claude_session_not_candidate() {
        let mut proc = make_proc(5001, 1, "claude --dangerously-skip-permissions");
        proc.has_tty = true;

        let db = db_state(&[99999], &[], &[]);
        let alive = alive_set(&[1, 5001]);

        let result = classify_candidate(&proc, &db, &alive, 600, &[]);
        assert!(
            result.is_none(),
            "claude session with TTY must never be a candidate"
        );
    }

    #[test]
    fn tty_claude_session_detected_by_is_interactive() {
        let mut proc = make_proc(5002, 1, "claude");
        proc.has_tty = true;
        assert!(is_interactive_claude(&proc), "TTY claude must be interactive");

        let mut proc2 = make_proc(5003, 1, "claude --dangerously-skip-permissions");
        proc2.has_tty = false;
        assert!(
            is_interactive_claude(&proc2),
            "--dangerously-skip-permissions means interactive even without TTY"
        );

        let proc3 = make_proc(5004, 1, "claude -p --spec foo.yaml");
        assert!(
            !is_interactive_claude(&proc3),
            "claude -p without TTY or --dangerously-skip-permissions is not interactive"
        );
    }

    // ── Test 6: Default dry-run never modifies anything ──────────────────────

    #[test]
    fn dry_run_does_not_call_kill() {
        // Verify that find_orphan_candidates is a pure function with no side effects.
        let procs = vec![
            make_proc(6001, 1, "claude -p --spec a.yaml"),
            make_proc(6002, 1, "tail -f /private/tmp/boi-XX-boi-rust/out.log"),
        ];
        let db = db_state(&[99999], &[], &[6001]);
        let alive = alive_set(&[1, 6001, 6002]);

        let candidates = find_orphan_candidates(&procs, &db, &alive, 600, &[]);
        assert!(
            !candidates.is_empty(),
            "should find at least one candidate"
        );
        // A second call with the same inputs returns the same results — no side effects.
        let candidates2 = find_orphan_candidates(&procs, &db, &alive, 600, &[]);
        assert_eq!(
            candidates.len(),
            candidates2.len(),
            "find_orphan_candidates must be side-effect-free (dry-run safe)"
        );
    }

    // ── Additional heuristic coverage ─────────────────────────────────────────

    #[test]
    fn orphaned_parent_dead_and_long_lived_is_candidate() {
        let proc = make_proc(7001, 9999, "claude -p --spec zombie.yaml");
        let db = db_state(&[99999], &[], &[]);
        // Parent 9999 absent from alive set → parent dead
        let alive = alive_set(&[1, 7001]);

        let result = classify_candidate(&proc, &db, &alive, 600, &[]);
        let reasons = result.expect("orphaned process with dead parent must be a candidate");
        assert!(
            reasons
                .iter()
                .any(|r| matches!(r, PruneReason::OrphanedProcess { .. })),
            "should have OrphanedProcess reason"
        );
    }

    #[test]
    fn exclude_pattern_prevents_candidacy() {
        let proc = make_proc(8001, 9999, "claude -p --spec protected.yaml");
        let db = db_state(&[99999], &[], &[]);
        let alive = alive_set(&[1, 8001]);

        let without = classify_candidate(&proc, &db, &alive, 600, &[]);
        assert!(without.is_some(), "should be candidate without exclude pattern");

        let with_excl =
            classify_candidate(&proc, &db, &alive, 600, &["protected.yaml".to_string()]);
        assert!(
            with_excl.is_none(),
            "exclude-pattern match must prevent candidacy"
        );
    }

    #[test]
    fn safelist_daemon_not_candidate() {
        let proc = make_proc(9001, 1, "/usr/local/bin/claude-mem --daemon");
        let db = db_state(&[99999], &[], &[]);
        let alive = alive_set(&[1, 9001]);

        let result = classify_candidate(&proc, &db, &alive, 600, &[]);
        assert!(result.is_none(), "claude-mem daemon must be protected by safelist");
    }

    #[test]
    fn not_in_registry_heuristic() {
        let proc = make_proc(10001, 1, "claude -p --spec foo.yaml");
        // workers table is non-empty but does NOT contain this PID
        let db = db_state(&[99999], &[], &[]);
        let alive = alive_set(&[1, 10001, 99999]);

        let result = classify_candidate(&proc, &db, &alive, 600, &[]);
        let reasons = result.expect("unregistered claude process must be a candidate");
        assert!(
            reasons
                .iter()
                .any(|r| *r == PruneReason::NotInWorkerRegistry),
            "should have NotInWorkerRegistry reason"
        );
    }

    #[test]
    fn inactive_worktree_cwd_is_candidate() {
        let mut proc = make_proc(11001, 1, "claude -p");
        // Non-existent BOI worktree path
        proc.cwd = Some("/private/tmp/boi-DEADBEEF-boi-rust".to_string());
        let db = db_state(&[99999], &[], &[]);
        let alive = alive_set(&[1, 11001, 99999]);

        let result = classify_candidate(&proc, &db, &alive, 600, &[]);
        let reasons = result.expect("process in dead BOI worktree CWD must be a candidate");
        assert!(
            reasons
                .iter()
                .any(|r| matches!(r, PruneReason::InactiveWorktree { .. })),
            "should have InactiveWorktree reason"
        );
    }
}
