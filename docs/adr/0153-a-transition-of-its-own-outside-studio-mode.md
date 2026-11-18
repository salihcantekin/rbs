# ADR-0153: A transition of its own outside studio mode

- Status: Accepted
- Date: 2026-11-18
- Deciders: Salih (owner)
- Amends: ADR-0128 (scene transitions: one collection kind for every program change), decision 14 (the choice outside studio mode)
- Builds on: ADR-0061 (studio mode), ADR-0100 (glide), ADR-0135 (a scene's own transition, one-off kinds), ADR-0140 (Stinger), ADR-0111 (new values of request fields)

## Context

ADR-0128 gives the scene collection one transition kind, used by every program change: `canvas.transition` in studio mode and `scene.switch` outside it. The owner reported (2026-11-18) that with Slide chosen on the Transition rail, switching scenes outside studio mode slides too, where he expects the plain Move (sources glide to their new boxes). Outside studio mode there is also no place to see or change that kind: the rail exists only in studio mode, and ADR-0128 decision 14's Scene menu choice was never built.

The owner chose (decision card, 2026-11-18) a separate setting for outside studio mode, Move by default, over "always Move outside studio mode" and over keeping today's behaviour.

## Decision

1. **Two kinds.** `CollectionTransition` gains `direct: TransitionKind`, the kind a program change uses while the canvas is not in studio mode (it has no preview scene, ADR-0061). `kind` stays the kind of studio mode (the rail's choice). Default `Move`. A canvas is in studio mode when the studio's main canvas has a preview scene; further canvases follow the main canvas's mode (they have no studio mode of their own).
2. **Shared settings.** The settings of each kind (Fade's duration, easing and through colour, Swipe's and Slide's duration, easing and direction, the stinger, Move's glide) are one set, used by both. Only which kind runs differs.
3. **Precedence unchanged otherwise.** A one-off kind (`scene.switch {kind}`, `canvas.transition {kind}`, ADR-0128 decision 13) wins, then the incoming scene's own transition (ADR-0135 decision 8), then the collection's kind for the mode (`kind` in studio mode, `direct` outside).
4. **Document.** Scene format 27 adds `transition.direct` (`"cut"`, `"fade"`, `"move"`, `"swipe"`, `"slide"` or `"stinger"`), omitted while Move. A document of format 26 or older has none, so it switches with Move outside studio mode (the owner's expected behaviour). A format 18 or older document maps its glide to `kind` as before (ADR-0128 decision 3) and, like every older document, switches with Move outside studio mode. An older build refuses format 27 without changing the file, as for every format change.
5. **Control API 1.97.** `collection.set_transition` takes an optional `direct_kind` (merged like the other parts, one undo step); the collection's `transition` in `scene.list` and `scene.changed` carries `direct_kind`, omitted while Move. A 1.96 server refuses the field (`invalid_params`), so a client checks for 1.97 first. This also changes the meaning of an existing field: before 1.97 `kind` was used by every program change, from 1.97 only in studio mode, so a 1.96 client that sets `kind` and calls `scene.switch` outside studio mode now gets Move. The control API counts that as major; like ADR-0098 and ADR-0151 decision 7, this ADR takes it as a stated exception at a minor version, listed in the spec's versioning paragraph: the owner asked for exactly this behaviour change, the request and answer shapes are unchanged, and a major version would cost every client a migration for one setting.
6. **UI.** The Scene menu gets a submenu at its end, after Rehearse on Preview (so it is not mistaken for the Transition action), "Transition outside studio mode", with Cut, Fade, Move, Swipe, Slide and Stinger, the current one checked (registry entry `collection.direct_transition`, a picker on `collection.set_transition` with parameter `direct_kind`). Durations and the other settings stay where they are (the rail's options, Settings › General for the glide). No new tokens or windows. The rail is unchanged and sets `kind`.
7. **Zero cost.** One more enum in the collection; the studio reads it when a program change is noted, never per frame (P13).

## Consequences

- Switching scenes outside studio mode is Move unless the user picks another kind for it; the studio-mode kind no longer leaks into plain switches.
- Users who relied on the rail's kind outside studio mode now get Move until they pick it in the Scene menu. This is the owner's intended default.
- The settings of the outside kind (durations, directions, the stinger file) are still set on the rail, which shows only in studio mode, and picking Stinger with no stinger file runs a Cut (as in studio mode, ADR-0140). Accepted for now; a follow-up may show the settings in the Scene menu or Settings, or disable Stinger there while no file is set.
- Crate semver: `rbs_scene::CollectionTransition`, `rbs_api::types::Transition` and `rbs_api::request::SetCollectionTransition` gain a public field and are not `#[non_exhaustive]`, so Rust code that builds them with a struct literal without `..` breaks (the in-tree callers are updated). The wire protocol stays compatible as described in decision 5.
- Specs: scene model (`CollectionTransition::direct`), user data (format 27), control API (1.97 section, `collection.set_transition`), recording (which kind a change uses), UI design system (Scene menu).
- Tests: format 26 → 27 round trip and the default; precedence in the studio (studio mode uses `kind`, outside uses `direct`, the scene override and the one-off still win); API set and snapshot; the menu's checked choice and its run.

## Alternatives considered

- **Always Move outside studio mode** (no setting). Simplest, but a user who wants a Fade for plain switches would have no way to get one. Not chosen by the owner.
- **Keep one kind for both modes** (ADR-0128 as is). The owner reported it as a bug.
- **Separate settings per mode for every part** (two Fade durations, and so on). Doubles the options for little gain; the kinds differ, their tuning rarely does.

## Review

Independent review per ADR-0014, 2026-11-18, a fresh AI session given the ADR, the ADRs it builds on, the specs and the diff: changes requested, 1 blocking and 7 minor findings, resolved in this revision.

1. Blocking: the meaning of `collection.set_transition {kind}` changes in a minor version. Resolved: decision 5 states the exception, and the control API versioning paragraph lists it.
2. `from_legacy` gave `direct` the legacy kind, against decision 4 and the scene model spec. Resolved: `direct` is Move, with a test.
3. Precedence was not tested against `direct`. Resolved: the rbs-record test now checks that a one-off kind and the incoming scene's transition both win over `direct` outside studio mode. Further canvases are not tested separately: the studio computes one effective transition per update, from the main canvas's mode, for every canvas.
4. The app test for leaving studio mode set both kinds to Cut. Resolved: a second test keeps `direct` at Move and checks that leaving studio mode, with and without an edit in Preview, reports no transition.
5. The outside kind's settings are reachable only on the rail; Stinger without a file cuts. Recorded in Consequences as accepted, with a follow-up.
6. Crate semver impact unstated. Resolved in Consequences.
7. Visible change needs the owner's approval; the picker beside the Transition action is easy to confuse. Dark and light screenshots are made for the owner, who approved visual changes for this work in advance (2026-11-18); the picker moved to the end of the Scene menu.
8. The ADR-0128 note is worded as a Review record.

Owner acceptance, 2026-11-18: Salih chose the design (decision card, "separate setting, default Move"), said "başla" to the proposal, and approved merges and visual changes of the running work in advance the same evening, which accepts this ADR, including the minor-version exception of decision 5.
