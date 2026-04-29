# Abandoned Projects Analysis — The Graveyard Tells the Truth

_Generated: 2026-04-29 (T6854 — Mirofish Business Opportunity spec)_

---

## What Was Analyzed

All project containers in `~/.boi/projects/` (49 entries) plus the git archive at `~/boi/_archive/python/` and broader project context from career analysis, assessment files, and spec history. The `q-NNN` numbered entries are individual spec research notes, not standalone projects — they're excluded from the abandonment analysis but inform the pattern section.

---

## The Map: What Mike Built and Where It Stopped

### TIER 1: Core Systems (Built Deep, Still Active)

**hex** — Persistent AI agent. Has shipped at least 6 major subsystems:
- hex-core: agent loop, session management
- hex-ui: web UI for agent control
- hex-events: event-driven policy engine (159+ tests, Docker-verified, production)
- hexagon-base: shared utilities
- ai-native-env: AI development environment
- hyperagents: self-improvement pipeline (reflect → eval → archive → score → dispatch, proven E2E)

How far did he get? **All the way**. hex is the project that actually compounded. 300+ BOI iterations documented against it. Real production load. Real memory benchmarking (7 configs, hybrid FTS5+sqlite-vec+RRF winner). Real eval frameworks.

**BOI** (Beginning of Infinity) — Agent orchestrator.
- v1: Python, 20K+ lines, 80+ test files, 1,536 test cases. Archived at `~/boi/_archive/python/`. Got to production with a full dashboard, daemon, worker fleet, critic system, eval system, and 300+ real specs executed.
- v2: Rust rewrite, April 2026. Complete rewrite of v1 in Rust — same architecture, typed correctness, ~10x speed gain. Currently in production.

How far? **Python version was complete enough to replace entirely**. Rust version is the current production system.

---

### TIER 2: Completed Research, Unclear Execution

**Polymarket Scanner** — A Python prediction market scanner.
- Built: src/scanner.py, signals.py, paper_trader.py, classifier.py, db.py, scripts/scan.sh
- How far: Real code, real API integration, real bug discovered (SCAN_MAX_PAGES=0 = unlimited requests = timeout every run)
- Why stopped: Bug was identified in depth (q-348 research). Whether it was actually fixed is unknown — the research is complete but no follow-up spec exists.
- Tells the truth: Mike will build the trading infrastructure but not necessarily make it work reliably.

**Zwerk** — AI-powered spreadsheet/board tool (pydantic-ai backend, Svelte frontend).
- How far: OAuth scope research done (q-123), Progressive disclosure strategy designed, Google verification strategy planned.
- Why stopped: No evidence of continued development in boi history. Research was done but no execution specs followed.
- Tells the truth: Mike starts product research for external-facing tools but doesn't follow through to users.

**Whitney Content Lab** — Photo/media curation system for his girlfriend.
- Built: Firebase setup (custom storage bucket, Firestore rules), media ingestion pipeline with AI classification, admin review queue, bulk moderation API.
- How far: Firebase infrastructure real and running (q-491, q-508). Admin endpoints built.
- Status: Active as of April 2026 — most recent active project outside hex/boi ecosystem.
- Tells the truth: Mike ships when there's a concrete human who needs it and it's small enough to finish.

**Hermes customizations** — Third-party AI assistant (NousResearch hermes-agent) being customized for Mike's use.
- Built: Voice restoration via system prompt injection (300-token voice trait injection to config.yaml), update strategy analysis, memory analysis.
- How far: Research complete, config patches applied.
- Tells the truth: Mike uses Hermes as a secondary agent system and tinkers with it rather than building on top of it.

---

### TIER 3: The Graveyard — Created and Never Started

These are boi project containers created with intent but zero specs ever dispatched:

| Project | Created | Description | What It Probably Was |
|---------|---------|-------------|---------------------|
| anti-pattern-enforcement | Unknown | Empty | Auto-enforcement of code quality anti-patterns in BOI specs |
| boi-optimizer | Unknown | Empty | Optimizing boi's performance/throughput |
| diversity-collapse | Apr 24, 2026 | Placeholder only | Research into AI thought homogenization or opinion collapse |
| hex-identity-tournament | Apr 24, 2026 | Placeholder only | Tournament to select hex's best persona/identity configuration |
| hex-memory | Apr 23, 2026 | Placeholder only | Deep memory architecture work for hex |
| hex-pm | Apr 8, 2026 | Placeholder only | Project management features for hex |
| hermes-memory-analysis | Apr 4, 2026 | Empty | Analyzing Hermes's memory system vs hex's approach |
| hex-events (project) | Mar 16, 2026 | Empty | Analysis/improvement work on hex-events |
| local-llm-server | Mar 19, 2026 | Empty | Setting up a local inference server |
| timeout-resilience | Apr 7, 2026 | Empty | Making hex resilient to provider timeouts |
| tirith-yolo-mode | Apr 4, 2026 | Empty | "YOLO mode" for hex's Tirith security system (bypass all checks) |
| tmux-makeover | Mar 18, 2026 | Empty | Redesigning hex's tmux layout and UX |

---

## Why Did He Stop? Pattern Analysis

### Pattern 1: The Cluster Creation Problem
Projects die in clusters. hex-identity-tournament and diversity-collapse were both created April 24, 2026 — same day. hex-memory was April 23. hex-pm was April 8. tirith-yolo-mode and hermes-memory-analysis both April 4. The pattern: Mike gets excited about multiple ideas simultaneously, creates project containers for all of them, then dispatches specs for only one or two. The rest sit empty.

**What this means**: Mike's mind runs parallel. He generates ideas faster than he executes them. The boi project creation act is his way of "parking" an idea — it feels like progress without being progress.

### Pattern 2: Solved Differently
Several abandoned projects had their core need addressed through other channels:

- **tmux-makeover** → The q-329 TUI/context-switching research produced a clear recommendation (fzf + gum), but the implementation was never dispatched as a separate project.
- **timeout-resilience** → The gateway-timeout-analysis project diagnosed the root cause (ReadTimeout during hex's restaurant search task), completing the "why" without the "fix."
- **boi-optimizer** → The entire Rust rewrite addressed performance more fundamentally than any optimization project could have.
- **hex-memory** → Memory provider research (memory-providers project) chose holographic, completing the research phase. The implementation may have been done directly in the hex repo.

**What this means**: Mike has a talent for finding the right level of solution. He'll abandon incremental fixes when a more fundamental solution appears. This is the "systems over features" principle in action — but it means many projects exist as permanent research stubs.

### Pattern 3: The Infrastructure Trap
Half the graveyard is meta-infrastructure: tools to improve the tools. anti-pattern-enforcement (for better spec writing), boi-optimizer (for faster spec execution), hex-memory (for better hex), hex-pm (for managing hex projects), hex-events project (for improving the event system). These never get specs dispatched because they're always slightly less urgent than the actual work.

**What this means**: Mike understands deeply that infrastructure compounds, but he has a blind spot: you can improve infrastructure forever without ever shipping what the infrastructure is for. The infrastructure graveyard is a specific failure mode of his operating style.

### Pattern 4: The Idea Jar Without Forcing Function
tirith-yolo-mode and diversity-collapse are the clearest examples of "interesting idea, no urgency." Tirith YOLO mode would let hex bypass its own security checks — a useful debugging tool but not a crisis. Diversity-collapse research might have been about AI homogenization of thought (a timely topic in April 2026) but had no concrete application.

**What this means**: Mike creates project containers when he reads something interesting. Without a concrete problem it solves, the project stalls immediately.

### Pattern 5: The Stage of Death is Always the Same
Critically: **Mike almost never gets halfway**. Projects are either:
- Never dispatched (the graveyard above — project created, no specs), or
- Completed through at least a full research cycle (ai-trends: 6 branches, all produced; career-analysis: 3 tasks, all complete; memory-providers: holographic selected)

He doesn't start a sprint and abandon it halfway through. The commit is binary. This is unusual — most people abandon at the "nearly done" stage. Mike abandons before he starts.

---

## What Patterns Emerge?

### The Consistent Completion Pattern

When Mike DOES execute, he goes deep. The ai-trends project produced tens of thousands of words of research across 6 branches. Career analysis produced a full five-path model, Anthropic-specific positioning analysis, and negotiation tactics. The polymarket-scanner research diagnosed a bug at the exact code line. He doesn't do surface-level work.

**The corollary**: The empty projects are empty because Mike knows what deep work looks like and has implicitly decided the project doesn't warrant it yet. "Yet" often becomes "never."

### The Personalization Driver

Projects with a real person attached ship. Whitney Content Lab: ships because Whitney needs it. hex: ships because Mike uses it daily. hex-ui: ships because Mike needs to interact with hex visually. Polymarket Scanner: built because Mike wants to trade.

Projects with only a theoretical beneficiary don't ship. local-llm-server: who's using it? tmux-makeover: Mike already uses tmux fine. diversity-collapse: intellectually interesting but serves no one specific.

### The Stated vs Actual Interests Gap

**Stated interests**: AI trends research, agent-to-agent communication, autonomous experimentation, local inference revolution, self-improvement systems.

**Actual completed work**: Infrastructure for his own agent (hex), tooling for his own workflow (BOI), projects serving a concrete relationship (Whitney Content Lab), and career positioning analysis.

The divergence is sharp: Mike talks a lot about the AI ecosystem (and produces excellent research about it) but builds primarily for himself and one other person (Whitney). His actual interest radius is much tighter than his intellectual radius.

---

## Which Abandoned Projects Should Be Revisited?

### High Value — Revisit Now

**hex-identity-tournament** (Apr 24, 2026)
- The concept: run multiple variants of hex's identity/persona configuration through a structured tournament to select the best configuration.
- Why worth revisiting: hex's identity is currently implicit — "Hermes voice injection" research shows Mike is already thinking about this. A tournament-based selection process would make it rigorous. Hex compounds over time; a better identity configuration compounds too.
- Why stopped: Too abstract to dispatch without a clearer success metric. Needs a definition of "better hex identity" before a tournament can be run.
- First step: Define 3-5 concrete identity dimensions to test (communication style, proactivity level, memory recall behavior) before creating specs.

**hex-memory** (Apr 23, 2026)
- The context: memory-providers research selected holographic as the only provider with trust scoring and contradiction detection. But holographic is an external dependency.
- Why worth revisiting: The implementation of holographic into hex's architecture was never dispatched. The research stopped at "holographic wins" without "here's how to integrate it."
- First step: Dispatch a spec to audit current hex memory vs holographic's trust scoring model, produce an integration plan.

**local-llm-server** (Mar 19, 2026)
- The context: Created when BitNet (100B params on CPU, 82% less energy) was announced. LLM cold-start optimization research (April 2026) showed that the 6-200s cold start is Node.js initialization, not inference — and that local models can't run agentic tools.
- Why worth revisiting differently: Not as agentic tool runners, but as judgment-phase models. The cold-start research already identified that OpenRouter models (Grok 4.1 Fast, Gemini 2.5 Flash) can be used for judgment-only BOI phases. A local inference server could reduce per-call costs for these judgment phases further.
- First step: Dispatch the phase-to-model mapping task (t-3 from llm-cold-start-optimization, which identified this) with a local inference constraint added.

**diversity-collapse** (Apr 24, 2026)
- The concept: Unknown from project files, but the name points to research on AI systems producing homogeneous outputs over time — a real phenomenon worth understanding if hex is to remain useful.
- Why worth revisiting: Hex's self-improvement loop could theoretically converge on a single stable configuration and stop improving. Understanding diversity-collapse dynamics in self-improving systems is directly relevant to hex's long-term architecture.
- First step: Add a context.md explaining what the project is actually for — the name alone isn't enough to dispatch specs.

### Lower Priority — Ideas That Aged Out

**anti-pattern-enforcement**: Automated detection of spec anti-patterns in BOI. Useful, but BOI now has a Critic system that does this manually. The Rust rewrite includes a spec validator. The need is partially met.

**timeout-resilience**: Gateway timeouts were diagnosed (restaurant search task permanently lost). The root cause was ReadTimeout during a long hex operation with no resume path. The Rust BOI rewrite includes proper timeout handling. Partially solved.

**tirith-yolo-mode**: A debugging mode for bypassing hex's security layer. Useful for development but not urgent enough to dispatch ever.

**tmux-makeover**: The TUI research (q-329) selected fzf + gum as the right tools. The makeover never happened. Given that hex-ui exists as a web interface, a tmux redesign is declining in priority.

**hex-pm**: Project management within hex. The boi spec system already handles project management for boi's own work. This may have been about hex tracking its OWN projects — like an internal to-do system for hex itself. That's interesting but redundant given BOI's existing project tracking.

**hermes-memory-analysis**: Was probably about comparing Hermes's memory system to hex's. Given that the memory-providers project already identified holographic as the right architecture, this comparison may be moot.

---

## What the Graveyard Reveals About Mike's Actual Interests vs Stated Interests

### His Stated Interests
AI trends, agent-to-agent knowledge sharing, autonomous experimentation, self-improvement systems, the local inference revolution, proactive intelligence scanning, compound engineering as a discipline.

### His Actual Interests (Revealed by Completion Pattern)
1. **Making his own tools better** — hex, boi, memory. Every completed project in the ecosystem serves this. This is the real core.
2. **His own financial position** — career analysis (Anthropic positioning), polymarket scanner (trading), mirofish business opportunity (hosting service). He spends real research effort on wealth-building vectors.
3. **Concrete deliverables for Whitney** — The Whitney Content Lab is the only external-facing project that shipped. This reveals that love is a more reliable shipping driver than intellectual curiosity.
4. **Understanding the AI ecosystem around his own tools** — The ai-trends, ai-agent-frameworks, stealth-browser, memory-providers, and persistent-agent-systems research all ultimately serve the question: "what should I incorporate into hex/boi?"

### The Gap
Mike talks about building for compound leverage and frontier positioning, but his completed project map shows he's mostly building for himself (one person, one agent system) and one other person (Whitney). There's no external customer. There's no user base. There's no feedback loop except his own daily usage.

This isn't a criticism — hex is a sophisticated system. But the "compound engineering brand" goal (mentioned in career analysis as a quarterly target) doesn't show up in any completed project. The brand-building work (mrap.me, LinkedIn, YouTube, the compound engineering concept) appears repeatedly in stated goals but shows zero boi specs executed against it.

**The graveyard's deepest truth**: Mike builds exquisite infrastructure for work he hasn't started yet. The abandoned projects are infrastructure improvements for a builder who hasn't yet decided what he's building for others.

---

## The Abandonment Stage

Across all abandoned projects: death occurs at the project creation stage, never mid-sprint.

Possible interpretations:
1. **Perfectionism before starting**: Mike knows what deep work looks like. He won't dispatch specs for a project until the context.md is good. The empty context is a blocker, not laziness.
2. **Natural filter**: The act of writing a context.md forces Mike to articulate what success looks like. Projects that fail this test (because there's no clear success state) stay empty.
3. **The boi project container as "parking lot"**: Creating a project container scratches the itch of "I should do this" without committing to it. It's the agile equivalent of creating a ticket and putting it in the backlog.

The healthiest interpretation: Mike's project creation discipline is actually working correctly. He creates containers for everything he's tempted to do, but only dispatches specs for things with clear success criteria and concrete beneficiaries. The graveyard is the filter, not the failure.

---

## Summary

**What he builds**: Infrastructure that compounds over time for his own use (hex, BOI), then research that helps him position himself (career analysis, market opportunity research), then deliverables for Whitney.

**What he abandons**: Meta-infrastructure ideas without forcing functions, research questions without concrete beneficiaries, and experiments that get solved differently before he starts.

**The consistent pattern**: He starts late and goes deep, or doesn't start at all. Halfway doesn't exist in his project history.

**The most important finding**: The gap between Mike's intellectual interests (AI ecosystem, compound engineering, autonomous agents at scale) and his project completion record (tools for himself, tools for Whitney) suggests he hasn't found his external customer yet. Everything he's built is prep work. The graveyard is mostly infrastructure improvements for a product that hasn't launched yet.

**Projects worth revisiting**: hex-identity-tournament (concrete architecture experiment with hex as the beneficiary), hex-memory implementation (holographic integration is chosen, just not built), local-llm-server (reframed as judgment-phase cost reduction, not agentic runner).

---

_Analysis based on: 49 project entries in ~/.boi/projects/, archived Python boi at ~/boi/_archive/python/, career-analysis research (q-348), ai-trends research (q-014 series), assessment-2026-03-16.md, boi-current-state.md, and individual research.md files across all projects._
