# UI review

Run the RBS UI review on the current change.

1. Read `.claude/skills/ui-review/SKILL.md` and go through every item of its checklist for the files this change touches. Answer each item OK, Issue or N/A with one line.
2. Run `cargo xtask check-ui`. If it fails because a count is lower, run `cargo xtask check-ui --lower`. If it fails because a count is higher, stop and ask the owner in writing (ADR-0115); do not edit `docs/ui/baseline.toml` or `docs/ui/exceptions.toml` by hand.
3. Run `cargo test -p rbs-app --lib look` for the token and contrast tests.
4. If the change is visible to a user, list the screenshots the owner needs (dark and light; high contrast when a surface or border token, an outline or a high-contrast branch changed) and do not merge before the owner approves.
