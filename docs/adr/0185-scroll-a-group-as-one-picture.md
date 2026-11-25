# ADR-0185: Scroll a group as one picture

- Status: Accepted (decision 1 superseded in part by ADR-0188; decision 3 superseded in part by ADR-0187; decision 5 superseded in part by ADR-0193)
- Date: 2026-11-25
- Deciders: Salih (owner)
- Supersedes: ADR-0184 decisions 3, 4 and 5 in part
- Builds on: [ADR-0182](0182-source-groups.md), [ADR-0150](0150-scrolling-layers-in-the-compositor.md)

## Context

The owner asked for Scroll on a selected group to move the group as one picture,
not to apply a separate item Scroll to each member. The owner also chose not to
offer crop, shape or border in group Properties.

ADR-0182 models a group as a tag on independent layers. Flattening draws those
layers separately and does not produce a group picture; ADR-0150 scrolls an
individual item's picture through its own window. Applying it to each member is
therefore not the requested operation. ADR-0184's group Properties decision
assumed member-level Scroll and needs to change.

## Decision

1. **One picture.** Group Scroll must compose the group's drawn picture members
   bottom first and scroll that picture as one unit. A member is drawn when
   `visible || cued`, subject to ADR-0105's shown-scene and ancestor rules; cue
   start and expiry change the picture, not the group run. Nested scenes must be
   supported, including their transforms, visibility and cues. A scrolling group
   in a nested scene is composed and scrolled before the containing scene is
   composed into an outer group; recursion is bounded by the existing scene
   nesting limit. A member's own Scroll continues inside the composed picture
   with its existing shared-run behaviour; group controls affect only the
   separate group run. Audio has no Scroll picture: if any direct member has no
   picture, the operation must be unavailable with a reason, not silently omit it.
2. **Window and coordinates.** The window is the axis-aligned union of all
   direct member picture boxes on the edited canvas, including hidden members,
   after their rotation, flip, nested transforms and layout-space scaling. This
   keeps cue and visibility changes from changing the window. Translate that
   canvas rectangle to the intermediate texture origin; draw the members with
   their existing transforms, crop, style, opacity and source-over order onto
   transparent black using the compositor's premultiplied linear-alpha rules.
   Transparent gaps stay transparent. At zero scroll the result within the
   window matches the members drawn without group Scroll. The completed picture
   fills that window without an additional aspect fit; ADR-0150's wrap, gap,
   repeat, start and fade apply to it. Anything outside the window is clipped.
3. **Storage and run.** A group owns optional Scroll settings separately for
   each canvas, with no inheritance to another canvas. Use the same parameter
   ranges and units as ADR-0150 in that canvas's layout space. A group run is
   keyed by `(canvas, originating scene, group, Scroll settings)`, independent
   of member runs and other groups. ADR-0063's held copies use their original
   scene and group identity, not their temporary run-scene id. Copies with the
   same key share phase across program, preview and transitions, but each renders
   its own member snapshot. If preview changes the saved Scroll settings while
   program is held, it gets a separate run and the held settings and run stay
   on air; the preview edit restarts its run. The run
   starts when a shown picture first needs it, ends when no output or preview
   uses it, and restarts on a later appearance. A studio restart rebases the
   run to the new engine clock as in ADR-0150. A membership or bounds change
   keeps the current phase and wraps it to the new period; a Scroll settings
   edit restarts it. Removing a group drops its run once no held or other
   snapshot still uses it. A saved edit is one undo
   step; pause, resume, restart and speed changes are live, not undoable.
4. **API and compatibility.** `group.apply` from ADR-0184 must provide typed
   operations to set or clear the selected canvas's group Scroll and to pause,
   resume, restart or set the live speed of its run. Live speed uses ADR-0150's
   range and ramp, lasts only for the run, and is not saved or undoable.
   Membership preflight and all-or-none errors still
   apply. A live group control must not change any member run, so a member run
   shared with an outside placement does not block the group control; this
   supersedes ADR-0184 decisions 4 and 5 only for group Scroll. The control API
   minor version must advance to the next free version; an older server returns
   `method_not_found`, and clients must check the version before using it. The
   next free scene-document format stores each group's per-canvas Scroll; it is
   written only when used, and an older reader refuses that newer document
   without overwriting it. The UI shows group Scroll rather than a bulk edit of
   member Scroll; group crop, shape and border are not offered.
5. **GPU and lifecycle.** Compose into an `Rgba16Float` GPU texture and sample it
   as one layer at the group's position in the layer stack; no CPU readback.
   Allocate and reserve resources on the control side, never in `Node::process`;
   reuse them while the group is actually rendered. No enabled group Scroll in
   any rendered scene means no group-composition node, texture or per-frame
   work. Limit dimensions to the device's maximum texture dimension and
   `MAX_CANVAS_SIDE`; reject invalid or oversized bounds with a reason, never
   silently scale or drop a member. On a recoverable group-local allocation or
   render failure, count and report the failure, draw that group transparent,
   and continue rendering unrelated layers; never fall back to independently
   drawn members. Device loss follows the existing graph-level recovery.
   Cue, membership, content and geometry changes invalidate the intermediate
   picture; an unchanged paused group permits render-on-change. Release unused
   resources after all program, preview, transition, hold and replay users finish.
6. **Contract gate.** Before implementation, the scene model, document format,
   control API, compositor and UI specs must define the exact wire shapes and
   errors, nested render plan, phase arithmetic, size rounding and allocation
   limits, the held-copy identity mapping and frozen versus live snapshots for
   program, preview, transitions, hold scenes and replay, once-finished events,
   failure reporting and testable render-on-change rules. A fresh independent
   review must cover those contracts before their implementation.

## Consequences

- A scrolling group retains its composition instead of becoming independently
  scrolling layers. It needs a group-composition render pass while active.
- The compositor and graph change is a hot-path change: measure before and after,
  complete `hot-path-review`, and obtain a human maintainer's approval.
- Tests must cover overlapping and hidden members, cues, mixed source kinds,
  recursive nested groups, held program and preview with shared phase and separate
  snapshots, diverging held and preview Scroll settings, canvas isolation,
  membership and bounds changes, undo, live controls, unsupported members and
  resource release. UI review and owner visual approval are required before merge.
- Measurements must compare group Scroll absent, saved but not shown, and active
  for one and several groups, including CPU, GPU and memory on the named machine.

## Alternatives considered

- **Apply item Scroll to every member.** Rejected by the owner: the group must
  scroll as one picture.
- **Keep Scroll on individual members only.** Rejected because it does not
  provide a Scroll operation for the selected group.
- **A dedicated graph node per active group.** A conditional compositor subpass
  is the proposed route; a separate node adds graph scheduling and intermediate
  edges. The render-plan contract and review must determine whether the subpass
  can meet nested scenes, resource lifetime and real-time requirements.

## Review

Independent AI review, 2026-11-25: changes requested. The reviewer found that the
proposal must resolve conflicts with ADR-0184 decisions 4 and 5, define the
composition geometry and alpha, canvas scope and run lifecycle, document and API
compatibility, cue visibility, and GPU resource and failure handling. The
proposed Scroll window and the contracts in decision 5 need revision and another
independent review before owner approval and implementation.

The owner selected the group bounds as the Scroll window and support for nested
scenes on 2026-11-25. An independent follow-up review found the first five
concerns resolved or substantially resolved, and requested live speed, recursive
nesting and snapshot coverage, and a device-loss correction. Decisions 1, 4 and 5
and the tests now address these; exact transition, hold and replay snapshot
contracts remain a mandatory spec and independent-review gate before implementation.

An independent follow-up review found the remaining decision-level issue: a
held program copy's temporary scene id would not share its original's run.
Decision 3 now keys compatible runs by the originating scene, and isolates held
program from a preview Scroll-settings edit. The snapshot contract and its
independent review remain mandatory before implementation.

Independent AI follow-up review, 2026-11-25: approved, no blocking findings.
The reviewer confirmed the held-copy identity and settings isolation; transition,
hold and replay snapshot details still require independent contract review before
implementation.

Owner approval: Salih, 2026-11-25, after the independent review.
