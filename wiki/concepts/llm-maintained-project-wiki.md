# LLM-Maintained Project Wiki

## Purpose

The `wiki/` directory is not just documentation for humans — it is **structured, queryable, version-controlled context** for LLMs and agents working on the project.

It enables:
- Consistent long-term memory across sessions and different agents (coding, Hermes reflection, research).
- Self-improvement loops: Hermes can read the wiki, analyze outcomes, and propose concrete updates.
- Reduced hallucination on project-specific details (APIs, schemas, past decisions, trading rules).
- Onboarding speed for new human or agent contributors.

## Principles

1. **One source of truth** for concepts, decisions, experiments, and operational knowledge.
2. **Markdown first, LLM-friendly**:
   - Clear headings, short paragraphs.
   - Bullet lists and tables preferred for facts.
   - Explicit "as of" dates and version notes.
   - Links between pages (relative).
3. **Living documents**: Never let pages go stale. Hermes + human edits keep them current.
4. **Distinguish facts vs. opinions** vs. hypotheses.
5. **Sources cited**: External claims link back to [sources/](../sources/).
6. **Decisions are first-class**: Rationale, alternatives considered, date, and (later) outcome review are recorded in `decisions/`.

## How Hermes Uses the Wiki

- **Input**: Periodically (or on triggers like market resolution) reads relevant slices (e.g. last 30 reflections + open experiments + strategy concepts + recent journal).
- **Processing**: Identifies gaps, contradictions, outdated assumptions, successful patterns worth amplifying.
- **Output**: Structured proposals (diffs or full rewritten sections) + justification. In dev mode can auto-apply many; high-impact changes require human PR/review.
- **Feedback loop**: After real-world outcome (trade P&L, resolution accuracy), writes "post-mortem" entries that reference the wiki state at decision time.

## Wiki Update Workflow (Ideal)

1. Change is contemplated or experiment concludes.
2. Relevant wiki page(s) are updated **as part of the change** (or immediately after).
3. Hermes is invoked (manually or scheduled) to synthesize broader implications.
4. Log entry in `log.md` + cross-links created.
5. (Future) Automated checks for broken internal links or schema drift.

## Directory Conventions

- `concepts/` — Timeless or slowly changing mental models and architecture.
- `decisions/` — Timestamped, with `README.md` index. Use YYYY-MM-DD-slug.md or simple sequential.
- `experiments/` — Hypothesis → method → results → lessons. Link to journal entries.
- `sources/` — Summaries + deep links + version pins of external systems (Polymarket API, SDKs, k8s operators, crates, etc.). Update when versions change.
- `runbooks/` — Actionable checklists. Tested regularly.
- `schema.md` — Single source; kept in sync with actual migrations.

## Anti-Patterns to Avoid

- Burying key facts in long PR descriptions or Slack.
- "We'll document later" (later never comes; Hermes will call it out).
- Inconsistent terminology.
- Stale "as of" dates without refresh.
- Duplication between wiki/ and docs/ or code comments (prefer wiki for high-level, comments for "why this line").

## Metrics of Health

- Hermes can answer "What was our rationale for X in May 2026?" by pointing at a decision file.
- A new agent session can implement a feature touching trading + UI + DB with <5 clarifying questions because the wiki + schema + AGENTS.md cover it.
- Number of "surprises" during post-resolution reviews trends down over time.

This page itself is maintained by the same process.
