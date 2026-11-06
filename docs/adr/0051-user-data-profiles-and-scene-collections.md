# ADR-0051: User data as versioned documents; output profiles and scene collections as libraries of named files

- Status: Proposed
- Date: 2026-11-06
- Deciders: Salih (owner)

## Context

The owner wants users to save, switch, import and export their settings and scenes like OBS profiles and scene collections, and later themes, workspaces, an asset library and shareable packs, so that "almost everything can be edited in many ways" and the community can share it.

Today:

- The scene collection (ADR-0021) exists only in memory. `rbs-app` builds one canvas and one scene at start; nothing is saved. The scene model spec leaves "the on-disk document format and its versioning" open and asks for "a validating constructor from parts, including the id counters".
- The app keeps `video.toml` (canvas size and rate), `recording.toml` and `stream.toml` directly in the config directory, one set per user. Each has its own `format_version` and its own load and save code; a newer file is reset to defaults on load and then overwritten on the next save.
- Workspaces (ADR-0017, ADR-0049) are TOML files in `workspaces/`, named by a slug of the workspace name, owned by `rbs-ui`. Remote control settings live in `remote-control.toml` (ADR-0044).
- The glossary separates the terms: a *Scene collection* holds scenes and sources, no layout and no output settings; an *Output profile* holds output settings, no scenes and no layout; a *Workspace* holds neither.

The forces: files are a public format (ADR-0014) that users edit, share and keep across RBS versions; a crash or a newer RBS must never destroy a user's work; ids that panels and the control API hold (canvas ids in panel ids, ADR-0049) must survive a reload; P1 keeps core crates free of file formats; P13 wants nothing to run while the user does nothing.

## Decision

1. **One user data layer.** A new application-layer crate, `rbs-user-data`, owns the files the user makes and keeps under the config directory. It depends on `rbs-scene`, `rbs-core`, `rbs-graph`, `serde` and `toml`; never on the engine, adapters or a toolkit. Only composition roots (`rbs-app`, `rbs-cli`) use it. `rbs-scene` gets no serialisation dependency: it gains `SceneCollection::from_parts` (the validating loader) and `id_counters`, and constructors for its model types.
2. **Versioned documents.** Every file is one TOML document with `format` (integer, starting at 1) and `kind` at the top, and `name` for library entries. Reading must check the kind; a higher format than the build knows must be refused and the file must never be overwritten; a lower one must be upgraded in memory one step at a time (`Document::upgrade`), and the original copied once to `<file>.format<N>.bak` before the first rewrite (the same rule ADR-0049 uses for workspaces). Writes must go through a temporary file in the same directory, flushed and renamed over the target, so a crash leaves the old or the new file. Unknown keys are errors within a known format (typos must not be dropped silently); output profiles keep unknown *sections* (decision 5).
3. **Libraries of named entries.** Kinds the user keeps several of are a `Library`: a directory with one file per entry. The file name is the entry's id, made once from the name (lowercase ASCII, accents folded, other characters `-`, a number added on a clash) and never changed, so renaming changes one line and references by id survive. Names are trimmed, 1 to 64 characters without control characters, and unique in the library ignoring case. Operations: list (unreadable and newer files are reported, kept and their ids stay taken), load, create, save, rename, duplicate, delete, export, import. Delete must move the file to `deleted/` in the library, never remove it. Export writes the entry in the current format to any path; import upgrades older formats, refuses newer ones and other kinds, and numbers a clashing name (`Name 2`) instead of failing.
4. **Layout under the config directory.**

   | Path | Holds | Owner |
   | --- | --- | --- |
   | `profiles/<id>.toml` | Output profiles, `kind = "output-profile"` | `rbs-user-data` |
   | `scenes/<id>.toml` | Scene collections, `kind = "scene-collection"` | `rbs-user-data` |
   | `current.toml` | Which profile and collection are in use, `kind = "current"` | `rbs-user-data` |
   | `workspaces/` | Workspaces (ADR-0017, ADR-0049; slug file names stay until a change of their own moves them to a `Library`) | `rbs-ui` |
   | `themes/`, `library/` | Reserved for user themes and the asset library; formats are later decisions | - |
   | `remote-control*.{toml,pem}` | Remote control (per user, not per profile) | `rbs-remote-settings` |

5. **Output profile = named sections.** A profile is a set of sections, each a TOML table owned by the code that uses it (in the app: `recording` and `stream`, each keeping its own `format_version`). Sections the build does not know are kept as they are when the profile is saved, so a newer app or a plugin loses nothing. A profile must never hold a secret: stream keys stay in the operating system's credential store, one entry per profile id, so any profile file can be exported and shared.
6. **Scene collection document.** The document maps one-to-one onto the model (ADR-0021 decision 10): canvases (id, name, size, frame rate as `[num, den]`, program and preview), sources (id, name, `type`, settings, filters), scenes (id, name, `canvas` for canvas scope, absent for shared, items with `source` or `scene`, `visible`, `layout`, per-canvas `overrides`) and the id counters (`next`). Setting values are TOML booleans, integers, floats and strings. Loading must go through `SceneCollection::from_parts`, which checks unique ids and every invariant of the scene model spec, so a loaded collection is as valid as an edited one; counters below an id in use are raised, never trusted. The format is specified in `docs/specs/user-data.md`.
7. **Canvas format belongs to the collection.** A canvas's size and frame rate are part of the scene collection (they are already in the model, and a collection with a horizontal and a vertical canvas needs both). The app's Video settings edit the current collection's canvas; `video.toml` is no longer used after migration (decision 9).
8. **Using them in the app.** The app keeps one current profile and one current collection, named in `current.toml`. A collection is saved after each committed edit (a drag is one save, at its end), on the control thread, off the hot paths. Switching profile or collection is refused while recording or streaming, because outputs are attached to the running canvas (ADR-0047). With no entries, the app creates "Default" of each. `rbs-cli` may read profiles and collections by name; the app is the only writer for now, so no file lock is needed (unlike `remote-control.toml`, which both write).
9. **Migration.** When `profiles/` does not exist, the app creates the "Default" profile from `recording.toml` and `stream.toml`, and the "Default" collection with one canvas from `video.toml`; the old files are left in place and no longer read. The stream key already saved keeps working for the Default profile.
10. **Zero cost when unused** (P13): nothing watches files or polls; documents are read when the app starts or the user switches, and written when the user changes something.

## Consequences

- Users get OBS-style profiles and scene collections with import and export, and their scenes survive a restart for the first time.
- A newer RBS's files are safe from an older one; an upgrade keeps the original; a crash during a save loses nothing.
- Every later user-made kind (themes, presets, asset library entries) gets versioning, naming, import and export by implementing `Document`; a pack format (`.rbspack`) can bundle library files without a new file format for each kind.
- Stable ids keep references (current selection, stream key entries, later panel ids or pack manifests) working across renames. The cost: a file name no longer shows the current name after a rename.
- The scene document is now a public format: changing it needs a new `format` and an upgrade step, and a spec change.
- Every edit writes the collection file (milliseconds for hundreds of objects). If collections grow large, saves can move to a background thread or be coalesced without changing the format.
- `deleted/` grows until the user empties it; the app can offer restore and cleanup later.
- Follow-ups: app wiring (profile and collection menus, autosave, migration, switching), `rbs-cli` flags to record or stream a named collection and profile, moving workspaces to a `Library` (own change, ADR-0017 fixed slug names), theme and asset library formats, pack format.

## Alternatives considered

- **A profile as a directory of the current files** (`profiles/<name>/recording.toml`, ...): reuses today's loaders, but a switch or import touches several files that can be half-written, and export needs an archive format. One file per entry makes create, rename, import and export single atomic steps.
- **Slug of the current name as file name** (as workspaces do): readable, but a rename moves the file and breaks references by name (stream key entries, current selection); stable ids avoid that.
- **serde derives inside `rbs-scene`**: one set of types, but puts a file format into a core crate (P1) and ties the public format to internal struct layout. A separate document mapping keeps the model free to change.
- **JSON for scene collections**: no ambiguity for large nested data and easy for web tools, but every other user file is TOML and users edit these by hand; TOML maps the same values.
- **SQLite or a single settings database**: transactions for free, but opaque to users, harder to share and diff, and a native dependency for what a few small files do.
- **Silently resetting unreadable files to defaults** (today's behaviour): simple, but the next save destroys the user's file; reporting and leaving the file is safer.

## Review

Independent review per ADR-0014 pending.
