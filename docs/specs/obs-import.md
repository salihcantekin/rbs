# Spec: OBS import

- Status: Draft (ADR-0131 Proposed; nothing implemented)
- Owner crate(s): `rbs-import-obs` (planned), `rbs-app`, `rbs-cli`
- Related ADRs: [0131](../adr/0131-import-obs-scene-collections-and-profiles.md), [0051](../adr/0051-user-data-profiles-and-scene-collections.md), [0065](../adr/0065-global-sources-and-safe-source-removal.md), [0077](../adr/0077-source-health-retarget-and-settings-window.md), [0103](../adr/0103-source-region.md), [0113](../adr/0113-experience-level-and-first-run.md)

## Purpose

Turn an OBS Studio scene collection, profile and hotkeys into a new RBS scene collection, output profile and shortcuts, and report every OBS object that did not come across exactly.

## Scope

In scope: reading OBS files, the import plan, the mapper registry, the transform conversion, the report, the entry points (`rbs-cli`, desktop, control API).

Out of scope: writing OBS files; importing from other apps (Streamlabs, XSplit); loading OBS plugins; adding skipped objects later (ADR-0131 decision 11 keeps allow-listed data for it).

## Contract

```rust
// rbs_import_obs
pub fn default_config_dirs() -> Vec<PathBuf>;                 // existing OBS folders on this OS
pub fn scan(config_dir: &Path) -> Result<ObsInstall, ImportError>; // collections and profiles by name
pub struct ObsInstall { collections: Vec<Found>, profiles: Vec<Found> }
pub struct Found { name: String, path: PathBuf }

pub trait Catalogue {                                          // implemented by rbs-app
    fn has(&self, type_id: &NodeTypeId) -> bool;               // sources, filters, transitions
    fn fields(&self, type_id: &NodeTypeId) -> Option<&[rbs_params::Field]>; // ranges used to clamp
}
pub trait Lookup {
    fn picture_size(&self, path: &Path) -> Option<(u32, u32)>;
    fn file_exists(&self, path: &Path) -> bool;
    fn targets(&self, type_id: &NodeTypeId) -> Vec<LookupTarget>; // asked at most once per type per import
}
pub struct LookupTarget { target: String, name: String, app: Option<String>, size: Option<(u32, u32)> }

pub struct Options { stream_key: bool, shortcuts: bool }      // both default true

pub fn plan(collection_json: &[u8], profile: Option<&ProfileFiles>, mappers: &Mappers,
            catalogue: &dyn Catalogue, lookup: &dyn Lookup, options: &Options) -> Result<ImportPlan, ImportError>;
pub struct ProfileFiles { basic_ini: Vec<u8>, service_json: Option<Vec<u8>>,
                          stream_encoder: Option<Vec<u8>>, record_encoder: Option<Vec<u8>> }

pub struct ImportPlan {
    pub name: String,                          // the OBS collection name
    pub collection: SceneCollection,           // passed SceneCollection::from_parts
    pub profile: Option<ObsProfileFacts>,     // neutral facts; rbs-app maps them
    pub secrets: ImportedSecrets,              // Debug prints <redacted>; no Serialize, no Display
    pub shortcuts: Vec<ImportedShortcut>,      // { command, args, chord: Chord }; app scope
                                               // args may hold CollectionArg::ThisImport, which rbs-app
                                               // replaces with the new collection's id before writing
    pub report: ImportReport,
}

pub struct Mappers;                            // builtin(), register_source/filter/transition
pub trait SourceMapper { fn obs_id(&self) -> &'static str;          // one mapper per OBS id
                         fn rbs_types(&self) -> &'static [&'static str]; // all must be in the Catalogue
                         fn map(&self, src: &ObsSource, ctx: &mut MapContext) -> SourceOutcome; }
pub trait FilterMapper { /* same shape, FilterOutcome */ }
pub trait TransitionMapper { /* same shape, TransitionOutcome */ }
```

`ImportError`: `TooLarge`, `NotJson`, `NotACollection` (no `sources` array), `Internal(String)` (the built collection failed `from_parts` although the importer enforces its invariants first: a bug). A single bad source, filter or item, a name clash or a scene past `MAX_LAYERS` is never an error: it is a report entry.

### Report

```rust
pub struct ImportReport { pub exact: Counts, pub entries: Vec<ReportEntry> }
pub struct ReportEntry { outcome: Outcome /* Approximated | Skipped */, area: Area,
                         obs_name: String, obs_type: String, scenes: Vec<String>,
                         reason: String, hint: Option<String> }
```

Reasons are one sentence of plain English ("Only PNG pictures can be shown for now."). No stream key, file content or full path appears in a report; a path is shown by its file name.

`rbs-app` saves the report as `imports/<collection id>.report.toml` (`kind = "obs-import-report"`, format 1), deleted with its collection and not exported with it. Skipped objects keep only allow-listed keys under `[[skipped]]` (ADR-0131 decision 11): URLs without query or fragment, no key containing `token`, `key`, `password`, `secret`, `auth` or `cookie`, paths cut to the file name, no opaque plugin state. The allow-list per OBS type is added here with the mapper that will use it.

### Transform conversion

ADR-0131 decision 8 is normative: absolute `pos`, `scale`, `bounds` are read; an item with only OBS 31 relative keys is reported until their conversion is checked against a real OBS 31 file; `bounds_crop`; crop filter as region plus item crop. OBS places an item at `pos` with the anchor `align` (bits: left 1, right 2, top 4, bottom 8; none is centre), size `native × scale` or, with a bounds type, the bounds box (stretch, scale inner, scale outer, to width, to height, max only) aligned by `bounds_align`, rotated by `rot` degrees about `pos`, after `crop_*`. RBS stores the box (`Layout.position`, `Layout.size`, negative for a flip) rotated about its centre. The converter computes the four corners of the OBS item and returns the RBS box with the same corners in texture order (within 0.01 px). The native size comes from `Lookup` or settings; when unknown, the canvas size is used and the item is marked approximate.

### Mapping table

ADR-0131 decision 7 is the first mapping table. Each mapper keeps its own row in this section when it is implemented.

### Degrading on scene validation (ADR-0131 decision 4)

| `SceneError` | Action |
| --- | --- |
| `DuplicateName` | "Name 2", "Name 3", ... |
| `EmptyName` | the OBS type's readable name plus a number |
| `TooManyLayers`, `NestingTooDeep`, `NestingCycle` | item left out, Skipped |
| `CropOnScene`, `StyleOnScene` | crop or style dropped, item kept, Approximated |
| `InvalidLayout`, `InvalidVolume`, `InvalidSyncOffset`, `InvalidSetting` | value clamped or the key dropped, Approximated |
| `CanvasTooLarge`, `EmptyCanvas`, `FrameRateTooHigh` (also a zero rate or `FPSDen` 0) | 1920×1080 at 30, Profile entry |
| `ScopeViolation`, `GlobalScene` | cannot arise: user scenes are shared; the global scene is scoped to its canvas and made only by the importer |
| `UnknownCanvas`, `UnknownSource`, `UnknownScene`, `UnknownItem`, `UnknownFilter`, `IndexOutOfRange`, `CanvasHasScenes`, `UndoConflict` | cannot arise: edit-time errors, not raised by `from_parts` |
| `DuplicateId`, `DuplicateFilter`, `IdsExhausted` | cannot arise: the importer issues ids from one counter per kind |
| `BadTypeId` | cannot arise: mapper type ids are checked by the `rbs-app` catalogue test |
| `InvalidFollow`, `SharedLevels` | cannot arise: one canvas, no follow, no scene levels imported |
| `InvalidGlide`, `InvalidShowFor` | cannot arise: default glide, no timed show imported |
| `ChromeNotDisplay` | cannot arise: chrome is never imported |

A unit test matches every `SceneError` variant against this table, so a new variant fails until it has an action. 

## Errors and failure behaviour

A failed plan writes nothing. A plan is written in order: collection, profile, credentials, shortcuts (ADR-0131 decision 9), each atomic; a failed later step leaves the earlier ones and is named in the report. Names clash-free by the free-name helper ("Name 2"). Unknown keys are ignored; a missing key takes OBS's default.

## Performance requirements

Runs on the control thread on demand only. A 200-source collection plans in under 100 ms on the reference machines, excluding `Lookup` enumeration. No threads, timers or resources while no import runs.

## Conformance tests

`crates/rbs-import-obs/tests`: golden fixtures (hand-written for this repository) for each mapper, groups, nested scenes, global audio devices, cross-platform ids, unsupported types; the property test of the transform conversion; a report contains no stream key; `from_parts` accepts every fixture's collection.

## Open questions

- Converting non-PNG images (needs a decoder; part of the media work).
- Mapping the Aitum vertical canvas plugin's data to the RBS vertical canvas (ADR-0093).
- Saved projectors to projector windows (ADR-0125).
