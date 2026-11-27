# ADR-0193: Simple group Show and group Scroll resources

- Status: Accepted
- Date: 2026-11-25
- Deciders: Salih (owner)
- Supersedes: ADR-0190, ADR-0191 and ADR-0192 in full
- Supersedes in part: ADR-0185 decision 5, its clause allocating on the control side and never in `Node::process`
- Extends: ADR-0062 decision 5, to a group span appearing or changing size
- Builds on: [ADR-0105](0105-timed-show.md), [ADR-0150](0150-scrolling-layers-in-the-compositor.md),
  [ADR-0062](0062-built-in-video-filters-and-properties-tabs.md), [ADR-0184](0184-group-properties-batch-to-members.md),
  [ADR-0185](0185-scroll-a-group-as-one-picture.md), [ADR-0188](0188-defer-new-nested-scenes.md)

## Context

ADR-0192 made group Show a staged graph transaction with hidden topology,
participant acknowledgments and a commit-versus-timeout race. ADR-0190 and
ADR-0191 added an executor mailbox, studio-wide pixel accounting, parked
service hooks and an incremental quiesce path so group Scroll textures could be
prepared and retired off the render thread. After a day of work none of these
reached a usable feature, and the partial code grew to thousands of lines. The
owner asked for the simple behaviour the rest of RBS already has.

Single-item Show (`item.show`, ADR-0105) starts a cue on the control thread and
lets the studio reconcile the graph; a later source failure is runtime health.
Every existing GPU node (compositor, filters, ADR-0062) acquires pool textures on
its own render executor when a size first appears and gives them back with
`forget` and `cool`, under ADR-0062 decision 5's real-time exception. ADR-0188
already limits group members to source placements, so no nested composition is
needed.

## Decision

1. **Group Show.** `group.apply` Show runs on the control thread in one step.
   Preflight compares the expected membership, checks scopes and checks that
   every hidden member has a valid show duration. Any refusal changes nothing.
   Otherwise the host starts, or restarts if one is running, the cue of every
   hidden member with one `now` in one control-state change, then the studio
   reconciles the graph as `item.show` does. Visible members are unchanged. Only
   the edited scene is cued: a held program copy keeps its captured cues and an
   outgoing transition keeps its frozen cues, as for `item.show` today. Audio and
   video switch at their next block or frame. A source that later fails to start
   or render is reported through runtime health; the other members keep running
   and no graph change is rolled back. There is no staged graph, acknowledgment,
   commit guard or timeout race. Show is live and not undoable.
2. **Layer list.** The compositor's layer list carries an optional list of
   group spans next to its layers: each span names its member layer range, its
   canvas window and its group Scroll record. A list without spans costs one
   emptiness check per tick, not one per layer. The node draws each span's
   members into the span's intermediate, then samples that as one scrolling layer
   at the span's position. Member and group Scroll records together count against
   `MAX_SCROLL_LAYERS`, written into the one `ScrollBinding` at distinct
   offsets; past the limit a layer draws nothing and is
   counted, as ADR-0150 decision 7 does. Records are assigned bottom first in
   draw order, a span's own record before its members' records; a span that
   gets no record has no intermediate pass and its members are not drawn, so no
   member pass is wasted. Each pass is its own `render_scrolled` call with its
   own record write and submit; `render_scrolled` gains a base record slot so a
   later pass does not overwrite an earlier pass's records before its submit.
   In the node's redraw-on-change check, a span whose Scroll is still moving
   (`moving_at` on the window frame) counts as moving, so the canvas pass
   redraws. A span's intermediate pass runs only when a member's key changed or
   the intermediate was re-acquired; otherwise the last intermediate is reused.
3. **Group Scroll resources.** This extends ADR-0062 decision 5's real-time
   exception to a group span appearing or changing size: the compositor node
   then acquires one `Rgba16Float` intermediate for that span from its own
   executor's pool, at most one acquire per span per such event, and reuses it
   while the size is unchanged. The sampling bind group comes from the pool;
   the wrap pipelines and `ScrollBinding` are built on the control thread first
   (ADR-0150 decision 7); per-pass buffers are preallocated in `prepare` from the
   node's existing capacity, plus one extra layer list of that capacity for
   building the canvas pass. A span pass draws only its members; the canvas
   pass draws the non-members plus one layer per span. The binder gives no span
   to a group with no drawn member, so neither pass exceeds the list length,
   which the node already caps at its capacity (`MAX_LAYERS`). `Compositor`
   gains a pass that draws into an intermediate of its own size with layer
   records in the span's window space; today's canvas-size check still applies
   to the canvas pass. Nothing else allocates in `process`. On a size
   change or when a span leaves the list the node gives the texture back with
   `forget_texture`. `CompositorNode` gains a `cool` that forgets its
   intermediates, so a scene leaving the schedule frees them. ADR-0185 decision 5
   is otherwise unchanged: no CPU readback, and no node, texture or per-frame
   work while no shown scene has an enabled group Scroll. This replaces its
   clause "allocate and reserve resources on the control side, never in
   `Node::process`".
4. **Limits.** `group.apply` Scroll set refuses, with a reason, a window over
   8,388,608 pixels or `MAX_CANVAS_SIDE`; ordinary item edits are not
   preflighted against it. At bind time a group whose window is over those
   limits, or over the device's `max_texture_dimension_2d` capability value
   passed to the binder, gets no span: its members are drawn transparent, the
   group is counted and reported in runtime health, and the rest of the bind is
   unaffected. The binder also caps each compositor at 4 spans and 16,777,216
   summed span pixels (128 MiB of `Rgba16Float`); a group past the cap takes the
   same oversized path, bottom-most spans keeping theirs. The compositors of one
   render executor share its pool: today up to four roles (program, preview,
   hold, incoming) on `MAX_CANVASES` = 2 canvases, so at most 8 compositors,
   32 pool slots and 1 GiB of group intermediates in the worst case. The owner
   accepts this bound without a studio-wide budget, reservation or retirement
   proof. Past it, `PoolFull` is handled as in decision 5.
5. **Failure.** A typed acquire error (`PoolFull` or another `GpuError`) or a
   failed group pass draws that group transparent, is counted and reported as
   runtime health, and does not affect other layers. An out-of-memory that wgpu
   does not return as a typed error falls under the existing device-loss
   recovery. Studio restart and collection switch use the existing shutdown and
   release paths.

## Consequences

- The disconnected ADR-0190/0191/0192 code is removed. Rewrite in the same
  change: the gpu-compositor "Planned scrolling groups" section (drop the
  control-side reservation and the 33,554,432-pixel studio budget, add spans and
  executor allocation), its ADR-0190/0191/0192 sections, the engine and
  media-graph ADR-0190/0191/0192 sections, the control-api 1.108 Show text,
  performance-budgets group limits and recording's ADR-0192 notes. ADR-0185
  decision 6's independent contract review still applies to the rewritten
  specs before the render path is connected. The binder's group Scroll gate is
  removed only together with the connected render path.
- The compositor change is a hot-path change: before/after measurements,
  `hot-path-review` and a human maintainer's approval remain required, as does
  owner visual approval of the Properties UI.
- Show can briefly display some members before a slow source produces its first
  frame; this matches single-item Show.
- Tests: preflight refusal changes nothing; success starts or restarts every
  hidden member at the same instant and does not cue the held program; a list
  without spans takes the existing path; group Scroll allocates no texture while
  unused, reuses one while shown, frees it on removal, size change and `cool`;
  a group Scroll over unchanged members keeps moving under `OnChange` without
  redrawing its intermediate; `group.apply` refuses oversized bounds; an oversized bound group and a failed
  acquire draw transparent without affecting other layers.

## Alternatives considered

- **Keep ADR-0192's transactional Show.** Rejected by the owner: its complexity
  did not produce a working feature, and single-item Show has the simpler
  behaviour users already accept.
- **Keep ADR-0190/0191 executor preparation and quiesce.** Rejected by the owner:
  every other GPU node allocates on its executor and releases through the
  existing lifecycle; group Scroll does not need a separate mechanism.

## Review

- Independent AI review, 2026-11-25: changes requested. Findings: an oversized
  group must not fail the whole bind; the real-time exception, layer-list
  carrier, shared scroll records and `cool` were unstated; held and outgoing
  cue rules and the spec rewrite list were missing; failure and worst-case
  memory were unbounded; hidden running cues restart. Decisions 1-5 and the
  Consequences now address each finding.
- Independent AI re-review, 2026-11-25: changes requested. Span layer records
  needed reserved capacity and an intermediate-sized pass; the worst case
  memory and pool slots were not contained. Decision 3 now reserves span
  layers and adds the pass; decision 4 caps spans and summed pixels per
  compositor; decision 2 states record precedence.
- Independent AI re-review, 2026-11-25: changes requested. The control side
  cannot call `reserve_layers`; the shared pool's aggregate worst case and
  per-pass record writes were unstated. Decision 3 now bounds both passes by the
  node's existing capacity; decision 4 states the aggregate bound with lower
  per-compositor caps; decision 2 gives `render_scrolled` a base record slot
  and assigns a span's record before its members'.
- Independent AI re-review, 2026-11-25: the three prior findings are resolved;
  changes requested because a span's moving Scroll was not part of the
  redraw-on-change check. Decision 2 now includes it and the tests cover it.
  The reviewer stated that, with this change and the owner's acceptance of the
  1 GiB bound recorded, the verdict is approve.

Owner approval: Salih, 2026-11-25, after the independent review, including the
1 GiB worst-case bound of decision 4.
