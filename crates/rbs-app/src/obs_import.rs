//! The app's side of the OBS import (ADR-0131, spec `docs/specs/obs-import.md`): the
//! build's catalogue, the profile facts mapped onto this app's recording and stream
//! settings, and the write in the order of decision 9 (collection, output profile,
//! stream key, shortcuts).
//!
//! Replay and audio settings are app-wide files, not part of an output profile; the
//! import changes no existing file but `shortcuts.toml` (decision 9), so they are
//! reported with what to set, never written.

use std::path::Path;

use rbs_graph::node::NodeTypeId;
use rbs_import_obs::profile::{
    Codec, EncoderFacts, EncoderKind, OutputFacts, RateControl, RecordFormat, SpeedPreset,
};
use rbs_import_obs::{Area, Catalogue, ImportPlan, ObsProfileFacts, Outcome, ReportEntry};
use rbs_params::Field;
use rbs_transport::Secret;
use rbs_ui::shortcut::Platform;
use rbs_user_data::{EntryId, OutputProfile, UserData, WriterLock};

use crate::record_profile::{
    AUDIO_KBPS, Encoding, Format, MAX_B_FRAMES, OutputSize, Preset, RateMode, RecordProfile,
    VideoCodec, output_size_problem,
};
use crate::stream_key;
use crate::stream_profile::{StreamProfile, parse_server};
use crate::user_session::{RECORDING, STREAM};

/// The RBS types of this build: sources from `rbs_sources::types`, filters from the
/// video and audio filter catalogues.
pub struct AppCatalogue;

impl Catalogue for AppCatalogue {
    fn has(&self, type_id: &NodeTypeId) -> bool {
        let t = type_id.as_str();
        rbs_sources::types::ALL.contains(&t)
            || rbs_filters::find(t).is_some()
            || rbs_audio_filters::find(t).is_some()
    }

    fn fields(&self, type_id: &NodeTypeId) -> Option<&'static [Field]> {
        let t = type_id.as_str();
        rbs_filters::find(t)
            .map(|f| f.fields())
            .or_else(|| rbs_audio_filters::find(t).map(|f| f.fields()))
    }
}

fn note(outcome: Outcome, name: &str, reason: impl Into<String>) -> ReportEntry {
    ReportEntry::new(outcome, Area::Profile, name, "", reason)
}

fn nearest(value: u32, choices: &[u32]) -> u32 {
    choices
        .iter()
        .copied()
        .min_by_key(|c| c.abs_diff(value))
        .unwrap_or(value)
}

/// Applies the OBS encoder facts to `e`; `what` names the part ("Recording encoder").
fn apply(e: &mut Encoding, f: &EncoderFacts, what: &str, notes: &mut Vec<ReportEntry>) {
    if let Some((codec, kind)) = f.encoder {
        e.video_codec = match codec {
            Codec::Hevc => VideoCodec::Hevc,
            Codec::Av1 => VideoCodec::Av1,
            _ => VideoCodec::H264,
        };
        if kind == EncoderKind::Software {
            notes.push(
                note(
                    Outcome::Approximated,
                    what,
                    "OBS encoded on the processor; RBS picks its best encoder, usually the graphics card's.",
                )
                .with_hint("Pick a software encoder in Settings if you want the processor."),
            );
        }
    }
    if let Some(rc) = f.rate_control {
        e.rate_control = match rc {
            RateControl::Vbr => RateMode::Vbr,
            RateControl::ConstantQuality => RateMode::ConstantQuality,
            _ => RateMode::Cbr,
        };
    }
    if let Some(k) = f.video_kbps {
        let clamped = k.clamp(500, 200_000);
        if clamped != k {
            notes.push(note(
                Outcome::Approximated,
                what,
                format!(
                    "The video bitrate {k} kbit/s became {clamped} kbit/s, the nearest RBS allows."
                ),
            ));
        }
        e.video_kbps = clamped;
    }
    e.max_video_kbps = f.max_video_kbps.map(|m| m.max(e.video_kbps).min(200_000));
    if let Some(q) = f.quality {
        e.quality = q.min(51);
    }
    if let Some(s) = f.keyframe_seconds {
        e.keyframe_seconds = s.clamp(1, 60);
    }
    if let Some(p) = f.preset {
        e.preset = match p {
            SpeedPreset::Quality => Preset::Quality,
            SpeedPreset::Speed => Preset::Speed,
            _ => Preset::Balanced,
        };
    }
    if let Some(b) = f.b_frames {
        e.b_frames = b.min(MAX_B_FRAMES);
    }
    if let Some(a) = f.audio_kbps {
        let n = nearest(a, &AUDIO_KBPS);
        if n != a {
            notes.push(note(
                Outcome::Approximated,
                what,
                format!("The audio bitrate {a} kbit/s became {n} kbit/s."),
            ));
        }
        e.audio_kbps = n;
    }
}

fn output_size(facts: &OutputFacts, notes: &mut Vec<ReportEntry>) -> Option<OutputSize> {
    let (width, height) = facts.output_size?;
    let size = Some(OutputSize { width, height });
    if output_size_problem(size).is_some() {
        notes.push(note(
            Outcome::Approximated,
            "Output size",
            format!("The output size {width} x {height} is not one RBS can encode, so the canvas size is used."),
        ));
        return None;
    }
    size
}

/// The new output profile from the OBS profile facts, and a report entry for each
/// setting that did not come across exactly. `has_key`: a stream key is imported
/// with it. `folder_exists` asks this computer about the recording folder.
pub fn output_profile(
    facts: &ObsProfileFacts,
    has_key: bool,
    folder_exists: &dyn Fn(&Path) -> bool,
) -> (OutputProfile, Vec<ReportEntry>) {
    let mut notes = Vec::new();
    let out = &facts.output;
    let size = output_size(out, &mut notes);
    let mut profile = OutputProfile::new();

    if let Some(r) = &out.recording {
        let mut rec = RecordProfile::new();
        match &r.folder {
            Some(f) if f.is_absolute() && folder_exists(f) => rec.folder = f.clone(),
            Some(_) => notes.push(
                note(
                    Outcome::Approximated,
                    "Recording folder",
                    "OBS's recording folder is not on this computer, so recordings go to the default folder.",
                )
                .with_hint("Pick a folder in Settings › Recording."),
            ),
            None => {}
        }
        if let Some(f) = r.format {
            rec.format = match f {
                RecordFormat::Mp4 => Format::Mp4,
                RecordFormat::Mov => Format::Mov,
                _ => Format::Mkv,
            };
            rec.video_format = rec.format;
        }
        let mut e = rec.encoding();
        apply(&mut e, &r.encoder, "Recording encoder", &mut notes);
        rec.set_encoding(e);
        rec.output_size = size;
        if let Some(why) = rec.problem() {
            notes.push(note(
                Outcome::Approximated,
                "Recording settings",
                format!("{why} The RBS defaults are used for the encoder."),
            ));
            let folder = rec.folder.clone();
            let format = rec.format;
            rec = RecordProfile::new();
            rec.folder = folder;
            rec.format = format;
            rec.video_format = format;
        }
        if let Err(e) = profile.set(RECORDING, &rec) {
            notes.push(note(Outcome::Skipped, "Recording settings", e.to_string()));
        }
    }

    if let Some(s) = &out.stream {
        let mut st = StreamProfile::new();
        if let Some(server) = &s.server {
            match parse_server(server) {
                Ok(_) => st.server = server.clone(),
                Err(_) => notes.push(
                    note(
                        Outcome::Skipped,
                        "Stream server",
                        "The server address is not one RBS can use.",
                    )
                    .with_hint("Enter the server address in Settings › Stream."),
                ),
            }
        } else if let Some(service) = &s.service {
            notes.push(
                note(
                    Outcome::Skipped,
                    "Stream server",
                    format!("OBS used its {service} service list; RBS needs the server address."),
                )
                .with_hint("Enter the server address in Settings › Stream."),
            );
        }
        st.key_saved = has_key;
        apply(&mut st.encoding, &s.encoder, "Stream encoder", &mut notes);
        st.output_size = size;
        if let Some(why) = st.problem() {
            notes.push(note(
                Outcome::Approximated,
                "Stream settings",
                format!("{why} The RBS defaults are used for the encoder."),
            ));
            st.encoding = StreamProfile::new().encoding;
        }
        if let Err(e) = profile.set(STREAM, &st) {
            notes.push(note(Outcome::Skipped, "Stream settings", e.to_string()));
        }
    }

    if let Some(r) = &out.replay
        && r.enabled
    {
        notes.push(
                ReportEntry::new(
                    Outcome::Skipped,
                    Area::Setting,
                    "Replay buffer",
                    "",
                    format!(
                        "OBS kept the last {} seconds; replay settings are for all profiles in RBS, so the import does not change them.",
                        r.seconds
                    ),
                )
                .with_hint("Turn on the replay buffer in Settings › Replay."),
            );
    }
    if let Some(device) = out
        .audio
        .as_ref()
        .and_then(|a| a.monitoring_device.as_ref())
    {
        notes.push(
                ReportEntry::new(
                    Outcome::Skipped,
                    Area::Setting,
                    "Monitoring device",
                    "",
                    format!(
                        "OBS monitored on {device:?}; the monitoring device is for all profiles in RBS, so the import does not change it."
                    ),
                )
                .with_hint("Pick it in Settings › Audio."),
            );
    }
    (profile, notes)
}

/// What [`write`] wrote.
#[derive(Debug)]
pub struct Written {
    /// The new scene collection's id and name.
    pub collection: (EntryId, String),
    /// The new output profile's id and name, when the profile was imported.
    pub profile: Option<(EntryId, String)>,
    /// Shortcuts added.
    pub shortcuts_added: usize,
    /// Entries for what the write left out or could not do, beside the plan's report.
    pub notes: Vec<ReportEntry>,
}

/// Writes a complete plan in the order of ADR-0131 decision 9: the collection, the
/// output profile, the stream key (`stream-key/<profile id>`), the shortcuts. Only the
/// collection is required; a later step that fails is a note, and the earlier ones
/// stay. The profile takes the OBS profile's name, or the collection's.
pub fn write(
    data: &UserData,
    lock: &WriterLock,
    plan: &ImportPlan,
    save_key: &dyn Fn(&str, Secret) -> Result<(), String>,
    platform: Platform,
    folder_exists: &dyn Fn(&Path) -> bool,
) -> Result<Written, String> {
    let collection = data
        .scene_collections()
        .create_free(lock, &plan.name, &plan.collection)
        .map_err(|e| e.to_string())?;
    let mut notes = Vec::new();
    let step =
        |what: &str, why: String| ReportEntry::new(Outcome::Skipped, Area::Profile, what, "", why);

    let mut profile_written = None;
    if let Some(facts) = &plan.profile {
        let key = plan.secrets.stream_key.as_ref();
        let stream_chosen = facts.output.stream.is_some();
        let (profile, mut more) =
            output_profile(facts, key.is_some() && stream_chosen, folder_exists);
        notes.append(&mut more);
        let wanted = facts.name.clone().unwrap_or_else(|| plan.name.clone());
        let has_sections = profile.sections().next().is_some();
        if has_sections {
            match data.profiles().create_free(lock, &wanted, &profile) {
                Ok(p) => profile_written = Some(p),
                Err(e) => notes.push(step(
                    "Output profile",
                    format!("The output profile could not be written: {e}. The scene collection was written."),
                )),
            }
        }
        match (&profile_written, key) {
            (Some((id, _)), Some(k)) if stream_chosen => {
                if let Err(e) = save_key(&stream_key::account(id.as_str()), Secret::new(k.expose()))
                {
                    notes.push(
                        step(
                            "Stream key",
                            format!("The stream key could not be saved: {e}."),
                        )
                        .with_hint("Enter it in Settings › Stream."),
                    );
                }
            }
            (None, Some(_)) => notes.push(
                step(
                    "Stream key",
                    "The stream key was not saved because the output profile was not written."
                        .into(),
                )
                .with_hint("Enter it in Settings › Stream."),
            ),
            _ => {}
        }
    }

    let mut shortcuts_added = 0;
    if !plan.shortcuts.is_empty() {
        let (mut doc, err) = data.shortcuts();
        if err.is_some() {
            notes.push(ReportEntry::new(
                Outcome::Skipped,
                Area::Hotkey,
                "Hotkeys",
                "",
                "The shortcuts file could not be read, so no hotkeys were added; it was left as it is.",
            ));
        } else {
            let left = rbs_import_obs::merge_shortcuts(
                &mut doc,
                &plan.shortcuts,
                collection.0.as_str(),
                platform,
            );
            let added = plan.shortcuts.len() - left.len();
            notes.extend(left);
            if added > 0 {
                match data.set_shortcuts(lock, &doc) {
                    Ok(()) => shortcuts_added = added,
                    Err(e) => notes.push(ReportEntry::new(
                        Outcome::Skipped,
                        Area::Hotkey,
                        "Hotkeys",
                        "",
                        format!("The hotkeys could not be added: {e}."),
                    )),
                }
            }
        }
    }
    Ok(Written {
        collection,
        profile: profile_written,
        shortcuts_added,
        notes,
    })
}

/// Files on this computer for the import: PNG sizes and file presence. Devices are not
/// enumerated yet, so device targets keep their defaults and the report says so.
pub struct AppLookup;

impl rbs_import_obs::Lookup for AppLookup {
    fn picture_size(&self, path: &Path) -> Option<(u32, u32)> {
        use std::io::Read as _;
        let mut head = [0u8; 24];
        std::fs::File::open(path).ok()?.read_exact(&mut head).ok()?;
        if head[..8] != *b"\x89PNG\r\n\x1a\n" || head[12..16] != *b"IHDR" {
            return None;
        }
        let w = u32::from_be_bytes([head[16], head[17], head[18], head[19]]);
        let h = u32::from_be_bytes([head[20], head[21], head[22], head[23]]);
        (w > 0 && h > 0).then_some((w, h))
    }
    fn file_exists(&self, path: &Path) -> bool {
        path.is_file()
    }
    fn targets(&self, _: &NodeTypeId) -> Vec<rbs_import_obs::LookupTarget> {
        Vec::new()
    }
}

/// The OBS collections and profiles found in OBS's settings folders on this computer.
#[derive(Debug, Clone, Default)]
pub struct Found {
    /// Collections: name and file.
    pub collections: Vec<(String, std::path::PathBuf)>,
    /// Profiles: name and folder.
    pub profiles: Vec<(String, std::path::PathBuf)>,
}

/// Scans OBS's settings folders. Reads only the file names and each collection's
/// `name`; an unreadable folder is left out.
pub fn find_obs() -> Found {
    let mut f = Found::default();
    for dir in rbs_import_obs::discover::default_config_dirs() {
        if let Ok(i) = rbs_import_obs::discover::scan(&dir) {
            f.collections
                .extend(i.collections.into_iter().map(|c| (c.name, c.path)));
            f.profiles
                .extend(i.profiles.into_iter().map(|p| (p.name, p.path)));
        }
    }
    f
}

/// The names of a collection's scenes in OBS's order (`scene_order`, else the order of
/// the scene sources). Empty when the file is not a collection.
pub fn scene_names(json: &[u8]) -> Vec<String> {
    let Ok(root) = serde_json::from_slice::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let names = |v: Option<&serde_json::Value>, pick: &dyn Fn(&serde_json::Value) -> bool| {
        v.and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter(|x| pick(x))
                    .filter_map(|x| x.get("name").and_then(serde_json::Value::as_str))
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let order = names(root.get("scene_order"), &|_| true);
    if !order.is_empty() {
        return order;
    }
    names(root.get("sources"), &|x| {
        x.get("id").and_then(serde_json::Value::as_str) == Some("scene")
    })
}

/// "1920 x 1080, 60 fps" from a profile's `[Video]`, or empty.
pub fn canvas_summary(files: &rbs_import_obs::ProfileFiles) -> String {
    let ini = rbs_import_obs::ini::Ini::parse(&String::from_utf8_lossy(&files.basic_ini));
    let v = rbs_import_obs::VideoFacts::read(&ini);
    match (v.base, v.fps) {
        (Some((w, h)), Some((n, d))) if d > 0 => {
            let fps = f64::from(n) / f64::from(d);
            let fps = if (fps - fps.round()).abs() < 0.005 {
                format!("{}", fps.round())
            } else {
                format!("{fps:.2}")
            };
            format!("{w} x {h}, {fps} fps")
        }
        (Some((w, h)), _) => format!("{w} x {h}"),
        _ => String::new(),
    }
}

/// One line of the Check page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckRow {
    /// The OBS object.
    pub name: String,
    /// The scenes it is in, or empty.
    pub place: String,
    /// Why, and what to do.
    pub reason: String,
}

impl From<&ReportEntry> for CheckRow {
    fn from(e: &ReportEntry) -> Self {
        Self {
            name: e.obs_name.clone(),
            place: e.scenes.join(", "),
            reason: match &e.hint {
                Some(h) => format!("{} {h}", e.reason),
                None => e.reason.clone(),
            },
        }
    }
}

/// The Check page: "4 · 8 · 7" (scenes, sources, items as in OBS), what came across
/// approximately and what did not. Parts the user left out are not listed.
pub fn check_view(report: &rbs_import_obs::ImportReport) -> (String, Vec<CheckRow>, Vec<CheckRow>) {
    let e = &report.exact;
    let exact = format!("{} · {} · {}", e.scenes, e.sources, e.items);
    let rows = |o: Outcome| report.with_outcome(o).map(CheckRow::from).collect();
    (exact, rows(Outcome::Approximated), rows(Outcome::Skipped))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use rbs_import_obs::discover::read_profile;
    use rbs_import_obs::mappers::filters::rbs as filter_types;
    use rbs_import_obs::{Lookup, LookupTarget, Mappers, Options, plan};
    use std::path::PathBuf;

    use crate::stream_key::{KeyStore, MemoryKeyStore};

    /// ADR-0131 decision 1: every mapper's RBS type and field key exists in this build.
    #[test]
    fn every_mapper_type_and_key_exists_in_this_build() {
        let mappers = Mappers::builtin();
        for m in mappers.sources() {
            for t in m.rbs_types() {
                assert!(rbs_sources::types::ALL.contains(t), "{}: {t}", m.obs_id());
            }
        }
        for m in mappers.filters() {
            for t in m.rbs_types() {
                assert!(
                    AppCatalogue.has(&NodeTypeId::new(*t)),
                    "{}: {t}",
                    m.obs_id()
                );
            }
        }
        for (t, used) in filter_types::ALL {
            let fields = AppCatalogue
                .fields(&NodeTypeId::new(t))
                .unwrap_or_else(|| panic!("{t} has no schema"));
            for k in used {
                assert!(fields.iter().any(|f| f.key == *k), "{t}: {k}");
            }
        }
    }

    struct Nothing;
    impl Lookup for Nothing {
        fn picture_size(&self, _: &Path) -> Option<(u32, u32)> {
            None
        }
        fn file_exists(&self, _: &Path) -> bool {
            false
        }
        fn targets(&self, _: &NodeTypeId) -> Vec<LookupTarget> {
            Vec::new()
        }
        fn platform(&self) -> Platform {
            Platform::Windows
        }
    }

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../rbs-import-obs/tests/fixtures")
    }

    fn the_plan(profile: &str, options: &Options) -> ImportPlan {
        let json = std::fs::read(fixtures().join("show.json")).unwrap();
        let files = read_profile(&fixtures().join(profile)).unwrap();
        plan(
            &json,
            Some(&files),
            &Mappers::builtin(),
            &AppCatalogue,
            &Nothing,
            options,
        )
        .unwrap()
    }

    #[test]
    fn an_advanced_profile_becomes_recording_and_stream_settings() {
        let p = the_plan("advanced", &Options::default());
        let facts = p.profile.as_ref().unwrap();
        let (profile, notes) = output_profile(facts, true, &|_| true);
        let rec: RecordProfile = profile.get(RECORDING).unwrap().unwrap();
        assert_eq!(rec.format, Format::Mp4);
        assert_eq!(rec.rate_control, RateMode::ConstantQuality);
        assert_eq!(rec.quality, 18);
        assert_eq!(rec.preset, Preset::Speed);
        assert_eq!(rec.audio_kbps, 160);
        // A Windows folder is kept on Windows; elsewhere it is not a full path here.
        let obs_folder = PathBuf::from("C:/Users/someone/Videos");
        let expected = if obs_folder.is_absolute() {
            obs_folder
        } else {
            crate::record_profile::default_folder()
        };
        assert_eq!(rec.folder, expected);
        assert_eq!(
            rec.output_size,
            Some(OutputSize {
                width: 1920,
                height: 1080
            })
        );
        let st: StreamProfile = profile.get(STREAM).unwrap().unwrap();
        assert_eq!(st.server, "rtmp://live.example.com/app");
        assert!(st.key_saved);
        assert_eq!(st.encoding.video_codec, VideoCodec::Hevc);
        assert_eq!(st.encoding.rate_control, RateMode::Vbr);
        assert_eq!(st.encoding.video_kbps, 8000);
        assert_eq!(st.encoding.max_video_kbps, Some(12_000));
        assert_eq!(st.encoding.b_frames, 2);
        assert!(rec.problem().is_none() && st.problem().is_none());
        let text: Vec<String> = notes.iter().map(ToString::to_string).collect();
        assert!(text.iter().any(|t| t.contains("Replay buffer")), "{text:?}");
        assert!(
            text.iter().any(|t| t.contains("Recording encoder")),
            "{text:?}"
        );
    }

    #[test]
    fn a_missing_folder_and_an_odd_audio_rate_are_reported() {
        let p = the_plan("simple", &Options::default());
        let (profile, notes) = output_profile(p.profile.as_ref().unwrap(), false, &|_| false);
        let rec: RecordProfile = profile.get(RECORDING).unwrap().unwrap();
        assert_eq!(rec.folder, crate::record_profile::default_folder());
        assert_eq!(rec.format, Format::Mkv);
        assert_eq!(rec.quality, 23);
        let st: StreamProfile = profile.get(STREAM).unwrap().unwrap();
        assert!(st.server.is_empty() && !st.key_saved);
        assert_eq!(st.encoding.video_kbps, 4500);
        let text: Vec<String> = notes.iter().map(ToString::to_string).collect();
        assert!(
            text.iter().any(|t| t.contains("Recording folder")),
            "{text:?}"
        );
        assert!(text.iter().any(|t| t.contains("Twitch")), "{text:?}");
        assert!(!text.iter().any(|t| t.contains("/Users/")), "{text:?}");
    }

    #[test]
    fn write_puts_collection_profile_key_and_shortcuts_in_order() {
        let dir = std::env::temp_dir().join(format!("rbs-app-obs-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let data = UserData::new(&dir);
        let lock = data.try_lock().unwrap().unwrap();
        let keys = MemoryKeyStore::new();
        let p = the_plan("advanced", &Options::default());
        let w = write(
            &data,
            &lock,
            &p,
            &|a, k| keys.write(a, &k),
            Platform::Windows,
            &|_| true,
        )
        .unwrap();
        assert_eq!(w.collection.1, "Evening show");
        let (pid, pname) = w.profile.clone().unwrap();
        assert_eq!(pname, "Advanced");
        let saved = keys
            .read(&stream_key::account(pid.as_str()))
            .unwrap()
            .unwrap();
        assert_eq!(saved.expose(), "live_SECRET-KEY-42");
        assert_eq!(w.shortcuts_added, 4);
        let (doc, _) = data.shortcuts();
        assert!(doc.bindings.iter().any(|b| b.command == "stream.toggle"));
        assert!(!format!("{w:?}").contains("SECRET"));

        // A second import gets free names and adds no duplicate shortcut.
        let w2 = write(
            &data,
            &lock,
            &p,
            &|a, k| keys.write(a, &k),
            Platform::Windows,
            &|_| true,
        )
        .unwrap();
        assert_eq!(w2.collection.1, "Evening show 2");
        assert_eq!(w2.profile.unwrap().1, "Advanced 2");
        let (doc2, _) = data.shortcuts();
        assert_eq!(doc2.bindings.len(), doc.bindings.len());
        drop(lock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn without_the_stream_settings_no_key_is_saved() {
        let dir = std::env::temp_dir().join(format!("rbs-app-obs-nokey-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let data = UserData::new(&dir);
        let lock = data.try_lock().unwrap().unwrap();
        let keys = MemoryKeyStore::new();
        let options = Options {
            stream_server: false,
            ..Options::default()
        };
        let p = the_plan("advanced", &options);
        let w = write(
            &data,
            &lock,
            &p,
            &|a, k| keys.write(a, &k),
            Platform::Windows,
            &|_| true,
        )
        .unwrap();
        let (pid, _) = w.profile.unwrap();
        assert!(
            keys.read(&stream_key::account(pid.as_str()))
                .unwrap()
                .is_none()
        );
        let profile: OutputProfile = data.profiles().load(&pid).unwrap().document;
        assert!(profile.get::<StreamProfile>(STREAM).unwrap().is_none());
        drop(lock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_wizard_helpers_read_the_fixtures() {
        let json = std::fs::read(fixtures().join("show.json")).unwrap();
        let names = scene_names(&json);
        assert!(names.contains(&"Main".to_owned()), "{names:?}");
        assert!(scene_names(b"not json").is_empty());
        let files = read_profile(&fixtures().join("advanced")).unwrap();
        assert_eq!(canvas_summary(&files), "2560 x 1440, 59.94 fps");
        let p = the_plan("advanced", &Options::default());
        let (exact, approx, skipped) = check_view(&p.report);
        assert_eq!(exact.matches('·').count(), 2);
        assert!(!approx.is_empty() && !skipped.is_empty());
        assert!(skipped.iter().all(|r| !r.reason.contains("SECRET")));
    }
}
