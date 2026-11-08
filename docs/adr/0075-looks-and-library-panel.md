# ADR-0075: Looks and the Library panel: presets of style and filters, and generated sample pictures

- Status: Accepted
- Date: 2026-11-08
- Deciders: Salih (owner)
- Builds on: ADR-0049 (workspace layout), ADR-0053 (item style and video filters), ADR-0057 (themes), ADR-0074 (still sources), ADR-0013 (zero cost when unused)

## Context

The owner wants the app to look good from the first minute: a Library with backgrounds, frames, a few filter presets and colour-adjustment presets, so that a source can be made to look right in one click. Today a user must open Properties, add a filter and type its values.

Forces: P13 (an unused feature costs nothing), U-rules and rule 15 (tokens only), rule 17 (find what the reference tools do badly), and licensing (ADR-0074: sample pictures are generated, original work).

## Decision

1. **A look is a copied preset.** `rbs_scene::look::Look` holds an id, a name, a group (`Color`, `Effect`, `Shape`), an optional `ItemStyle` (shape, border) and a list of filters (type id and settings). Applying a look copies its values onto the selected source's item and filter chain. Nothing links the source to the look afterwards, so editing the filters later never changes the look and removing the look needs no cleanup. The model and the saved scene collection are unchanged (no format version).
2. **Built-in looks are code**, ids `builtin:<slug>`: Colour (Warm, Cool, Vivid, Muted, Black and white, Soft contrast), Effects (Soft blur, Sharpen, Green screen), Shape (Circle, Circle with border, Rounded corners, Rounded with frame, No shape). They live in `rbs-scene` as data only; `rbs-scene` does not depend on the filter catalogue, so a test in `rbs-sources` checks that every filter a look names is accepted by the catalogue with exactly those settings. User-saved looks and shareable packs are a later step and need their own decision.
3. **Apply semantics and scope.** A look's style replaces the style of the selected item on the canvas being edited. Filters belong to the source (ADR-0053), so the scope of one look is mixed: the shape and border change in the edited scene only, the filters change the source everywhere it is used (other scenes, other canvases, a global source of ADR-0065). If the source appears twice in the edited scene, one of them (the first found) is styled. For a global source the item is its item of the canvas. The Apply note says that a filter of the same kind is replaced in every scene that uses the source.
   For each filter, a filter of the same type already on the source has its settings **replaced** (keys the look does not name go back to the type's defaults, and the filter is enabled); otherwise the filter is **appended at the end of the chain**, after the user's own filters. Two looks that use the same filter type therefore do not stack: Warm and then Vivid leaves Vivid. The Apply note says so in general words, because Shape looks touch no filter. Filters the look does not name are left alone. All values are validated strictly before any change; on a failure everything done so far is rolled back and the user gets a typed refusal. A successful apply is exactly one undo step (`EditKind::Look`), also when it changes the style and several filters. Applying a look that changes nothing records nothing.
4. **Samples are added by a command.** `AddSample` adds an `org.rbs.source.image` source (ADR-0074) with a built-in `sample` id, placed over the whole canvas and **stretched** to it (no fit or fill yet, so a 16:9 frame on a 9:16 canvas is distorted; fit and fill come with the Add source picker). Each sample is generated at 1920 x 1080, whatever the canvas, which is 8 MiB of CPU pixels per instance (ADR-0074, decision 10) and no more for a 4K canvas. A background is inserted below the other items, a frame above them. The two edits are one undo step.
5. **The Library is a panel** (`rbs.library`), by default an auto-hide group on the left edge in the Edit workspace (ADR-0049), so it costs no screen space until opened. It shows Looks grouped by kind with an Apply button on each row (disabled while no source is selected) and the sample pictures with a thumbnail and an Add button. It uses only library components and tokens; the two thumbnail sizes are the tokens `size.library.thumb-w` and `size.library.thumb-h`, not themeable.
6. **Zero cost when unused (P13).** The look and sample data are static or generated on demand. The panel content is instantiated only while its tab or flyout is shown (`if tab.id == "rbs.library"` in `app.slint`), and the thumbnails are made in its `init`, once, on the UI thread (eight pictures of 288 x 162, about 1.5 MiB, kept for the session). A closed panel does no work. Nothing runs per frame.
7. **Control API (U20, ADR-0008): dated exception.** Applying a look and adding a sample are not yet control API commands, so an agent or a Stream Deck cannot do what the panel does. This exception is proposed for the owner to confirm with this ADR (2026-11-08), and the commands are a follow-up of the next API minor version: `look.list`, `look.apply(source, look)` and `source.add_sample(sample)`, with the picture kinds added to the source kind enumeration. Until then, clients see an Image or Colour source as a source with no `kind` (the source type id is still reported).
8. **Spelling.** UI text uses "Colour" (glossary, look group, source label).
9. **Linux preview.** The Linux build does not draw the program picture yet, so the visual effect of looks and pictures there is checked through the filter list and the generated thumbnails; the effect is verified on Mac and Windows.

## Consequences

- One click gives a graded, shaped or framed source, and one Undo reverses it.
- Looks do not follow later edits of the preset (a copy, not a reference). A user who wants a changed Warm edits the filter values; saving own looks comes later.
- `rbs-app` gains `Command::ApplyLook` and `Command::AddSample`; `SourceKind` gains `Image` and `Color` (not yet in the Add source menu, a follow-up with the PNG file picker).
- `Look` holds `&'static str` data. User-saved looks need an owned, serialisable twin and a decision of their own.
- Applying a look is not previewed first. A hover preview on the user's own source (one extra filter-chain pass on the GPU only while hovering, nothing otherwise) is the next step beyond parity.
- Saved workspaces made before this change have no Library tab; it is available from View > Panels and Reset workspace.

## Reference tools (CLAUDE.md rule 17)

- **OBS Studio** has Colour Correction and LUT filters (with a few bundled LUT files), and rounded or masked sources are possible with mask and blend filters or an image mask; each is set up by hand, filter by filter, and nothing bundles a recipe of several filters and a shape. Backgrounds and frames are files the user makes elsewhere. (Checked against the OBS documentation as known in 2026; the owner or a reviewer should re-check against the current release.)
- **Streamlabs Desktop** ships a theme and overlay library with ready-made scenes and frames, which is a store of designs, not a set of presets for one source's colour and shape. **vMix** has colour correction presets per input.
- **Where this falls short today:** no user-saved looks, no sharing of packs, no LUT looks, no preview of a look on the user's own source before applying it, and looks do not stack when they use the same filter type.
- **What RBS does better by design:** a look is one click and one undo step across style and filters together (a circular, bordered, colour-graded camera in one action); looks and samples are generated in code, so they add no binary assets, no licence questions and no cost until the panel is opened; shape and border are ordinary item style (ADR-0053) and need no mask file. The next step, a live hover preview, is cheap in a GPU compositor.

## Alternatives considered

- **Looks as live links to a preset.** Editing a preset would change every source that uses it, but it needs a reference model, a missing-preset state and a format change. A copy is simpler and matches how OBS settings behave.
- **A new model type for looks in the scene collection.** Unneeded while looks are built in; it can come with user-saved looks.
- **Shipping sample pictures as files.** Rejected in ADR-0074.

## Review

Reviewer: independent AI session (round 1), PR #214 comment, 2026-11-08
Verdict: approve with changes; kept Proposed until fixed
Concerns:
1. U20 / ADR-0008: no control API commands for looks and samples. -> Recorded as a dated exception for the owner to confirm, with the follow-up commands named (decision 7).
2. Mixed scope (style per item, filters per source, global sources, duplicate items). -> Resolved in decision 3.
3. Replace semantics, no stacking, chain position. -> Resolved in decision 3; the Apply note says a filter of the same kind is replaced in every scene that uses the source.
4. Sample size and fit. -> Resolved in decision 4: stretched, generated at 1920 x 1080 (8 MiB per instance).
5. Zero-cost claim for the closed panel. -> Resolved in decision 6 (content is instantiated only while shown; thumbnail cost stated). No automated test: the Slint instantiation is not testable without a window.
6. Reference tools overstated and shallow. -> Rewritten; Streamlabs and vMix added; hover preview named as the next step.
7. Spec sync. -> Panel id list, `LibraryRow`, flyout size 320, redraw policy and glossary (Look, Library) updated.
8. Spelling. -> "Colour" everywhere (decision 8).
9. API clients see no kind for Image and Colour. -> Stated in decision 7.
Smaller notes: the workspace view test now asserts that the Library edge tab is present; the owned twin of `Look` for user-saved looks is noted in Consequences.

Round 2: same reviewer session, at 239ad75: all nine concerns addressed, approved. Two nits (the Apply note must name the every-scene scope and must not name Colour correction for Shape looks) fixed after the round. The owner confirmed the U20 exception in decision 7 on 2026-11-08, and the ADR is Accepted.
