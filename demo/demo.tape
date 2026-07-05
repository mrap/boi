# demo/demo.tape — the BOI "money shot," <90s.
#
# Run with: vhs demo/demo.tape   (from the repo root; produces demo/demo.gif)
#
# This tape is 100% offline and credential-free by design:
#   - No `boi daemon` is started, so `boi dispatch` never reaches preflight
#     (no `goose` version/credential check happens at all).
#   - `boi dispatch` still parses, lints, and PERSISTS the spec — it just
#     can't hand it to a daemon that isn't running, so it exits non-zero
#     with a `NoDaemon` message. That's real, intentional behavior (see
#     docs/architecture.md — "fail loud, no daemon → no silent write"), not
#     a recording bug.
#   - `boi dashboard` is read-only: it opens the persisted spec straight out
#     of SQLite and renders the real 3-task DAG, queued and blocked exactly
#     as dispatch left it.
#
# What this tape does NOT show, and needs Mike physically present for:
#   - Starting `boi daemon` with a real `~/.boi/v2/secrets/claude.env`
#     (a live Claude Code OAuth token) and a real `goose` on PATH.
#   - Re-running `boi dispatch demo/spec.toml` against that live daemon, so
#     preflight passes and the pipeline actually executes: workers write
#     real code in real worktrees, verify gates run for real, and the
#     dashboard's phase/task rows advance live (queued → active → passing)
#     instead of sitting at queued/blocked.
#   - That live run is the recording worth publishing; this tape is the
#     safe, reproducible half of it. See demo/README.md.

Output "demo/demo.gif"

Set Shell "bash"
Set FontSize 18
Set Width 1200
Set Height 720
Set Padding 20
Set TypingSpeed 40ms

Type "# BOI: dispatch a DAG of coding tasks from one TOML spec"
Enter
Sleep 1s

Type "cat demo/spec.toml"
Enter
Sleep 3500ms

Type "# a real workspace this spec's base_branch/policy checks can open"
Enter
Sleep 500ms
Type "rm -rf /tmp/boi-demo-workspace && mkdir -p /tmp/boi-demo-workspace && (cd /tmp/boi-demo-workspace && git init -q -b main && git commit -q --allow-empty -m init)"
Enter
Sleep 1s

Type "# dispatch parses, validates, and lints the spec before persisting it"
Enter
Type "boi dispatch demo/spec.toml"
Enter
Sleep 2500ms
Type "# ^ persisted + queued. No daemon running in this recording, so it stops"
Enter
Type "#   there rather than silently pretending to start — see demo/README.md"
Enter
Sleep 1500ms

Type "boi dashboard"
Enter
Sleep 5s

# The dashboard is an interactive TUI (ratatui) — hold on the DAG view long
# enough to read it: the spec `queued`, 3 tasks, `document-endpoint` blocked
# on the other two.
Sleep 5s

Type "q"
