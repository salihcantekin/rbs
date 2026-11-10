# ADR-0084: Source transform commands (flip, rotate, fit, align) and a categorized source context menu

- Status: Proposed
- Date: 2026-11-09
- Deciders: Salih (owner)
- Builds on: [ADR-0050](0050-command-registry-menus-and-toolbar.md) (command registry), [ADR-0053](0053-item-style-and-video-filter-chains.md), [ADR-0058](0058-crop-editing-of-sources.md), [ADR-0059](0059-scenes-lock-and-source-audio-in-the-studio.md), [ADR-0065](0065-global-sources-and-safe-source-removal.md), [ADR-0077](0077-source-health-retarget-and-settings-window.md)
- Spec: [scene-model](../specs/scene-model.md) (`Layout`), [control-api](../specs/control-api.md), [ui-design-system](../specs/ui-design-system.md) (Properties, Transform)

## Context

A scene item already carries everything needed for flips, rotation and fitting: `Layout` has a box (`position`, `size` where a negative component flips), a clockwise `rotation` about the box centre and a `crop`, and the compositor applies all of it in the layer transform at no extra GPU cost (gpu-compositor spec). But the user can only drag handles; there is no "Flip horizontal", no "Rotate 90°", no "Fit to screen". Properties shows position, size, rotation and crop fields, and the source context menu (preview and Sources panel) holds only the commands from ADR-0065 and ADR-0077. The owner (2026-11-09) wants Flip and other useful options in Properties, and the important ones in a categorized right-click menu on a source so commands are one click away.

Forces: commands come from the registry (ADR-0050), so a shortcut, the palette and a toolbar slot come for free; every user-visible command must be doable through the control API with the same undo behaviour (control-api spec); no new model fields if the existing ones suffice.

## Decision

1. **No model change, no scene format change.** Every command below edits `Layout` (and layer order, visibility, lock, mute, which exist). Each is one undo step on the user's undo stack. A single multi-field edit (for example Fit) is one record.
2. **Screen-space flips.** `Flip horizontal` and `Flip vertical` mirror the item as the user sees it on the canvas, whatever its rotation. With rotation `r`, a screen-horizontal flip negates the width and sets the rotation to `-r` (mod 360); a screen-vertical flip negates the height and sets the rotation to `-r` (mod 360). The box centre does not move. Flipping twice restores the layout exactly. Because only the parity of flips is canonical in `Layout` (`w<0,h<0,r` draws the same as `w>0,h>0,r+180`), the model gets one **normal form**: the height is never negative (a vertical flip is stored as a negative width with rotation `r+180`), rotation is in `0..360`; the loader, the handle drags and these commands normalise to it, and Properties derives its two toggles from it, so the toggles and the commands agree for every layout. Properties shows the state as two toggles (flipped horizontally / vertically, read as seen on the canvas), so the toggle and the command always agree.
3. **Rotation.** `Rotate 90° clockwise`, `Rotate 90° counter-clockwise` and `Rotate 180°` add to `rotation` (mod 360) about the centre; the box does not change. `Reset rotation` sets 0.
4. **Fit and align.** The commands, all relative to the canvas: `Fit to canvas` (the largest centred box whose rotated bounding box fits the canvas, with the cropped picture's aspect; for rotations that are not multiples of 90° it uses the bounding box of the rotated box; `Layout::fit` is extended or wrapped for this), `Fill canvas` (the smallest centred box that covers the canvas; parts outside the canvas are not drawn, crop is untouched), `Stretch to canvas` (box = canvas, aspect not kept), `Center`, `Center horizontally`, `Center vertically`, and `Reset transform` (rotation 0, no flips, the box at the picture's native size after crop, centre kept; crop, style and opacity untouched). A source that has no picture yet (camera not started) disables the fit commands with the reason, like other disabled registry items.
5. **Registry ids and API.** Ids: `item.flip_h`, `item.flip_v`, `item.rotate_cw`, `item.rotate_ccw`, `item.rotate_180`, `item.reset_rotation`, `item.fit`, `item.fill`, `item.stretch`, `item.center`, `item.center_h`, `item.center_v`, `item.reset_transform`. Like `item.set_crop`, they write the layout that method writes (the canvas override when the canvas has one, else the default layout). They are refused with `conflict` for a locked item, and with `invalid_params` when no picture size is known yet (still images and colour sources have one from their settings, ADR-0074); items of the global scene follow the same rules. Multi-selection is one undo step through `edit.apply` or an `items` array on each method. The control API gets the methods `item.flip {item, axis}`, `item.rotate {item, degrees}` (a multiple of 90), `item.fit {item, mode}` (`fit`, `fill`, `stretch`) and `item.align {item, axis?}`, class `control`, and `item.reset_transform {item}`, each one undo step, in a protocol minor version assigned at merge time (version and scene-format numbers are allocated in merge order, not claimed by ADRs). Existing methods keep working (`item.set_visible`, `item.set_locked`, layer order, `item.set_crop`, `source.reload`, `source.retarget`). The registry commands resolve to these methods (U20), so the registry test needs no special case.
6. **Properties.** The Transform tab gets a "Flip & rotate" row (two toggles and three rotate buttons) and a "Fit" row (Fit, Fill, Stretch) with the existing position, size, rotation and crop fields unchanged, built from library components only (ADR-0076). The values are the item's `Layout`; there is nothing to save separately.
7. **The categorized source context menu.** Right-click on an item (preview picture, Sources panel row, selection outline) shows the same menu everywhere (U3), in this order, with separators between categories and a submenu per category of more than three entries:
   - **Transform**: Flip horizontal, Flip vertical, Rotate (submenu: 90° clockwise, 90° counter-clockwise, 180°, Reset rotation), Fit to canvas, Fill canvas, Stretch to canvas, Center (submenu: both, horizontally, vertically), Reset transform.
   - **Arrange**: Bring to front, Bring forward, Send backward, Send to back (the existing layer-order commands).
   - **Visibility**: Hide / Show, Lock / Unlock, and for a source with audio Mute / Unmute.
   - **Source**: Source settings..., Filters..., Reload, Choose another target... (ADR-0077), Copy / Paste filters is a later addition.
   - **Remove**: Remove from scene, Delete source (ADR-0065, with its existing confirmation).
   Entries show their shortcut when one is bound, a check mark for state (Hide, Lock, Mute, Flip), and a disabled state with the reason. The menu applies to the selected item; when several items are selected the commands apply to all of them as one undo step, and commands that make no sense for several (Source settings...) are disabled with the reason. Which entries the menu shows is a registry property (`menu = "source-context"`, category and order), so a later command joins a category by registration, not by editing the menu. Plugins cannot add entries until the plugin SDK declares commands (ADR-0050).
8. **Zero cost.** The transform is already in the layer transform; the commands run on the control thread when invoked and touch the render path only through the normal scene update. No new node, no per-frame check.
9. **Visual change.** Per rule 16 the owner approves a proposal of the Properties rows and the menu (categories, icons, order), then screenshots in three themes, before merge. Icons come from the icon set; a missing icon is added to it in the same change.

## Consequences

- Flip, rotate and fit become one-click and keyboard-bindable, remote panels (Stream Deck) get them through the API, and the registry test (U20) covers them.
- Screen-space flips differ from the raw sign of `size` once an item is rotated. That is why Properties shows the on-canvas state, not the stored sign, and why `scene-model.md` documents the mapping.
- The multi-select rule needs the selection model to expose several items; until it does, the menu applies to the one selected item.
- Spec follow-ups in the implementing PRs: `scene-model.md` (the flip mapping), `control-api.md` (methods), `ui-design-system.md` (Properties rows, menu), `glossary.md`.

## Alternatives considered

- **Store `flip_x` / `flip_y` booleans on `Layout`.** Clearer on disk but a scene format change for something the sign of `size` already encodes; rejected under "no model change".
- **Raw-sign flips (negate size, keep rotation).** Simplest, but a rotated item would mirror along its own axis, which is not what "Flip horizontal" means to a user.
- **Fill by cropping.** Destroys information and fights ADR-0058; drawing the overflow outside the canvas costs nothing.
- **A menu per surface.** The preview, the Sources panel and the selection outline would drift apart (the problem ADR-0050 removed).

## Review

- 2026-11-09, independent AI session (round 1, reviewing only the ADRs, specs and code): changes requested. Concerns covered contract details, claims about existing code, hot-path and zero-cost rules, and number allocation across these ADRs; each was resolved in the text above (the numbers of protocol minors and scene formats are now assigned at merge time, not claimed by ADRs).
- Round 2 (verification by a different fresh reviewer) and the owner's approval are still needed before the status can become Accepted (ADR-0014).

## Implementation notes

- 2026-11-10: registry ids are parameterised instead of one id per variant: `item.flip?axis=`, `item.rotate`, `item.fit` and `item.align` (pickers, shown as submenus), `item.reset_transform`. `Reset rotation` is not a separate command (Reset transform and the rotation field cover it), and `item.align` takes `axis` of `both`, `horizontal` or `vertical`. The Arrange, Source and Remove menu categories and multi-selection follow in later changes; protocol minor 1.18 was taken at merge time.
