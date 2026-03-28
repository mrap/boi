"""Built-in gate registry for BOI guardrails."""

from . import diff_check, lint_pass, no_secrets, tests_pass, verify_commands

BUILTIN_GATES = {
    "verify-commands-pass": verify_commands.run,
    "diff-is-non-empty": diff_check.run,
    "tests-pass": tests_pass.run,
    "lint-pass": lint_pass.run,
    "no-secrets": no_secrets.run,
}
