# Spec: User data

- Status: Draft
- Owner crate(s): `rbs-user-data`; `rbs-scene` (`SceneCollection::from_parts`)
- Related ADRs: [0014](../adr/0014-review-policy.md), [0017](../adr/0017-flexible-panels-and-workspaces.md), [0021](../adr/0021-scene-collection-model.md), [0049](../adr/0049-main-window-areas-panel-states-and-sizes.md), [0051](../adr/0051-user-data-profiles-and-scene-collections.md)

## Purpose

What users make and keep (output profiles, scene collections, and later themes, presets and library assets) is stored as versioned TOML documents under the config directory, in libraries of named entries that can be created, renamed, duplicated, deleted, imported and exported. This spec defines the document envelope, the library rules, the directory layout, the output profile and the scene collection document.

## Scope

In scope: document versioning and safe writes; libraries; `current.toml`; output profiles as sections; the scene collection document format.

Out of scope, later changes: workspaces (ADR-0017 and ADR-0049, owned by `rbs-ui`, until a change of their own moves them onto a library); themes and the asset library (directories reserved); packs (`.rbspack`); the app's menus and settings windows ([UI design system spec](ui-design-system.md), Output profile and Scene collection menus); how the app's recording and stream sections look (they keep their own `format_version` and are documented with the app).

## Contract

### Layout (`UserData`)

`UserData::new(config)` with the config directory (`rbs_remote_settings::config_dir()`):

| Path | Contents |
| --- | --- |
| `profiles/<id>.toml` | Output profiles (`UserData::profiles()`) |
| `scenes/<id>.toml` | Scene collections (`UserData::scene_collections()`) |
| `current.toml` | `Current { profile, scene_collection }`: ids of the entries in use (`current()`, `set_current()`) |
| `themes/<id>.toml`, `appearance.toml` | User themes (`UserData::themes()`) and the appearance choice (`appearance()`, `set_appearance()`), [themes spec](themes.md), ADR-0057 |
| `shortcuts.toml` | The user's changes to the default shortcuts (`UserData::shortcuts()`, `set_shortcuts()`), [shortcuts spec](shortcuts.md), ADR-0060 |
| `workspaces/`, `library/` | Reserved (`WORKSPACES_DIR`, `LIBRARY_DIR`) |

`current()` returns an unset selection and the reason when `current.toml` cannot be read; an id that names no entry is treated as unset by the caller.

### Documents (`document`)

A kind implements `Document`: `KIND`, `FORMAT` (current version, from 1), `from_body`, `to_body` and optionally `upgrade(from, body)` (one step, `from` to `from + 1`). The file is a TOML table:

| Key | Meaning |
| --- | --- |
| `format` | Integer, at least 1 |
| `kind` | `Document::KIND` |
| `name` | Display name (library entries) |
| anything else | The body |

- `scan_format(text)`: the first top-level `format = <integer>` line, found without parsing (before any `[` line). `parse`, `peek` and `write` use it to recognise a newer file even when its syntax cannot be parsed.
- Files larger than `MAX_FILE` (16 MiB) are not read (`Invalid`). A file that large in the way of a `write` is not read whole: its first 4 KiB are scanned for a newer format, else it is moved aside.
- `peek` reads only the envelope (`Envelope { format, kind, name }`), parsing the lines before the first table when they hold all three, else the whole file; it checks kind and format but not the body.
- `parse` / `read`: a file without an integer `format` of at least 1 or without `kind` is `NoVersion`; another kind is `WrongKind`; a higher format is `Newer`; a lower one is upgraded step by step before `from_body`. `Read` reports the name, the format found and the document. Invalid TOML is `Syntax`; a body that does not fit is `Invalid` with the reason.
- `render` / `write`: writes `format = FORMAT`, `kind`, `name` if given, then the body; a body using a reserved key is `Invalid`. `write` refuses with `Newer` when the file on disk has a higher format (by `scan_format` or by parsing) and leaves it untouched; when it has a lower one, copies it atomically to `<file>.format<N>.bak` (`backup_path`) unless that exists; when it does not load as a document of this kind (envelope or body), moves it to `<file>.unreadable.bak` or `<file>.unreadable-N.bak` (`unreadable_path`); then writes. Rendering is deterministic, so unchanged documents give identical files. A kind may set a lower limit (`Document::MAX_SIZE`, at most `MAX_FILE`; themes: 64 KiB), used for reading and writing it. A rendered document over its limit is refused (`Invalid`) before anything is written.
- `write_atomic`: creates the parent directory, writes a temporary file of its own (`<file>.<pid>-<n>.tmp`, created new), flushes it to disk, renames it over the file (on Windows retried up to ten times, 50 ms apart, while another program holds the file) and on Unix flushes the directory; on failure the temporary file is removed.

### Libraries (`library`)

`Library<T: Document>::new(dir)`; the directory is created by the first write.

- **Ids** (`EntryId`): the file name without `.toml`: lowercase ASCII letters, digits and `-`, 1 to 64 bytes, not starting or ending with `-`, not a Windows device name (`con`, `prn`, `aux`, `nul`, `com0`-`com9`, `lpt0`-`lpt9`). `EntryId::from_name` folds common accented Latin letters (`ç` to `c`, `ı` to `i`, ...), lowercases, turns every other run into one `-`, keeps at most 56 bytes and adds `-1` to a device name; `entry` if nothing is left. A new entry gets that id, or the id with `-2`, `-3`, ... when it was ever used: by a file in the library (readable or not) by a backup beside it (`<id>.toml.format1.bak`, `<id>.toml.unreadable.bak`), or by a file in `deleted/` (its stem). Ids never change and are never given to a second entry.
- **Names**: trimmed; 1 to `MAX_NAME` (64) characters; no control characters or text direction marks (U+200E, U+200F, U+202A-U+202E, U+2066-U+2069) (`BadName`); unique in the library, compared after lowercasing, without Unicode normalisation (`NameTaken`).
- `list()`: entries directly in the directory whose name is a valid id plus `.toml`, sorted by name ignoring case, from their envelopes (`peek`); files that cannot be read, including files of a newer format, and `.toml` files whose name is not an id are in `problems` with the reason, untouched, and their ids stay taken.
- `load(id)`, `find(name)`, `create(name, document) -> id`, `save(id, entry)` (existing entries only, `NotFound` otherwise; the name check runs only when the name changes; written under the `write` rules, so a newer entry is refused and a broken one moved aside), `rename(id, name)` (id unchanged), `duplicate(id, name) -> id`. `Entry::new(name, document)`.
- `delete(id)`: moves the file to `deleted/<id>.toml` (or `<id>-N.toml` when taken) and returns the new path. Nothing is removed.
- `export_with(id, path, scrub)`: writes the entry, after `scrub` changed it (removing what belongs to this computer only), in the current format, to `path` under the `write` rules; a path inside the library directory is refused (`Invalid`). `export(id, path)` scrubs nothing.
- `import(path) -> (id, name)`: reads with upgrades; refuses newer formats and other kinds; names the entry after the file's `name`, else the file stem, else `Imported`, shortened to 64 characters; a clashing name becomes `Name 2`, `Name 3`, ..., shortened so it stays within 64 characters.

### Writer lock

Every library write (`create`, `save`, `rename`, `duplicate`, `delete`, `import`) and `set_current` takes `&WriterLock` as proof. `UserData::try_lock() -> Result<Option<WriterLock>, DataError>` locks `user-data.lock` in the config directory (`Ok(None)` when another program holds it). Only the holder writes libraries and `current.toml`; a second app instance works read-only; `rbs-cli` only reads. The holder sends every write through one writer thread, in order (ADR-0051 decision 8); the library itself does not order concurrent calls.

### Output profile (`OutputProfile`, `kind = "output-profile"`, format 1)

The body is a set of sections, each a TOML table; section names are lowercase ASCII letters, digits, `-`, `_` and `.`, and not `format`, `kind` or `name`. `get::<S>(section)` deserialises one (`Ok(None)` if absent, `Invalid` if it does not fit), `set(section, &value)` replaces one (must serialise to a table; refused with `Newer` when the section present has a higher integer `format_version`, `SECTION_VERSION`, than the new value), `remove(section)`. Unknown sections are kept on save. A section that does not load is left alone by the app until the user resets it. A profile never holds a secret.

### Scene collection document (`kind = "scene-collection"`, format 5)

`rbs_scene::SceneCollection` implements `Document`. Body:

| Key | Type | Notes |
| --- | --- | --- |
| `next` | table `canvas`, `source`, `scene`, `item`, `filter` (integers, default 0) | Next ids (`SceneCollection::id_counters`) |
| `canvases` | array of tables `id`, `name`, `width`, `height`, `frame_rate = [num, den]`, `program?`, `preview?`, `globals?`, `master?` | `program`, `preview` and `globals` (format 5, ADR-0065: the canvas's global scene) are scene ids; `master = { volume?, muted? }` (format 7, ADR-0081) is written only when it is not the default (`volume` 1, not muted); a `volume` out of `0..=1` refuses the document |
| `sources` | array of tables `id`, `name`, `type`, `settings?`, `filters?`, `volume?` (1), `muted?` (false), `sync_offset_ms?` (0), `mixer_hidden?` (false) | `type` is the node type id; `volume` is a linear factor in `0..=1` (format 3, ADR-0059); `sync_offset_ms` an integer in `-200..=200` (format 4, ADR-0062); `mixer_hidden = true` keeps the source out of the Mixer, still heard (format 6, ADR-0080); out of range refuses the document, as an out-of-range `volume` does |
| `sources.filters` | array of tables `id`, `type`, `enabled?` (default true), `settings?` | In order; built-in types keep numbers as floats and colours as `"#rrggbb"` / `"#rrggbbaa"` strings ([video-filters.md](video-filters.md)) |
| `scenes` | array of tables `id`, `name`, `canvas?`, `items?` | `canvas` = scope `Canvas(id)`; absent = `Shared` |
| `scenes.items` | array of tables `id`, `source?` or `scene?` (exactly one), `visible?` (default true), `locked?` (default false, format 3), `layout`, `overrides?` | Bottom first |
| `layout` | table `position = [x, y]`, `size = [w, h]`, `rotation?` (0), `crop? = [left, top, right, bottom]` (0s), `opacity?` (1), `style?` | Canvas pixels; crop in source pixels |
| `layout.style` | table `shape?` (`"rectangle"` default, `"circle"`, `"ellipse"`), `radius?` (0; rectangles only), `border? = { width, color }`, `shadow? = { offset? = [x, y], blur?, color }` | Format 2 (ADR-0053). Box units; colours `"#rrggbb"` or `"#rrggbbaa"` (written as `#rrggbbaa`); a radius on another shape and colours in other forms are refused |
| `overrides` | array of tables `canvas`, `layout` | At most one per canvas |
| `settings` | table of booleans, integers, floats and strings | Other TOML types are refused |

Format 1 to 6 documents load unchanged (format 2 only adds `style`, format 3 only adds `volume`, `muted` and `locked`, format 4 only adds `sync_offset_ms`, format 5 only adds `globals`, format 6 only adds `mixer_hidden`, format 7 only adds the canvas `master`); the app writes format 7, which builds that know only an older format refuse without touching the file (ADR-0053 decision 11, ADR-0059, ADR-0062, ADR-0065, ADR-0080, ADR-0081). Defaults are left out when written. Floats are written as the shortest decimal that reads back as the same `f32` (`0.1`, not `0.10000000149011612`). Unknown keys are refused. Loading builds the model and calls `SceneCollection::from_parts(canvases, sources, scenes, counters)`, which:

- trims names like edits do;
- refuses two canvases, sources, scenes or items (across all scenes) with one id (`SceneError::DuplicateId`), and filter ids used twice (`DuplicateFilter`);
- refuses canvases larger than `MAX_CANVAS_SIDE` (16384) pixels on a side (`CanvasTooLarge`) or faster than `MAX_FRAME_RATE` (1000) frames per second (`FrameRateTooHigh`), like edits;
- raises each counter above the highest id in use (`IdsExhausted` if an id is `u32::MAX`), so a document with stale counters never makes the collection reuse an id it holds;
- checks every invariant of the [scene model spec](scene-model.md#invariants).

A document that fails any of these is `DataError::Invalid` with the reason. A change that makes the loader refuse data an earlier build accepted needs a new `format` with an `upgrade` step that repairs the data (ADR-0051 decision 6).

## Errors and failure behaviour

`DataError` (non-exhaustive): `Io`, `Syntax`, `NoVersion`, `WrongKind { found, expected }`, `Newer { found, supported }`, `Invalid(reason)`, `BadName(reason)`, `NameTaken(name)`, `NotFound(id)`. Nothing panics. A file that cannot be read is reported and left as it is; the caller decides what to use instead (the app: the defaults, with a note, and it does not overwrite the file).

## Performance requirements

User-paced; never on a hot path. The app saves collections on a writer thread (ADR-0051 decision 8). Measured by `slow_save_cost_of_a_large_collection` (release): 2000 sources and items save in about 45 ms and load in about 25 ms; a library of 20 such collections lists in about 4 ms. Nothing runs while the user does nothing: no watching, no polling (P13).

## Conformance tests

`crates/rbs-user-data/tests`:

- `document.rs`: round trip with format, kind and name; step-by-step upgrades; newer formats, other kinds, missing or bad versions, invalid TOML and bodies refused; writes never replace a newer file (also one this build cannot parse) and back up an older one once; unreadable files moved aside, no temporary files left; the format line scan; the size limit; reserved keys.
- `library.rs`: ids from names (accents, other scripts, length, device names) and id validation; ids never reused after delete; saves over newer and broken entries (also entries broken only in their body, whose backup keeps the id taken); documents too large to read back are never written; export scrub and no export into the library; newer sections not replaced; the writer lock; create, list, load, save, find; unique names ignoring case and name checks; rename keeps the id, duplicate gets a new one; delete moves to `deleted/` and keeps every copy; unreadable and newer files reported, kept and their ids taken; export and import, also into another user's data, numbered names on a clash (within 64 characters), other kinds refused, unnamed files named after the file; profile sections typed with unknown ones kept; `current.toml` round trip and unreadable selection.
- `scenes.rs`: a collection with two canvases, nested and canvas-scoped scenes, filters, every setting type, overrides, crop, rotation, opacity, hidden items and studio mode round-trips exactly, with counters and stable text; the documented example loads; every scene model invariant checked on load (cycles, crop on nested scenes, overrides for canvases that cannot show the scene, scope of nested scenes, duplicate filters, nesting depth, canvas and frame rate bounds, exhausted ids); every state of a 200-step deterministic edit sequence round-trips with its counters; invalid documents (unknown keys, frame rate, empty canvas, items placing nothing, two things or a missing source, non-finite or out-of-range layouts and settings, non-plain settings, bad type ids, blank names, duplicate ids, two overrides for one canvas, scope violations) are refused with a reason; collections live in their own library and profiles are not imported as collections.

`crates/rbs-scene/tests/collection.rs`: `from_parts` rebuilds an equal collection with its counters, raises stale counters and trims names, and refuses duplicate ids and broken invariants.
