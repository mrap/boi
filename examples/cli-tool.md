# CLI Todo Manager

<!-- Example spec: A medium-complexity spec that builds a command-line todo list
     manager in Python. Demonstrates task dependencies and self-evolution.
     Expected completion: 5 iterations (~10-15 minutes) -->

Build a simple command-line todo list manager in Python (stdlib only) that stores tasks in a JSON file. Supports adding, listing, completing, and deleting tasks.

## Tasks

### t-1: Create the core data model and storage
PENDING

**Spec:** Create `todo.py` with the core todo manager:

1. A `Todo` dataclass with fields: `id` (int), `text` (str), `done` (bool), `created_at` (ISO 8601 string)
2. A `TodoStore` class that:
   - Loads/saves todos from a JSON file (default: `~/.todos.json`)
   - Accepts a custom file path for testing
   - `add(text) -> Todo` creates a new todo with auto-incrementing ID
   - `list_all() -> list[Todo]` returns all todos
   - `complete(id) -> bool` marks a todo as done, returns False if not found
   - `delete(id) -> bool` removes a todo, returns False if not found
3. All imports must be stdlib only (`json`, `dataclasses`, `datetime`, `pathlib`)

**Verify:** `python3 -c "from todo import TodoStore; s = TodoStore('/tmp/test-todos.json'); t = s.add('test'); print(t.text, t.done)"` prints `test False`.

**Self-evolution:** If additional fields are needed (priority, due date), add a task.

### t-2: Build the CLI interface
PENDING

**Spec:** Add a `if __name__ == "__main__"` block to `todo.py` with argparse:

Commands:
- `python3 todo.py add "Buy groceries"` - adds a todo, prints confirmation
- `python3 todo.py list` - shows all todos (numbered, with done/pending status)
- `python3 todo.py done 3` - marks todo #3 as complete
- `python3 todo.py delete 3` - deletes todo #3
- `python3 todo.py list --pending` - shows only incomplete todos

Output format for `list`:
```
[1] [ ] Buy groceries (2024-01-15)
[2] [x] Walk the dog (2024-01-14)
```

**Verify:** Run the full add/list/done/list cycle from the command line and verify output matches expected format.

### t-3: Write unit tests
PENDING

**Spec:** Create `test_todo.py` using `unittest`:

1. Test adding a todo returns correct fields
2. Test listing returns added todos in order
3. Test completing a valid ID returns True and marks it done
4. Test completing an invalid ID returns False
5. Test deleting a valid ID removes it from the list
6. Test deleting an invalid ID returns False
7. Test persistence: add a todo, create a new TodoStore with the same file, verify the todo is there
8. Use `tempfile.mkdtemp()` for test storage (no hardcoded paths)

**Verify:** `python3 -m unittest test_todo -v` shows all tests passing.

### t-4: Add search and filter functionality
PENDING

**Spec:** Add search and filter capabilities:

1. `TodoStore.search(query) -> list[Todo]` - case-insensitive substring search on todo text
2. CLI command: `python3 todo.py search "groceries"` - shows matching todos
3. `TodoStore.list_all(pending_only=False) -> list[Todo]` - filter parameter
4. Update tests with search test cases

**Verify:** Add 3 todos, search for a substring that matches 1, verify only 1 result returned. Tests pass.

**Self-evolution:** If users need regex search or tag-based filtering, add a task.

### t-5: Add export and summary features
PENDING

**Spec:** Add summary stats and export:

1. `TodoStore.summary() -> dict` - returns `{"total": N, "done": N, "pending": N}`
2. CLI command: `python3 todo.py summary` - prints stats in a readable format
3. CLI command: `python3 todo.py export` - prints all todos as JSON to stdout (for piping)
4. Update tests for summary and export

**Verify:** After adding 3 todos and completing 1, `summary` shows `total: 3, done: 1, pending: 2`. `export` outputs valid JSON. Tests pass.
