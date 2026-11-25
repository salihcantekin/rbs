# ADR-0187: Version group Scroll settings for run identity

- Status: Accepted
- Date: 2026-11-25
- Deciders: Salih (owner)
- Supersedes in part: ADR-0185 decision 3 (group run key and same-key sharing)
- Builds on: [ADR-0185](0185-scroll-a-group-as-one-picture.md)

## Context

ADR-0185 requires both a fresh group Scroll run when settings change and phase
sharing for the same `(canvas, originating scene, group, settings)` key. If a
held Program retains settings A while Preview changes A to B and back to A,
those requirements conflict. The owner chose to restart Preview in that case.

## Decision

1. **Version each effective change.** A session-local settings generation
   identifies each effective saved Scroll settings value for a group and
   canvas. Every change, including clear, Undo and Redo, issues a new
   generation; setting the current value again does not. A newly loaded group
   starts with a fresh generation. Generations are not saved; the studio host
   owns a monotonic allocator for its lifetime, including studio restarts and
   collection switches. It never reuses a generation. Switching collections
   retires old collection state; switching back allocates fresh generations.
   Scene duplication or copying creates fresh generations for its new groups;
   deleting and undoing a group or scene, and ungrouping and undoing it,
   similarly allocate fresh generations for restored groups. Existing held
   or outgoing snapshots retain their captured generations until retired.
2. **Capture and share.** A snapshot captures the generation with its saved
   settings. The run key is `(canvas, originating scene, group, settings
   generation)`; snapshots of the same key share phase. Held Program and
   outgoing snapshots keep their captured generation and settings. Program,
   Preview, incoming, outgoing and live hold-scene occurrences each capture
   the generation and settings from the scene snapshot they render; they never
   resolve an old snapshot's generation from the subsequently edited scene.
   Freeze-frame hold and replay use their captured output, not a new group
   snapshot. A Preview
   edit gets a new generation even if the new value equals an older held
   value, so it starts a fresh run and cannot alter the held phase. This
   replaces only ADR-0185 decision 3's settings-based key and sharing rule.
3. **Run lifecycle is separate.** A settings generation is not the public
   run id. A run starts when needed and ends when unused as in ADR-0185; a
   later appearance or live restart issues a new run id without changing the
   saved settings generation. Membership and bounds edits keep the generation
   and phase. A studio restart preserves generations and rebases live runs as
   in ADR-0185; while the API server remains running, each continuing run
   retains its public run id. The studio host owns the API-server-lifetime
   run-id allocator and associates each id with the run incarnation. Studio
   handover transfers the snapshot identities, keys, generations, run state,
   incarnations and current public ids together. A newly started API server
   instance creates a new id lifetime; reattaching or restarting only a
   transport of the same instance does not. Before the new instance serves
   queries or publishes events, the control thread assigns fresh ids to its
   still-active runs and establishes the new lifetime's state. An id is an
   opaque unsigned 64-bit run-incarnation identifier, not the settings
   generation. Exhaustion is reported as an error; ids must never wrap or be
   reused in one lifetime.
   Switching collections retires old runs and their ids only when the switch
   commits successfully, without resetting the API-server-lifetime allocator.
   A failed switch restores the old mappings, runs and ids unchanged.
4. **No partial edit.** Issue and publish the new generation as part of the
   same successful control-thread edit as the saved settings; a refused edit
   changes neither generation nor run. The API's public run id and event
   identify live runs, not internal settings generations.

## Consequences

- Matching held and Preview snapshots continue to share phase until settings
  change; returning to an old value does not rejoin an old held run.
- This changes the internal key only: document format 31 and the planned public
  API have no settings-generation field. Public run ids remain scoped to the
  API-server lifetime; saved settings and undo records do not persist a
  generation.
- Update the planned compositor, scene and control API contracts before
  implementation and submit them to the independent review required by
  ADR-0185. Test A→B→A with held Program, Undo and Redo, a no-op settings
  write, live restart, studio and API-server restart, successful and failed
  collection switches, duplication, deletion/undo, removal and later
  reappearance.

## Alternatives considered

- **Rejoin the matching old phase.** Rejected by the owner: a settings change
  must restart Preview, even when its value matches the held Program.

## Review

Independent AI review, 2026-11-25: changes requested. The reviewer asked for
public run-id continuity across studio restart; decision 3 now specifies it.
A second independent review requested allocator ownership, handover,
collection and restored-group lifetimes, and compatibility details; decisions
1 and 3 and the consequences now specify these. A further review requested
snapshot coverage, API server boundaries and failed-switch rollback; decisions
2 and 3 now specify them. Independent AI review, 2026-11-25: approve, with
the dependent spec updates and independent contract review required before
implementation. Owner approval: Salih, 2026-11-25.
