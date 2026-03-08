# Changelog

All notable changes to BOI will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-03-07

### Added
- Core spec-driven execution engine with fresh-context-per-iteration design
- Four execution modes: Execute, Challenge, Discover, Generate
- Priority queue with DAG-based task blocking
- Parallel workers using git worktrees for isolation
- Self-evolving specs: workers add tasks at runtime as they discover new work
- 18-signal quality scoring across Code, Test, Documentation, and Architecture
- Critic system with configurable checks and custom check support
- Experiment proposals with adopt/reject/defer workflow
- Generate mode with goal-only specs, decomposition, and convergence detection
- Live spec management (add, skip, reorder, block tasks)
- Project model with shared context injection
- Natural language interface via `boi do`
- Per-iteration telemetry with Deutschian progress metrics
- Integration hooks (on-complete, on-fail) with JSON event log
- Universal install script for macOS and Linux
- Comprehensive test suite (unit, integration, eval)
