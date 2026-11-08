# ADR-0075: Looks and the Library panel: presets of style and filters, and generated sample pictures

- Status: Proposed
- Date: 2026-11-08
- Deciders: Salih (owner)
- Builds on: ADR-0049 (workspace layout), ADR-0053 (item style and video filters), ADR-0057 (themes), ADR-0074 (still sources), ADR-0013 (zero cost when unused)

## Context

The owner wants the app to look good from the first minute: a Library with backgrounds, frames, a few filter presets and colour-adjustment presets, so that a source can be made to look right in one click. Today a user must open Properties, add a filter and type its values.

Forces: P13 (an unused feature costs nothing), U-rules and rule 15 (tokens only), rule 17 (find what the reference tools do badly), and licensing (ADR-0074: sample pictures are generated, original work).

## Decision

1. **A look is a copied preset.** `rbs_scene::look::Look` holds an id, a name, a group (`Color`, `Effect`, `Shape`), an optional `ItemStyle` (shape, border) and a list of filters (type id and settings). Applying a look copies its values onto the selected source's item and filter chain. Nothing links the source to the look afterwards, so editing the filters later never changes the look and removing the look needs no cleanup. The model, the saved scene collection and the control API are unchanged (no format version, no new API method).
2. **Built-in looks are code**, ids `builtin:<slug>`: Colour (Warm, Cool, Vivid, Muted, Black and white, Soft contrast), Effects (Soft blur, Sharpen, Green screen), Shape (Circle, Circle with border, Rounded corners, Rounded with frame, No shape). They live in `rbs-scene` as data only; `rbs-scene` does not depend on the filter catalogue, so a test in `rbs-sources` checks that every filter a look names is accepted by the catalogue with exactly those settings. User-saved looks and shareable packs are a later step and need their own decision.
3. **Apply semantics.** A look's style replaces the item's style on the canvas being edited. For each filter, a filter of the same type already on the source has its settings replaced (and is enabled); otherwise the filter is appended. Filters the look does not name are left alone. All values are validated strictly before any change; on a failure everything done so far is rolled back and the user gets a typed refusal. A successful apply is exactly one undo step (`EditKind::Look`), also when it changes the style and several filters. Applying a look that changes nothing records nothing.
4. **Samples are added by a command.** `AddSample` adds an `org.rbs.source.image` source (ADR-0074) with a built-in `sample` id covering the canvas. A background is inserted below the other items, a frame above them. The two edits are one undo step.
5. **The Library is a panel** (`rbs.library`), by default an auto-hide group on the left edge in the Edit workspace (ADR-0049), so it costs no screen space until opened. It shows Looks grouped by kind with an Apply button on each row (disabled while no source is selected) and the sample pictures with a thumbnail and an Add button. It uses only library components and tokens; the two thumbnail sizes are the tokens `size.library.thumb-w` and `size.library.thumb-h`, not themeable.
6. **Zero cost when unused (P13).** The look and sample data are static or generated on demand. Thumbnails are generated once, when the panel is first shown, not at startup, and a closed panel does no work. Nothing runs per frame.
7. **Linux preview.** The Linux build does not draw the program picture yet, so the visual effect of looks and pictures there is checked through the filter list and the generated thumbnails; the effect is verified on Mac and Windows.

## Consequences

- One click gives a graded, shaped or framed source, and one Undo reverses it.
- Looks do not follow later edits of the preset (a copy, not a reference). A user who wants a changed Warm edits the filter values; saving own looks comes later.
- `rbs-app` gains `Command::ApplyLook` and `Command::AddSample`; `SourceKind` gains `Image` and `Color` (not yet in the Add source menu, a follow-up with the PNG file picker).
- Saved workspaces made before this change have no Library tab; it is available from View > Panels and Reset workspace.

## Reference tools (CLAUDE.md rule 17)

- **OBS Studio** has no built-in presets for filters or shapes. A user adds a Colour Correction or LUT filter and sets values by hand, and a rounded or circular source needs a mask image or a plugin. Pictures and frames come from files the user makes elsewhere. Settings are per filter, and nothing groups a recipe of several of them.
- **Where this falls short today:** no user-saved looks, no sharing of packs, no LUT looks, no previews of a look on the user's own source before applying it.
- **What RBS does better by design:** a look is one click and one undo step across style and filters together (a circular, bordered, colour-graded camera in one action); looks and samples are generated in code, so they add no binary assets, no licence questions and no cost until the panel is opened; shape and border are ordinary item style (ADR-0053) and need no mask file.

## Alternatives considered

- **Looks as live links to a preset.** Editing a preset would change every source that uses it, but it needs a reference model, a missing-preset state and a format change. A copy is simpler and matches how OBS settings behave.
- **A new model type for looks in the scene collection.** Unneeded while looks are built in; it can come with user-saved looks.
- **Shipping sample pictures as files.** Rejected in ADR-0074.

## Review

Reviewer: not yet reviewed (needs an independent AI session or a human, rule 11).
