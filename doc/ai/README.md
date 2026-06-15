# AI Onboarding Documentation

This directory is durable onboarding material for future AI coding agents and human contributors working in this repository.

It is not product documentation. Product docs live under `docs/src/`; generated mdBook output lives under `docs/book/` and should not be hand-edited.

Use this set to answer:

- What is this project?
- Where should a change go?
- Which rules are verified by code, CI, or manifests?
- Which commands are safe, required, expensive, or environment-dependent?
- What assumptions are still open?

Start with `00_READ_THIS_FIRST.md`. For implementation work, read the root `AGENTS.md` and the local `AGENTS.md` for the crate or directory you are changing.

## Documentation Set

- `00_READ_THIS_FIRST.md`: entry point and rules for future agents.
- `10_ARCHITECTURE.md`: high-level architecture and boundaries.
- `20_PROJECT_MAP.md`: factual repository map.
- `30_DESIGN_RULES.md`: evidence-backed design rules.
- `40_COMMON_PATTERNS.md`: repeated implementation patterns.
- `50_TESTING_AND_COMMANDS.md`: command guide.
- `60_PERFORMANCE_NOTES.md`: performance-sensitive areas.
- `70_GLOSSARY.md`: project terms.
- `80_OPEN_QUESTIONS.md`: centralized uncertainty.
- `90_LOCAL_AGENT_PLAN.md`: local `AGENTS.md` plan.
- `AI_CHANGELOG.md`: durable architecture/change memory.
