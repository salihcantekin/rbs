# ADR-0136: No halo in the frame bar, so Glow keeps its height

- Status: Accepted
- Date: 2026-11-17
- Deciders: Salih (owner)
- Builds on: [ADR-0122](0122-bold-theme-effects.md) (bold theme effects), [ADR-0120](0120-theme-effects.md), [ADR-0071](0071-merged-title-bar-on-windows.md) (merged title bar)
- Supersedes in part, in **ADR-0122**:
  - **Decision 8, "The first positions to opt in", the item "The frame bar's Go live"** with all its lines (height, caption regions, other items keep their size, window buttons, horizontal, which cell opts in, fit). The frame bar no longer opts in to the halo.
  - **Decision 10, the rendered test's frame-bar lines:** "The halo tests run on the real FrameBar through `MainWindow` …" and "The halo against a focus ring" (Go live's neighbours with the halo lit, as Buttons and as `vertical-menu` SplitButtons).
  - **Consequences,** the bullet "Glow changes layout at the opted-in positions" as far as it names the frame bar ("The frame bar grows by about 22 px, and Go live gets about 7 px more on each side").

  Everything else in ADR-0122 stays: the halo itself, `Theme.halo-clear`, `halo-room` on Button and SplitButton, the clear-zone rule for any position that opts in, and DialogActions as an allowed later position.

## Context

ADR-0122 decision 8 made the frame bar's Go live the first position that opts in to the Glow halo. The halo reaches 12 px past the button, and its clear zone (`Theme.halo-clear` + `size.focus-ring`) must stay free on every side. So in Glow the frame bar grows from `size.control.md + 2·space.1 + size.border` (37 px) to `size.control.md + 2·(halo-clear + size.focus-ring) + size.border` (59 px), and Go live's cell gets about 7 px more on each side.

After using it, the owner reported (2026-11-17) that choosing Glow makes the top toolbar visibly taller than normal and asked whether it is a bug. It is not a bug; the code follows ADR-0122. But:

- Toolbar space is precious (owner, 2026-11-14), and the frame bar is on screen all the time.
- Changing the look moves the whole studio down by 22 px. A look should change how controls look, not where the panels are.
- The halo's purpose was to make the main action stand out. In the frame bar Go live already stands out: it is the only primary and has the accent fill, and in Glow it keeps the Glow shadow.

The owner chose this option on 2026-11-17 over a smaller halo for the bar only and over keeping the 59 px bar.

## Decision

1. **The frame bar does not opt in to the halo.** The binding `halo-room: item.id == "stream.toggle"` at the `Shell.toolbar-fixed` instance in FrameBar is removed, so `halo-room` is false (its default). In Glow, Go live in the frame bar looks like every other primary that does not opt in: the Calm face plus the Glow shadow (ADR-0122 decision 8, "Every other primary looks exactly as in calm, apart from its shadow").
2. **The frame bar keeps one height in every look.** FrameBar's `row-pad` is `space.1` in Calm, Glow, Glass and Tactile, so the bar is `size.control.md + 2·space.1 + size.border` tall. Only `row-pad` changes. `ToolbarCell.halo-pad` stays as it is and is 0 at every position, because no instance sets `halo-room`. The separator keeps its fixed height `size.control.md − 2·space.1`, and the window buttons still span the row (ADR-0071).
3. **The pass-throughs and the caption handler stay.** `ToolbarCell` keeps its `halo-room` pass-through, false by default, so a later ADR can opt a fixed-size toolbar position back in without changing the library. FrameBar keeps `changed height => publish-regions()`: the bar's height still changes with density (`size.control.md`), and the caption row must follow it.
4. **No position opts in to the halo today.** DialogActions in held-edit windows may still opt in when it is implemented, with the window's minimum size including the zone. The probe's opted-in row (`overlay_probe.slint`, a plain Button) keeps the halo, its clear zone and its layout rule tested. A SplitButton with `halo-room` (the zone wider by `size.focus-ring`) then has no rendered test; the ADR that opts a SplitButton position in adds one to the probe.

## Consequences

- In Glow the frame bar is 37 px tall, the same as in the other looks, and the studio below it no longer moves when the look changes.
- Glow loses its most visible halo. Glow still differs clearly from Calm through the rims, the inner glow and the shadows (ADR-0122 decision 7).
- This is a visible change (CLAUDE.md rule 16): the PR carries frame-bar screenshots in Calm and Glow, dark and light, and merges only after the owner approves them.
- Tests (`crates/rbs-app/tests/effects_render.rs`):
  - `the_frame_bar_keeps_the_halo_zone_and_its_neighbours_rings` becomes `the_frame_bar_keeps_its_layout_and_its_neighbours_rings`: in Glow the bar has the same height and the same control boxes as in Calm, Go live draws no halo (nothing of its effects lies beyond the extent of its box), and the rings of the item before Go live, of Record and of Record ▾ still pass the ring rule beside Go live's Glow shadow and Record's rim and inner glow. It still runs as plain Buttons and as SplitButtons, from the regular test and from the slow compact test, which is renamed `slow_effects_keep_every_focus_ring_at_compact_density` because it no longer checks a frame-bar zone.
  - `the_caption_row_follows_the_frame_bar` checks the published caption row in Calm and Glow (the same height) and after a switch from Regular to Compact density (the new height).
  - The probe tests (`the_probe_keeps_the_halo_zone_clear`, `effects_change_the_layout_only_at_opted_in_positions`) stay.
  - `studio_ui`'s fit test keeps Calm and Glow; only its comment changes.
- Specs: `docs/specs/ui-design-system.md` ("Positions that opt in today": none; DialogActions may) and `docs/specs/themes.md` (the rendered-test lines that name the real frame bar: the clear zone, the halo against a neighbour's ring and the caption row) are updated in the same change.
- Stale comments are updated in the same change: FrameBar's row comment and the separator comment in `app.slint`, and the fit-test comment in `studio_ui.rs`.
- The unused `Button.fx-glow` property (`components.slint`) is removed in the implementing PR.
- No token, theme file or token format changes. No hot path is touched.
- **On acceptance:**
  - `docs/adr/README.md` gets a row for ADR-0136, and ADR-0122's row adds "decision 8 (frame bar) superseded in part by ADR-0136".
  - ADR-0122's Review gets the pointer "decision 8's frame-bar position, decision 10's frame-bar test lines and the frame-bar Consequences line superseded by ADR-0136".
  - The `ui-review` item 17 stays; it applies to any position that opts in.

## Alternatives considered

- **A smaller halo only in the frame bar** (4 to 5 px blur). It still grows the bar by 6 to 8 px, needs a second halo size per position (a new token and a rule), and a halo that small is hard to see.
- **Let the halo overflow the bar onto the panels below.** It breaks ADR-0122's clear-zone rule (no clip edge and no other content inside the zone) and would draw over the preview and the docks.
- **Keep it as it is.** The owner reported the growth as a problem.

## Review

- 2026-11-17, independent AI session (fresh context; only the proposal, the specs and the code), round 1: changes requested. Its points were the supersede scope (decision 10 and the Consequences line), the caption-row and slow compact tests, the untested SplitButton halo, keeping the neighbour-ring check, decision 2 contradicting decision 3 on `halo-pad`, the missing themes.md lines, stale comments, the index and pointers, rule 16 screenshots, and the unused `Button.fx-glow` property. Each point is resolved in this revision. The unused property is removed in the implementing PR. 
- 2026-11-17, the same independent session, round 2: approve. Its two nits (the `fx-glow` removal named in Consequences, and renaming the slow compact test) are applied.

2026-11-17 12:34 UTC: owner approval after the dark and light screenshots (Salih: "tamamdır güzel olmuş onaylıyorum"). Status Accepted.
