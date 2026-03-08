# Hello BOI

<!-- Example spec: The simplest possible BOI spec. Creates a "Hello BOI" script
     and a test for it. Great for verifying your install works.
     Expected completion: 2 iterations (~2-3 minutes) -->

A minimal spec that creates a greeting script and verifies it works. Use this to confirm BOI is installed and running correctly.

## Tasks

### t-1: Create the hello script
PENDING

**Spec:** Create a file called `hello-boi.sh` in the project root with the following behavior:

1. When run with no arguments, print `Hello, BOI!` to stdout
2. When run with a name argument, print `Hello, <name>!` (e.g., `./hello-boi.sh World` prints `Hello, World!`)
3. The script must be executable (`chmod +x`)
4. Use `#!/usr/bin/env bash` as the shebang
5. Use `set -uo pipefail` for safety

**Verify:** `bash hello-boi.sh` prints `Hello, BOI!`. `bash hello-boi.sh Alice` prints `Hello, Alice!`. Exit code is 0 for both.

### t-2: Add tests for the hello script
PENDING

**Spec:** Create `test_hello.sh` that validates the hello script:

1. Test default output: `bash hello-boi.sh` outputs exactly `Hello, BOI!`
2. Test named output: `bash hello-boi.sh World` outputs exactly `Hello, World!`
3. Test exit code: both invocations return exit code 0
4. Print `PASS` or `FAIL` for each test case
5. Exit with code 1 if any test fails, 0 if all pass

**Verify:** `bash test_hello.sh` prints 3x PASS and exits with code 0.
