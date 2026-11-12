# ADR-0101: Mixer columns use fixed strips, side-by-side clusters and horizontal scroll

- Status: Accepted
- Date: 2026-11-11
- Deciders: owner
- Refines: [ADR-0078](0078-saved-section-state-and-shared-section-components.md) decision 5 (Mixer arrangements), [ADR-0083](0083-mixer-orientation-chosen-by-the-user.md) decision 5 (columns scroll, min strip width), [ADR-0081](0081-master-strip-in-the-mixer.md) (Master placement)

## Context

The Mixer's columns layout shared the panel width across group clusters and drew foldable `SectionHeader`s above the strips. Those headers' preferred width widened each cluster, and forcing the cluster row to the panel width squeezed strips until Master (and its level readout) clipped off the right edge. With three sources the fader column collapsed. ADR-0083 already required a minimum strip width and horizontal scroll in columns, but the design-system text and the implementation had drifted to "columns do not scroll / strips shrink". The owner also asked that strip buttons sit bottom-aligned beside the fader, and that columns drop fold for This scene / Global in favour of resize (or reorder).

## Decision

1. **The panel assigns the box.** Every columns strip is `size.mixer.strip` wide and at least `size.mixer.min-h` tall (the room under the group caption). The name elides. Positions inside the strip are explicit: the meter and the fader fill that box, and the readout, clip, monitor, mute and more are five fixed slots anchored to the bottom. Master's monitor and more slots stay empty and keep their height. A panel shorter than the row (caption plus that minimum, plus padding) scrolls vertically; a taller panel gives the extra height to the fader and keeps the buttons on screen.
2. **This scene and Global stay open in columns.** They use a compact uppercase caption that elides to the pane width and never sets preferred width. They must not collapse sideways to an edge tab in columns. Rows keep foldable section headers (ADR-0078).
3. **Side-by-side clusters.** This scene, Global, Master (after a divider) and Monitor pack left to right at their preferred widths. They must not stretch to opposite edges of a wide panel.
4. **Horizontal scroll of the row.** When the clusters do not fit, the whole row scrolls horizontally. Columns must not squeeze strips below `size.mixer.strip`, and Master must not overlap a source strip.
5. **Hidden.** Folded, Hidden stays a sideways strip. Open, each hidden source is a vertical strip of `size.mixer.strip` (name, Show button, no fader), in the same row as the other strips. The rows arrangement keeps the compact horizontal row (ADR-0080).
6. **Spec sync.** `ui-design-system.md` (Mixer faders, meters tokens) and the themes "not themeable" list must match this decision in the same change.

## Consequences

- Columns stay readable with many sources; Strips stay adjacent; shrinking the panel scrolls instead of overlapping.
- Columns no longer fold This scene / Global; fold remains in rows where vertical space is the scarce resource.
- Token `size.mixer.splitter` remains available; columns pack by preferred width. Optional `scene_share` in `panel_options` is reserved but unused while clusters pack naturally.
- Visual change needs owner screenshots (rule 16) before merge.

## Alternatives considered

- **Keep foldable headers in columns, only bottom-align buttons.** Headers still force width; the overflow bug remains.
- **One horizontal scroll for all strips including Master.** Master can leave the viewport; the owner required it stay visible.
- **Reorder groups by drag instead of a splitter.** Larger interaction surface; splitter matches existing dock resize language and is enough for crowded Global.

## Review

- 2026-11-12: Owner (Salih) accepts this ADR. Column strips keep `size.mixer.strip`, clusters sit side by side, and the row scrolls horizontally. Status: Accepted.
