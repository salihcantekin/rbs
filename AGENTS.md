# AGENTS.md

Instructions for AI coding agents (Cursor, Codex and others) working in the RBS repository.

**Read [CLAUDE.md](CLAUDE.md) fully before any change.** It holds the binding rules for every agent, whatever its name; this file does not repeat them. The procedures in `.claude/skills/*/SKILL.md` are plain Markdown and apply to every agent too.

## UI stop rule (ADR-0115)

An AI agent whose change would break a rule of `docs/ui/principles.md`, the design system spec (`docs/specs/ui-design-system.md`) or an accepted UI ADR must stop and ask the owner in writing before making the change. It must not add an exception, raise a count or mark a literal to get past a check.

- `cargo xtask check-ui` counts three things in `crates/rbs-app/ui` (raw `TouchArea` outside the library, component names declared in more than one file, `help:` texts over 90 characters). Each count must equal `docs/ui/baseline.toml` plus `docs/ui/exceptions.toml`. It runs in `cargo test --workspace`.
- Never edit those two files by hand. When your change lowers a count, run `cargo xtask check-ui --lower`. When a count would rise, stop and ask the owner; only after written approval run `cargo xtask check-ui --except <check> <count> <file> <reason> <owner's approval> <date>`.
- The checks see only what they count. Every other rule (tokens, library components, row anatomy, `Toggle` for a setting, `DialogActions` for window buttons, glossary terms) is checked with the UI review checklist.
- Screenshots for the owner's approval of a visual change are dark and light; add high contrast when the change touches a surface or border token, an outline, or a branch on the high-contrast theme.

Before finishing any change to UI code, tokens, components or UI text, go through the checklist in [.claude/skills/ui-review/SKILL.md](.claude/skills/ui-review/SKILL.md). Rules: [docs/ui/principles.md](docs/ui/principles.md). Design system: [docs/specs/ui-design-system.md](docs/specs/ui-design-system.md). Decision: [ADR-0115](docs/adr/0115-ui-stop-rule-for-agents.md).
