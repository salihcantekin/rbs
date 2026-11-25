//! Import an OBS Studio scene collection and profile (ADR-0131, spec
//! `docs/specs/obs-import.md`).
//!
//! [`plan`] is pure: the collection's JSON, the profile's files, the mapper registry,
//! the build's [`Catalogue`] and a [`Lookup`] of this computer give an [`ImportPlan`]:
//! a validated [`SceneCollection`], the profile facts and a report of every OBS object
//! that did not come across exactly. It writes nothing; the composition root writes
//! the plan as a new collection and output profile. [`discover`] finds OBS folders.

mod build;
pub mod discover;
pub mod env;
pub mod hotkeys;
pub mod ini;
pub mod mappers;
pub mod obs;
pub mod profile;
pub mod report;
pub mod transform;

use std::collections::BTreeSet;
use std::fmt;

use rbs_scene::SceneCollection;

pub use env::{Catalogue, Lookup, LookupTarget, NoLookup};
pub use hotkeys::{ImportedShortcut, merge_into as merge_shortcuts};
pub use ini::VideoFacts;
pub use mappers::Mappers;
pub use profile::{ImportedSecrets, OutputFacts, StreamKey};
pub use report::{Area, Counts, ImportReport, Outcome, ReportEntry};

/// Largest OBS file read, in bytes.
pub const MAX_FILE: u64 = 32 * 1024 * 1024;

/// The canvas used when the profile does not give one.
pub const FALLBACK_CANVAS: (u32, u32) = (1920, 1080);

/// The frame rate used when the profile does not give one.
pub const FALLBACK_FPS: (u32, u32) = (30, 1);

/// Which scenes to bring.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SceneChoice {
    /// Every scene.
    #[default]
    All,
    /// Exactly these scenes by OBS name; referenced scenes are not added.
    Only(BTreeSet<String>),
}

/// What the user chose to bring in the wizard (ADR-0131, owner direction). Every part
/// is on by default. A part left out is not in the plan and is one report entry with
/// [`Outcome::NotChosen`]. Build it with `..Options::default()` so a new choice keeps
/// its default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Scenes.
    pub scenes: SceneChoice,
    /// OBS's global audio devices (desktop audio, microphones).
    pub global_audio: bool,
    /// Audio filters.
    pub audio_filters: bool,
    /// Video filters, including a crop filter.
    pub video_filters: bool,
    /// Hotkeys (step 2).
    pub hotkeys: bool,
    /// Canvas size and frame rate from the profile.
    pub canvas: bool,
    /// Recording settings (step 2).
    pub recording: bool,
    /// Stream server (step 2).
    pub stream_server: bool,
    /// Stream key, into the credential store only (step 2).
    pub stream_key: bool,
    /// Replay buffer (step 2).
    pub replay: bool,
    /// Audio settings (step 2).
    pub audio_settings: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            scenes: SceneChoice::All,
            global_audio: true,
            audio_filters: true,
            video_filters: true,
            hotkeys: true,
            canvas: true,
            recording: true,
            stream_server: true,
            stream_key: true,
            replay: true,
            audio_settings: true,
        }
    }
}

/// The files of an OBS profile folder.
#[non_exhaustive]
#[derive(Debug, Clone, Default)]
pub struct ProfileFiles {
    /// `basic.ini`.
    pub basic_ini: Vec<u8>,
    /// `service.json`.
    pub service_json: Option<Vec<u8>>,
    /// `streamEncoder.json`.
    pub stream_encoder: Option<Vec<u8>>,
    /// `recordEncoder.json`.
    pub record_encoder: Option<Vec<u8>>,
}

impl ProfileFiles {
    /// A profile with only `basic.ini`.
    pub fn from_ini(basic_ini: impl Into<Vec<u8>>) -> Self {
        Self {
            basic_ini: basic_ini.into(),
            ..Self::default()
        }
    }
}

/// What an OBS profile says, in neutral terms the composition root maps onto its own
/// settings (ADR-0131 decision 4).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObsProfileFacts {
    /// The profile's name (`[General] Name`).
    pub name: Option<String>,
    /// `[Video]`.
    pub video: VideoFacts,
    /// Output, stream, replay and audio settings, each `None` when not chosen.
    pub output: OutputFacts,
}

/// The result of [`plan`].
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct ImportPlan {
    /// The OBS collection's name.
    pub name: String,
    /// The collection to write; it passed `SceneCollection::from_parts`.
    pub collection: SceneCollection,
    /// The profile facts, when a profile was given.
    pub profile: Option<ObsProfileFacts>,
    /// The stream key, for the credential store only.
    pub secrets: ImportedSecrets,
    /// Hotkeys as shortcuts, app scope. Empty when not chosen.
    pub shortcuts: Vec<ImportedShortcut>,
    /// What did not come across exactly.
    pub report: ImportReport,
}

/// Why no plan could be made. A single bad source, filter or item is never an error:
/// it is a report entry.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportError {
    /// A file is larger than [`MAX_FILE`].
    TooLarge,
    /// The collection is not JSON.
    NotJson,
    /// The JSON has no `sources` array.
    NotACollection,
    /// The folder has no `basic` folder.
    NotAnObsFolder,
    /// A file could not be read (the I/O error kind; never a path).
    Io(String),
    /// The built collection failed validation although the importer enforces its
    /// rules first: a bug.
    Internal(String),
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => write!(f, "The OBS file is larger than 32 MiB."),
            Self::NotJson => write!(f, "The file is not an OBS scene collection (not JSON)."),
            Self::NotACollection => write!(f, "The file is not an OBS scene collection."),
            Self::NotAnObsFolder => write!(f, "The folder is not an OBS settings folder."),
            Self::Io(kind) => write!(f, "The file could not be read: {kind}."),
            Self::Internal(e) => write!(f, "The import could not be built: {e}."),
        }
    }
}

impl std::error::Error for ImportError {}

/// Builds the import plan. Pure: reads only the given bytes and asks `lookup`.
pub fn plan(
    collection_json: &[u8],
    profile: Option<&ProfileFiles>,
    mappers: &Mappers,
    catalogue: &dyn Catalogue,
    lookup: &dyn Lookup,
    options: &Options,
) -> Result<ImportPlan, ImportError> {
    if collection_json.len() as u64 > MAX_FILE {
        return Err(ImportError::TooLarge);
    }
    if profile.is_some_and(|p| p.basic_ini.len() as u64 > MAX_FILE) {
        return Err(ImportError::TooLarge);
    }
    let root: serde_json::Value =
        serde_json::from_slice(collection_json).map_err(|_| ImportError::NotJson)?;
    if !root.get("sources").is_some_and(serde_json::Value::is_array) {
        return Err(ImportError::NotACollection);
    }
    for f in profile.iter().flat_map(|p| {
        [&p.service_json, &p.stream_encoder, &p.record_encoder]
            .into_iter()
            .flatten()
    }) {
        if f.len() as u64 > MAX_FILE {
            return Err(ImportError::TooLarge);
        }
    }
    let ini = profile.map(|p| ini::Ini::parse(&String::from_utf8_lossy(&p.basic_ini)));
    let mut later = ImportReport::default();
    let mut secrets = ImportedSecrets::default();
    let facts = profile.zip(ini.as_ref()).map(|(files, ini)| {
        let video = VideoFacts::read(ini);
        let (output, s) = profile::read(ini, files, video.base, video.output, options, &mut later);
        secrets = s;
        ObsProfileFacts {
            name: ini.get("General", "Name").map(str::to_owned),
            video,
            output,
        }
    });
    let (mut plan, scenes) = build::build(
        &root,
        facts,
        mappers,
        catalogue,
        lookup,
        options,
        ImportReport::default(),
    )?;
    if options.hotkeys {
        plan.shortcuts = hotkeys::read(ini.as_ref(), &root, &scenes, lookup.platform(), &mut later);
    } else if profile.is_some() {
        later.push(ReportEntry::new(
            Outcome::NotChosen,
            Area::Hotkey,
            "Hotkeys",
            "",
            "Hotkeys were not chosen.",
        ));
    }
    plan.secrets = secrets;
    for e in later.entries {
        plan.report.push(e);
    }
    Ok(plan)
}
