# Contributing to BOI

Thanks for your interest in contributing to BOI.

## Getting Started

1. Fork the repository
2. Clone your fork: `git clone https://github.com/YOUR_USERNAME/boi.git`
3. Create a branch: `git checkout -b my-feature`
4. Make your changes
5. Run tests: `cd ~/boi && python3 -m unittest discover -s tests -p 'test_*.py'`
6. Commit: `git commit -m "Add my feature"`
7. Push: `git push origin my-feature`
8. Open a Pull Request

## Development Setup

Requirements:
- Python 3.10+
- bash
- git
- tmux

No pip dependencies. BOI uses Python stdlib only.

## Running Tests

```bash
# All unit tests
python3 -m unittest discover -s tests -p 'test_*.py'

# Specific test file
python3 -m unittest tests.test_queue

# Integration tests
python3 -m unittest tests.integration_boi

# Eval tests
python3 -m unittest tests.eval_boi
```

All tests must pass before submitting a PR. Tests use mock data only, no live API calls.

## Code Style

- Python: follow PEP 8
- Shell: use `set -uo pipefail` (no `-e`), quote variables, use `[[ ]]` for conditionals
- No external dependencies. stdlib only for Python, coreutils only for shell.

## What to Contribute

- Bug fixes with test cases
- New critic checks (add to `templates/checks/`)
- Documentation improvements
- Platform compatibility fixes (macOS/Linux edge cases)
- Performance improvements to queue or spec parsing

## Pull Request Guidelines

- One logical change per PR
- Include tests for new functionality
- Update documentation if behavior changes
- Keep PRs small and focused

## Reporting Issues

Open a GitHub issue with:
- What you expected to happen
- What actually happened
- Steps to reproduce
- Your OS and Python version

## License

By contributing, you agree that your contributions will be licensed under the MIT License.
