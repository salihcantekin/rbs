# Spec: OBS import

- Status: Draft (ADR-0131 Proposed; nothing implemented)
- Owner crate(s): `rbs-import-obs` (planned), `rbs-app`, `rbs-cli`
- Related ADRs: [0131](../adr/0131-import-obs-scene-collections-and-profiles.md), [0051](../adr/0051-user-data-profiles-and-scene-collections.md), [0065](../adr/0065-global-sources-and-safe-source-removal.md), [0077](../adr/0077-source-health-retarget-and-settings-window.md), [0103](../adr/0103-source-region.md), [0113](../adr/0113-experience-level-and-first-run.md)

## Purpose

Turn an OBS Studio scene collection, profile and hotkeys into a new RBS scene collection, output profile and shortcuts, and report every OBS object that did not come across exactly.

## Scope

In scope: reading OBS files, the import plan, the mapper registry, the transform conversion, the report, the entry points (`rbs-cli`, desktop, control API).

Out of scope: writing OBS files; importing from other apps (Streamlabs, XSplit); loading OBS plugins; adding skipped objects later (ADR-0131 decision 7 keeps the data for it).

## Contract

```rust
// rbs_import_obs
pub fn default_config_dirs() -> Vec<PathBuf>;                 // existing OBS folders on this OS
pub fn scan(config_dir: &Path) -> Result<ObsInstall, ImportError>; // collections and profiles by name
pub struct ObsInstall { collections: Vec<Found>, profiles: Vec<Found> }
pub struct Found { name: String, path: PathBuf }

pub trait Lookup {
    fn picture_size(&self, path: &Path) -> Option<(u32, u32)>;
    fn file_exists(&self, path: &Path) -> bool;
    fn targets(&self, type_id: &NodeTypeId) -> Vec<LookupTarget>; // asked at most once per type per import
}
pub struct LookupTarget { target: String, name: String, app: Option<String>, size: Option<(u32, u32)> }

pub struct Options { stream_key: bool, shortcuts: bool }      // both default true

pub fn plan(collection_json: &[u8], profile: Option<&ProfileFiles>, mappers: &Mappers,
            lookup: &dyn Lookup, options: &Options) -> Result<ImportPlan, ImportError>;
pub struct ProfileFiles { basic_ini: Vec<u8>, service_json: Option<Vec<u8>>,
                          stream_encoder: Option<Vec<u8>>, record_encoder: Option<Vec<u8>> }

pub struct ImportPlan {
    pub name: String,                          // the OBS collection name
    pub collection: SceneCollection,           // passed SceneCollection::from_parts
    pub profile: Option<ObsProfileFacts>,
    pub shortcuts: Vec<ImportedShortcut>,      // { command, chord: Chord, scope }
    pub report: ImportReport,
}

pub struct Mappers;                            // builtin(), register_source/filter/transition
pub trait SourceMapper { fn obs_ids(&self) -> &'static [&'static str];
                         fn map(&self, src: &ObsSource, ctx: &mut MapContext) -> SourceOutcome; }
pub trait FilterMapper { /* same shape, FilterOutcome */ }
pub trait TransitionMapper { /* same shape, TransitionOutcome */ }
```

`ImportError`: `TooLarge`, `NotJson`, `NotACollection` (no `sources` array), `Invalid(String)` (the built collection failed `from_parts`; a bug in a mapper, reported with the mapper's OBS id). A single bad source, filter or item is never an error: it is a report entry.

### Report

```rust
pub struct ImportReport { pub exact: Counts, pub entries: Vec<ReportEntry> }
pub struct ReportEntry { outcome: Outcome /* Approximated | Skipped */, area: Area,
                         obs_name: String, obs_type: String, scenes: Vec<String>,
                         reason: String, hint: Option<String> }
```

Reasons are one sentence of plain English ("Only PNG pictures can be shown for now."). No stream key, file content or full path appears in a report; a path is shown by its file name.

`rbs-app` saves the report as `imports/<collection id>.report.toml` (`kind = "obs-import-report"`, format 1) with the skipped objects' OBS JSON under `[[skipped]]`.

### Transform conversion

OBS places an item at `pos` with the anchor `align` (bits: left 1, right 2, top 4, bottom 8; none is centre), size `native × scale` or, with a bounds type, the bounds box (stretch, scale inner, scale outer, to width, to height, max only) aligned by `bounds_align`, rotated by `rot` degrees about `pos`, after `crop_*`. RBS stores the box (`Layout.position`, `Layout.size`, negative for a flip) rotated about its centre. The converter computes the four corners of the OBS item and returns the RBS box with the same corners (within 0.01 px). The native size comes from `Lookup` or settings; when unknown, the canvas size is used and the item is marked approximate.

### Mapping table

ADR-0131 decision 5 is the first mapping table. Each mapper keeps its own row in this section when it is implemented.

## Errors and failure behaviour

A failed plan writes nothing. A plan that succeeds is written as a new collection and a new output profile under the writer lock; a write error leaves no half-written entry (`Library::create` is atomic). Unknown keys are ignored; a missing key takes OBS's default.

## Performance requirements

Runs on the control thread on demand only. A 200-source collection plans in under 100 ms on the reference machines, excluding `Lookup` enumeration. No threads, timers or resources while no import runs.

## Conformance tests

`crates/rbs-import-obs/tests`: golden fixtures (hand-written for this repository) for each mapper, groups, nested scenes, global audio devices, cross-platform ids, unsupported types; the property test of the transform conversion; a report contains no stream key; `from_parts` accepts every fixture's collection.

## Open questions

- Converting non-PNG images (needs a decoder; part of the media work).
- Mapping the Aitum vertical canvas plugin's data to the RBS vertical canvas (ADR-0093).
- Saved projectors to projector windows (ADR-0125).
