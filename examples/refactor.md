# Refactor Hello BOI

<!-- Example spec: Demonstrates how BOI works with existing code. Takes the
     hello-boi.sh script (output of the hello-world example) and refactors it
     into a more capable greeting tool. Run the hello-world example first.
     Expected completion: 3 iterations (~5-8 minutes) -->

Refactor the `hello-boi.sh` script from the hello-world example into a proper greeting tool with multiple output formats and a test suite. This spec assumes `hello-boi.sh` and `test_hello.sh` already exist in the project root.

## Tasks

### t-1: Convert shell script to Python
PENDING

**Spec:** Rewrite `hello-boi.sh` as `greet.py` in Python:

1. Preserve existing behavior: no args prints `Hello, BOI!`, one arg prints `Hello, <arg>!`
2. Add `--format` flag with options:
   - `text` (default): `Hello, Alice!`
   - `json`: `{"greeting": "Hello", "name": "Alice"}`
   - `upper`: `HELLO, ALICE!`
3. Add `--help` flag using argparse
4. Keep `hello-boi.sh` as a wrapper that calls `python3 greet.py "$@"`
5. Python stdlib only

**Verify:** `python3 greet.py` prints `Hello, BOI!`. `python3 greet.py Alice --format json` prints valid JSON. `bash hello-boi.sh` still works via the wrapper.

### t-2: Write comprehensive tests
PENDING

**Spec:** Create `test_greet.py` using `unittest`:

1. Test default greeting (no args) returns `Hello, BOI!`
2. Test named greeting returns `Hello, <name>!`
3. Test JSON format outputs valid JSON with correct fields
4. Test upper format outputs uppercase
5. Test the shell wrapper `hello-boi.sh` still produces correct output
6. Test edge cases: empty string arg, arg with spaces (quoted)

**Verify:** `python3 -m unittest test_greet -v` shows all tests passing.

### t-3: Add batch greeting mode
PENDING

**Spec:** Add ability to greet multiple names:

1. `python3 greet.py Alice Bob Charlie` prints one greeting per line
2. `python3 greet.py --format json Alice Bob` prints a JSON array of greeting objects
3. Reading from stdin: `echo -e "Alice\nBob" | python3 greet.py --stdin` greets each line
4. Update tests for batch mode

**Verify:** `python3 greet.py Alice Bob` prints 2 lines. Piping 3 names via stdin produces 3 greetings. Tests pass.
