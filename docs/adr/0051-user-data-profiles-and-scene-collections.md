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
2. **Versioned documents.** Every file is one TOML document with `format` (integer, starting at 1) and `kind` at the top, and `name` for library entries; RBS writes these first. Reading must check the kind; a higher format than the build knows must be refused, also when the rest of the file cannot be parsed (the format is found by a line scan first), and such a file must never be overwritten; a lower one must be upgraded in memory one step at a time (`Document::upgrade`), and the original copied once (atomically) to `<file>.format<N>.bak` before the first rewrite (the same rule ADR-0049 uses for workspaces). A file that does not load as a document of the kind (broken by hand, also only in its body, or another kind) must be moved to `<file>.unreadable[-N].bak` before it is replaced, never overwritten. Writes must go through a temporary file of their own (unique name, created new) in the same directory, flushed and renamed over the target, the directory flushed on Unix and the rename retried briefly on Windows sharing violations, so a crash leaves the old or the new file. Files over 16 MiB are not read (shared files are untrusted input), and a document that would be larger is never written. Unknown keys are errors within a known format (typos must not be dropped silently); output profiles keep unknown *sections* (decision 5).
3. **Libraries of named entries.** Kinds the user keeps several of are a `Library`: a directory with one file per entry. The file name is the entry's id, made once from the name (lowercase ASCII, accents folded, other characters `-`, a number added on a clash, Windows device names such as `con` never used) and never changed, so renaming changes one line and references by id survive. An id must never be given to a second entry, also after a delete (ids of files in `deleted/` stay taken), so nothing kept by id (a stream key, the current selection) can pass to another entry. Names are trimmed, 1 to 64 characters without control characters or text direction marks, and unique in the library ignoring case (no Unicode normalisation). Operations: list (reads only envelopes; unreadable and newer files and `.toml` files whose name is not an id are reported, kept and their ids stay taken), load, create, save, rename, duplicate, delete, export, import. Delete must move the file to `deleted/` in the library, never remove it. Export writes the entry in the current format to a path outside the library, after an optional scrub of what belongs to this computer only; import upgrades older formats, refuses newer ones and other kinds, shortens over-long names and numbers a clashing name (`Name 2`) instead of failing.
4. **Layout under the config directory.**

   | Path | Holds | Owner |
   | --- | --- | --- |
   | `profiles/<id>.toml` | Output profiles, `kind = "output-profile"` | `rbs-user-data` |
   | `scenes/<id>.toml` | Scene collections, `kind = "scene-collection"` | `rbs-user-data` |
   | `current.toml` | Which profile and collection are in use, `kind = "current"` | `rbs-user-data` |
   | `workspaces/` | Workspaces (ADR-0017, ADR-0049; slug file names stay until a change of their own moves them to a `Library`) | `rbs-ui` |
   | `themes/`, `library/` | Reserved for user themes and the asset library; formats are later decisions | - |
   | `remote-control*.{toml,pem}` | Remote control (per user, not per profile) | `rbs-remote-settings` |

5. **Output profile = named sections.** A profile is a set of sections, each a TOML table owned by the code that uses it (in the app: `recording` and `stream`, each keeping its own `format_version`). Sections the build does not know are kept as they are when the profile is saved. A known section with a higher `format_version` than the build writes, or one that does not load, must be kept and treated as read-only: `set` refuses to replace a section with a higher `format_version`, and the app replaces one only when the user asks to reset it. Section owners must bump `format_version` whenever an older build could not read the section.
   **Secrets and per-computer data.** A document must never hold a secret. Stream keys stay in the operating system's credential store under an account named after the profile id (`stream-key/<id>`; the account used before profiles maps to the `default` profile only, and ids are never reused, decision 3); deleting a profile removes its credential, so a profile restored by hand from `deleted/` comes back without its stream key. Settings that a later settings schema marks secret follow the same rule, keyed by entry id and object id, and are never written to files. Export scrubs what belongs to one computer (for example "a key is saved", recording folders) through a per-kind hook. Text fields cannot be policed completely: the app must warn when a server URL looks like it carries a key (an extra RTMP path segment).
6. **Scene collection document.** The document maps one-to-one onto the model (ADR-0021 decision 10): canvases (id, name, size, frame rate as `[num, den]`, program and preview), sources (id, name, `type`, settings, filters), scenes (id, name, `canvas` for canvas scope, absent for shared, items with `source` or `scene`, `visible`, `layout`, per-canvas `overrides`) and the id counters (`next`). Setting values are TOML booleans, integers, floats and strings. Loading must go through `SceneCollection::from_parts`, which checks unique ids and every invariant of the scene model spec, so a loaded collection is as valid as an edited one; counters below an id in use are raised, never trusted. Canvas sides are at most 16384 pixels and frame rates at most 1000 per second (shared files are untrusted input). Any later change that refuses data an earlier build accepted (a tighter invariant) must come with a new document `format` and an `upgrade` step that repairs such data, so a saved show never becomes unreadable. The format is specified in `docs/specs/user-data.md`.
7. **Canvas format belongs to the collection.** A canvas's size and frame rate are part of the scene collection (they are already in the model, and a collection with a horizontal and a vertical canvas needs both). The app's Video settings edit the current collection's canvas; `video.toml` is no longer used after migration (decision 9).
8. **Using them in the app.** The app keeps one current profile and one current collection, named in `current.toml`. After each committed edit (a drag is one edit, at its end; program and preview changes too) the collection is handed to a writer thread that keeps only the latest snapshot and saves it, so the control thread never waits for the disk and a burst of cuts is one save. Every write to user data (saves, create, rename, duplicate, delete, import, export, `current.toml`) goes through that one writer, in order: a pending snapshot carries the entry's current name, and switching collection, renaming, deleting and closing the app queue behind pending saves (closing waits for them), so no write can land on top of a newer one. A failed save keeps the collection marked unsaved, is shown to the user and retried on the next change and on close. Switching profile or collection is refused while recording or streaming, because outputs are attached to the running canvas (ADR-0047). With no entries, the app creates "Default" of each. Only one program writes user data: the app takes the writer lock (`user-data.lock`, `UserData::try_lock`) at start; a second instance opens user data read-only and says so; `rbs-cli` only reads.
9. **Migration.** Each library migrates on its own, so a crash between the two leaves nothing half-done: when `profiles/` does not exist, the app creates the "Default" profile (id `default`) from `recording.toml` and `stream.toml`; when `scenes/` does not exist, the "Default" collection with one canvas from `video.toml`. The old files are left in place and no longer read. The stream key already saved keeps working for the Default profile.
10. **Zero cost when unused** (P13): nothing watches files or polls; documents are read when the app starts or the user switches, and written when the user changes something.

## Consequences

- Users get OBS-style profiles and scene collections with import and export, and their scenes survive a restart for the first time.
- A newer RBS's files are safe from an older one; an upgrade keeps the original; a crash during a save loses nothing.
- Every later user-made kind (themes, presets, asset library entries) gets versioning, naming, import and export by implementing `Document`; a pack format (`.rbspack`) can bundle library files without a new file format for each kind.
- Stable ids keep references (current selection, stream key entries, later panel ids or pack manifests) working across renames. The cost: a file name no longer shows the current name after a rename.
- The scene document is now a public format: changing it needs a new `format` and an upgrade step, and a spec change.
- Measured (`slow_save_cost_of_a_large_collection`, release, Linux container): a collection of 2000 sources and items (455 KB) saves in about 45 ms, loads in about 25 ms, and a library of 20 such collections lists in about 4 ms (envelopes only). Hence the writer thread in decision 8; nothing of this runs on a hot path.
- Semver: the crates are not published; the public contract is the file format, versioned by `format`. `Document` uses `toml::Table` in its signatures, so document kinds are tied to TOML and the `toml` crate; that is accepted for user-editable application data.
- `deleted/` grows until the user empties it; the app can offer restore and cleanup later.
- Follow-ups: app wiring (profile and collection menus, autosave, migration, switching), `rbs-cli` flags to record or stream a named collection and profile, moving workspaces to a `Library` (own change, ADR-0017 fixed slug names), theme and asset library formats, pack format.

## Alternatives considered

- **A profile as a directory of the current files** (`profiles/<name>/recording.toml`, ...): reuses today's loaders, but a switch or import touches several files that can be half-written, and export needs an archive format. One file per entry makes create, rename, import and export single atomic steps.
- **Slug of the current name as file name** (as workspaces do): readable, but a rename moves the file and breaks references by name (stream key entries, current selection); stable ids avoid that.
- **Opaque ids (UUID, ULID or a counter)**: never clash across computers and need no reserved-name rules, but file names say nothing in a file manager or a bug report, and RBS has no random source in this layer. Ids made once from the name, never reused and checked against device names keep files recognisable; a pack format can rename on import.
- **serde derives inside `rbs-scene`**: one set of types, but puts a file format into a core crate (P1) and ties the public format to internal struct layout. A separate document mapping keeps the model free to change.
- **JSON for scene collections**: no ambiguity for large nested data and easy for web tools, but every other user file is TOML and users edit these by hand; TOML maps the same values.
- **SQLite or a single settings database**: transactions for free, but opaque to users, harder to share and diff, and a native dependency for what a few small files do.
- **Silently resetting unreadable files to defaults** (today's behaviour): simple, but the next save destroys the user's file; reporting and leaving the file is safer.

## Review

Independent AI session per ADR-0014, 2026-11-06 (given only the proposal, specs, principles and code). Verdict: changes requested, 15 concerns; all addressed in the same change:

1. Blocking: writes overwrote files that did not parse (a newer RBS's syntax, a hand edit). The format is now found by a line scan first, so newer files are refused even when unparseable; any other unreadable file is moved to `<file>.unreadable[-N].bak` before it is replaced (decision 2). Tests `a_newer_file_this_build_cannot_parse_is_still_never_replaced`, `an_unreadable_file_is_moved_aside_before_it_is_replaced`, and the library save cases.
2. Blocking: ids were reused after delete, handing a stream key to a new profile. Ids in `deleted/` stay taken; the credential account rule, the legacy key mapping and removal on delete are in decision 5. Test `delete_moves_the_file_aside`.
3. Known sections written by a newer build were reset and overwritten: `set` refuses a lower `format_version`; decision 5 makes such sections read-only until the user resets them. Test `a_section_from_a_newer_build_is_not_replaced`.
4. Windows device names and silently ignored `.toml` files: device names get `-1` and are not valid ids; such files are reported. Tests in `library.rs`.
5. Weak atomic write: unique temporary names created new, directory flush on Unix, Windows retry (decision 2). Sharing one implementation with `rbs-remote-settings` and `rbs-ui` is a follow-up (those crates are out of scope here).
6. Validation not tied to the format, no bounds, no size cap: the tightening rule, canvas and frame rate bounds (`MAX_CANVAS_SIDE`, `MAX_FRAME_RATE` in `rbs-scene`) and the 16 MiB cap (decisions 2, 6).
7. Autosave cost: saves keep the name check only when the name changes, listing reads envelopes only, the writer thread coalesces saves (decision 8), and a measurement is recorded above.
8. Two instances and failed saves: writer lock, read-only second instance, failed-save behaviour (decision 8). Test `only_one_program_holds_the_writer_lock`.
9. Secrets and per-computer data in exports: `Library::export_with` scrub hook, the rule for secret settings and the URL warning (decision 5). Test `export_can_scrub_and_never_writes_into_the_library`.
10. Partial backup after a crash: the backup is written atomically from the bytes read.
11. Tests: added the cases above, every scene model invariant through the loader, and a 200-step deterministic edit sequence that renders and parses every state and compares content and counters.
12. API evolution: `IdCounters` is a plain struct (adding a counter is meant to break callers); `Read`, `Entry`, `Listed`, `Listing` are `#[non_exhaustive]` with `Entry::new`; semver stated in Consequences. `DuplicateId` keeps the `canvas#3` text form, which names kind and id.
13. Names: over-long imported names are shortened, text direction marks refused, no Unicode normalisation (stated in decision 3).
14. Migration crash safety: each library migrates on its own (decision 9).
15. Export into the library or over files: refused inside the library; other targets follow the write rules of decision 2.

Re-review by the same session, 2026-11-06: changes requested; concerns 2-7, 9, 10 and 12-15 confirmed fixed, 5's shared implementation accepted as a follow-up. New concerns and resolutions:

1. Blocking: a file with a valid envelope but a broken body was still overwritten. `write` now loads the whole existing document and moves it aside on any error but `Newer`. Test `a_hand_edit_that_breaks_only_the_body_is_kept_when_saved_over`.
2. Major: a writer thread for autosave raced with direct rename and delete. Decision 8 now routes every write through the one writer, in order.
3. Minor: a document too large to read back could be written. `render` refuses it. Test `a_document_too_large_to_read_back_is_never_written`.
4. Minor: the writer lock is advisory; library writes do not require it. Accepted: the app is the only program that links write paths, and `rbs-cli` gets read-only commands; enforcing it in the type system can come with the app wiring if a second writer appears.
5. Minor: moving a broken file aside could free its id. Backups (`<id>.toml.*.bak`) keep their id taken (same test as 1).
6. Nit: a hand-restored profile has no stream key. Stated in decision 5.
