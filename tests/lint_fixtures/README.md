# Lint fixtures

Regression tests for the workspace lint configuration in `Cargo.toml`.

Each fixture file in this directory:
- Contains a deliberate violation of a specific lint.
- Includes a `#![deny(...)]` opt-in at file scope to make the violation hard-fail.
- Documents the expected diagnostic in a top-of-file comment.

These fixtures verify lint *behaviour*, not just configuration presence. If
a lint silently stops firing (e.g., due to a rustc version bump or a config
typo), CI fails on the fixture.

Run locally with: `cargo build --tests --locked` and observe lint output.
