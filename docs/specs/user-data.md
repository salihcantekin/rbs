# Spec: User data

- Status: Draft
- Owner crate(s): `rbs-user-data`; `rbs-scene` (`SceneCollection::from_parts`)
- Related ADRs: [0014](../adr/0014-review-policy.md), [0017](../adr/0017-flexible-panels-and-workspaces.md), [0021](../adr/0021-scene-collection-model.md), [0049](../adr/0049-main-window-areas-panel-states-and-sizes.md), [0051](../adr/0051-user-data-profiles-and-scene-collections.md)

## Purpose

What users make and keep (output profiles, scene collections, and later themes, presets and library assets) is stored as versioned TOML documents under the config directory, in libraries of named entries that can be created, renamed, duplicated, deleted, imported and exported. This spec defines the document envelope, the library rules, the directory layout, the output profile and the scene collection document.

## Scope

In scope: document versioning and safe writes; libraries; `current.toml`; output profiles as sections; the scene collection document format.

Out of scope, later changes: workspaces (ADR-0017 and ADR-0049, owned by `rbs-ui`, until a change of their own moves them onto a library); themes and the asset library (directories reserved); packs (`.rbspack`); the app's menus and settings windows (UI design system spec); how the app's recording and stream sections look (they keep their own `format_version` and are documented with the app).

## Contract

### Layout (`UserData`)

`UserData::new(config)` with the config directory (`rbs_remote_settings::config_dir()`):

| Path | Contents |
| --- | --- |
| `profiles/<id>.toml` | Output profiles (`UserData::profiles()`) |
| `scenes/<id>.toml` | Scene collections (`UserData::scene_collections()`) |
| `current.toml` | `Current { profile, scene_collection }`: ids of the entries in use (`current()`, `set_current()`) |
| `workspaces/`, `themes/`, `library/` | Reserved (`WORKSPACES_DIR`, `THEMES_DIR`, `LIBRARY_DIR`) |

`current()` returns an unset selection and the reason when `current.toml` cannot be read; an id that names no entry is treated as unset by the caller.

### Documents (`document`)

A kind implements `Document`: `KIND`, `FORMAT` (current version, from 1), `from_body`, `to_body` and optionally `upgrade(from, body)` (one step, `from` to `from + 1`). The file is a TOML table:

| Key | Meaning |
| --- | --- |
| `format` | Integer, at least 1 |
| `kind` | `Document::KIND` |
| `name` | Display name (library entries) |
| anything else | The body |

- `parse` / `read`: a file without an integer `format` of at least 1 or without `kind` is `NoVersion`; another kind is `WrongKind`; a higher format is `Newer`; a lower one is upgraded step by step before `from_body`. `Read` reports the name, the format found and the document. Invalid TOML is `Syntax`; a body that does not fit is `Invalid` with the reason.
- `render` / `write`: writes `format = FORMAT`, `kind`, `name` if given, then the body; a body using a reserved key is `Invalid`. `write` refuses with `Newer` when the file on disk has a higher format and leaves it untouched; when it has a lower one, copies it to `<file>.format<N>.bak` (`backup_path`) unless that exists, then writes. Rendering is deterministic, so unchanged documents give identical files.
- `write_atomic`: creates the parent directory, writes `<file>.tmp`, flushes it to disk and renames it over the file; on failure the temporary file is removed.

### Libraries (`library`)

`Library<T: Document>::new(dir)`; the directory is created by the first write.

- **Ids** (`EntryId`): the file name without `.toml`: lowercase ASCII letters, digits and `-`, 1 to 64 bytes, not starting or ending with `-`. `EntryId::from_name` folds common accented Latin letters (`ç` to `c`, `ı` to `i`, ...), lowercases, turns every other run into one `-` and keeps at most 56 bytes; `entry` if nothing is left. A new entry gets that id, or the id with `-2`, `-3`, ... when a file with it exists. Ids never change.
- **Names**: trimmed; 1 to `MAX_NAME` (64) characters; no control characters (`BadName`); unique in the library, compared after lowercasing (`NameTaken`).
- `list()`: entries directly in the directory whose name is a valid id plus `.toml`, sorted by name ignoring case; files that cannot be read, including files of a newer format, are in `problems` with the reason, untouched, and their ids stay taken.
- `load(id)`, `find(name)`, `create(name, document) -> id`, `save(id, entry)` (existing entries only, `NotFound` otherwise), `rename(id, name)` (id unchanged), `duplicate(id, name) -> id`.
- `delete(id)`: moves the file to `deleted/<id>.toml` (or `<id>-N.toml` when taken) and returns the new path. Nothing is removed.
- `export(id, path)`: writes the entry, current format, to `path` (atomically).
- `import(path) -> (id, name)`: reads with upgrades; refuses newer formats and other kinds; names the entry after the file's `name`, else the file stem, else `Imported`; a clashing name becomes `Name 2`, `Name 3`, ..., shortened so it stays within 64 characters.

The app is the only writer of libraries and `current.toml` (no lock); other programs only read.

### Output profile (`OutputProfile`, `kind = "output-profile"`, format 1)

The body is a set of sections, each a TOML table; section names are lowercase ASCII letters, digits, `-`, `_` and `.`, and not `format`, `kind` or `name`. `get::<S>(section)` deserialises one (`Ok(None)` if absent, `Invalid` if it does not fit), `set(section, &value)` replaces one (must serialise to a table), `remove(section)`. Unknown sections are kept on save. A profile never holds a secret.

### Scene collection document (`kind = "scene-collection"`, format 1)

`rbs_scene::SceneCollection` implements `Document`. Body:

| Key | Type | Notes |
| --- | --- | --- |
| `next` | table `canvas`, `source`, `scene`, `item`, `filter` (integers, default 0) | Next ids (`SceneCollection::id_counters`) |
| `canvases` | array of tables `id`, `name`, `width`, `height`, `frame_rate = [num, den]`, `program?`, `preview?` | `program` and `preview` are scene ids |
| `sources` | array of tables `id`, `name`, `type`, `settings?`, `filters?` | `type` is the node type id |
| `sources.filters` | array of tables `id`, `type`, `enabled?` (default true), `settings?` | In order |
| `scenes` | array of tables `id`, `name`, `canvas?`, `items?` | `canvas` = scope `Canvas(id)`; absent = `Shared` |
| `scenes.items` | array of tables `id`, `source?` or `scene?` (exactly one), `visible?` (default true), `layout`, `overrides?` | Bottom first |
| `layout` | table `position = [x, y]`, `size = [w, h]`, `rotation?` (0), `crop? = [left, top, right, bottom]` (0s), `opacity?` (1) | Canvas pixels; crop in source pixels |
| `overrides` | array of tables `canvas`, `layout` | At most one per canvas |
| `settings` | table of booleans, integers, floats and strings | Other TOML types are refused |

Defaults are left out when written. Floats are written as the shortest decimal that reads back as the same `f32` (`0.1`, not `0.10000000149011612`). Unknown keys are refused. Loading builds the model and calls `SceneCollection::from_parts(canvases, sources, scenes, counters)`, which:

- trims names like edits do;
- refuses two canvases, sources, scenes or items (across all scenes) with one id (`SceneError::DuplicateId`), and filter ids used twice (`DuplicateFilter`);
- raises each counter above the highest id in use (`IdsExhausted` if an id is `u32::MAX`), so a document with stale counters never makes the collection reuse an id it holds;
- checks every invariant of the [scene model spec](scene-model.md#invariants).

A document that fails any of these is `DataError::Invalid` with the reason.

## Errors and failure behaviour

`DataError` (non-exhaustive): `Io`, `Syntax`, `NoVersion`, `WrongKind { found, expected }`, `Newer { found, supported }`, `Invalid(reason)`, `BadName(reason)`, `NameTaken(name)`, `NotFound(id)`. Nothing panics. A file that cannot be read is reported and left as it is; the caller decides what to use instead (the app: the defaults, with a note, and it does not overwrite the file).

## Performance requirements

Control thread only, user-paced; never on a hot path. Listing a library parses each entry (milliseconds for tens of entries). Nothing runs while the user does nothing: no watching, no polling (P13).

## Conformance tests

`crates/rbs-user-data/tests`:

- `document.rs`: round trip with format, kind and name; step-by-step upgrades; newer formats, other kinds, missing or bad versions, invalid TOML and bodies refused; writes never replace a newer file and back up an older one once; reserved keys.
- `library.rs`: ids from names (accents, other scripts, length) and id validation; create, list, load, save, find; unique names ignoring case and name checks; rename keeps the id, duplicate gets a new one; delete moves to `deleted/` and keeps every copy; unreadable and newer files reported, kept and their ids taken; export and import, also into another user's data, numbered names on a clash (within 64 characters), other kinds refused, unnamed files named after the file; profile sections typed with unknown ones kept; `current.toml` round trip and unreadable selection.
- `scenes.rs`: a collection with two canvases, nested and canvas-scoped scenes, filters, every setting type, overrides, crop, rotation, opacity, hidden items and studio mode round-trips exactly, with counters and stable text; the documented example loads; invalid documents (unknown keys, frame rate, empty canvas, items placing nothing, two things or a missing source, non-finite or out-of-range layouts and settings, non-plain settings, bad type ids, blank names, duplicate ids, two overrides for one canvas, scope violations) are refused with a reason; collections live in their own library and profiles are not imported as collections.

`crates/rbs-scene/tests/collection.rs`: `from_parts` rebuilds an equal collection with its counters, raises stale counters and trims names, and refuses duplicate ids and broken invariants.
