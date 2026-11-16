//! `rbs-cli obs-import`: check or run an OBS import from the command line (ADR-0131
//! decision 12, step 1). `check` prints the report and writes nothing; `run` writes the
//! plan as a new scene collection in the user data folder, under the writer lock.

use std::io::{Read as _, Write};
use std::path::{Path, PathBuf};

use rbs_graph::node::NodeTypeId;
use rbs_import_obs::{
    Catalogue, Lookup, LookupTarget, Mappers, Options, SceneChoice, discover, plan,
};
use rbs_params::Field;

/// What to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObsImportCommand {
    /// `obs-import scan`: list OBS folders, collections and profiles.
    Scan,
    /// `obs-import check|run <collection.json> [options]`.
    Plan(PlanArgs),
}

/// `check` or `run`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanArgs {
    /// `run` writes; `check` only reports.
    pub write: bool,
    /// The OBS collection file.
    pub collection: PathBuf,
    /// The OBS profile folder.
    pub profile: Option<PathBuf>,
    /// The wizard's choices.
    pub options: Options,
}

/// Parses the words after `obs-import`.
pub fn parse<'a>(mut it: impl Iterator<Item = &'a str>) -> Result<ObsImportCommand, String> {
    let write = match it.next() {
        Some("scan") => {
            return match it.next() {
                None => Ok(ObsImportCommand::Scan),
                Some(extra) => Err(format!("obs-import scan: unexpected {extra:?}")),
            };
        }
        Some("check") => false,
        Some("run") => true,
        Some(other) => return Err(format!("obs-import: unknown command {other:?}")),
        None => return Err("obs-import needs scan, check or run".into()),
    };
    let mut collection = None;
    let mut profile = None;
    let mut options = Options::default();
    let mut scenes = std::collections::BTreeSet::new();
    while let Some(arg) = it.next() {
        match arg {
            "--profile" => {
                let dir = it.next().ok_or("obs-import: --profile needs a folder")?;
                profile = Some(PathBuf::from(dir));
            }
            "--scene" => {
                let name = it.next().ok_or("obs-import: --scene needs a name")?;
                scenes.insert(name.to_owned());
            }
            "--no-global-audio" => options.global_audio = false,
            "--no-audio-filters" => options.audio_filters = false,
            "--no-video-filters" => options.video_filters = false,
            "--no-canvas" => options.canvas = false,
            other if other.starts_with("--") => {
                return Err(format!("obs-import: unexpected {other:?}"));
            }
            path if collection.is_none() => collection = Some(PathBuf::from(path)),
            extra => return Err(format!("obs-import: unexpected {extra:?}")),
        }
    }
    if !scenes.is_empty() {
        options.scenes = SceneChoice::Only(scenes);
    }
    Ok(ObsImportCommand::Plan(PlanArgs {
        write,
        collection: collection.ok_or("obs-import needs the OBS collection file (.json)")?,
        profile,
        options,
    }))
}

/// The RBS types of this build: sources from `rbs_sources::types`, filters from the
/// video and audio filter catalogues.
pub struct BuildCatalogue;

impl Catalogue for BuildCatalogue {
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

/// Files on this computer; devices are not enumerated from the command line, so
/// targets are left to the defaults and the report says so.
pub struct FileLookup;

/// Width and height from a PNG header, without decoding the picture.
pub fn png_size(path: &Path) -> Option<(u32, u32)> {
    let mut head = [0u8; 24];
    std::fs::File::open(path).ok()?.read_exact(&mut head).ok()?;
    if head[..8] != *b"\x89PNG\r\n\x1a\n" || head[12..16] != *b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes([head[16], head[17], head[18], head[19]]);
    let h = u32::from_be_bytes([head[20], head[21], head[22], head[23]]);
    (w > 0 && h > 0).then_some((w, h))
}

impl Lookup for FileLookup {
    fn picture_size(&self, path: &Path) -> Option<(u32, u32)> {
        png_size(path)
    }
    fn file_exists(&self, path: &Path) -> bool {
        path.is_file()
    }
    fn targets(&self, _: &NodeTypeId) -> Vec<LookupTarget> {
        Vec::new()
    }
}

/// Runs the command, writing to `out`.
pub fn run(cmd: &ObsImportCommand, config: &Path, out: &mut dyn Write) -> Result<(), String> {
    match cmd {
        ObsImportCommand::Scan => {
            let dirs = discover::default_config_dirs();
            if dirs.is_empty() {
                writeln!(out, "No OBS settings folder was found.").map_err(|e| e.to_string())?;
            }
            for dir in dirs {
                let found = discover::scan(&dir).map_err(|e| e.to_string())?;
                writeln!(out, "{}", dir.display()).map_err(|e| e.to_string())?;
                for c in &found.collections {
                    writeln!(out, "  collection {:?}: {}", c.name, c.path.display())
                        .map_err(|e| e.to_string())?;
                }
                for p in &found.profiles {
                    writeln!(out, "  profile {:?}: {}", p.name, p.path.display())
                        .map_err(|e| e.to_string())?;
                }
            }
            Ok(())
        }
        ObsImportCommand::Plan(a) => {
            let json = discover::read_limited(&a.collection).map_err(|e| e.to_string())?;
            let profile = match &a.profile {
                Some(dir) => Some(discover::read_profile(dir).map_err(|e| e.to_string())?),
                None => None,
            };
            let plan = plan(
                &json,
                profile.as_ref(),
                &Mappers::builtin(),
                &BuildCatalogue,
                &FileLookup,
                &a.options,
            )
            .map_err(|e| e.to_string())?;
            write!(out, "{}", plan.report).map_err(|e| e.to_string())?;
            if !a.write {
                writeln!(out, "Nothing was written (check).").map_err(|e| e.to_string())?;
                return Ok(());
            }
            let data = rbs_user_data::UserData::new(config);
            let lock = data
                .try_lock()
                .map_err(|e| e.to_string())?
                .ok_or("RBS is running; close it or import from the app")?;
            let (id, name) = data
                .scene_collections()
                .create_free(&lock, &plan.name, &plan.collection)
                .map_err(|e| e.to_string())?;
            writeln!(out, "Wrote the scene collection {name:?}.").map_err(|e| e.to_string())?;
            write_shortcuts(&data, &lock, &plan, id.as_str(), out)?;
            if plan.profile.is_some() {
                writeln!(
                    out,
                    "The output profile and the stream key are imported by the app (Settings, Import from OBS)."
                )
                .map_err(|e| e.to_string())?;
            }
            Ok(())
        }
    }
}

/// Adds the imported hotkeys to `shortcuts.toml` (ADR-0131 decision 9): the one
/// existing file an import changes, and only by adding free chords. When the file
/// cannot be read, it is left as it is and the step is skipped.
fn write_shortcuts(
    data: &rbs_user_data::UserData,
    lock: &rbs_user_data::WriterLock,
    plan: &rbs_import_obs::ImportPlan,
    collection_id: &str,
    out: &mut dyn Write,
) -> Result<(), String> {
    if plan.shortcuts.is_empty() {
        return Ok(());
    }
    let w = |out: &mut dyn Write, line: String| writeln!(out, "{line}").map_err(|e| e.to_string());
    let (mut doc, err) = data.shortcuts();
    if err.is_some() {
        return w(
            out,
            "The shortcuts file could not be read, so no hotkeys were added; it was left as it is."
                .into(),
        );
    }
    let left_out = rbs_import_obs::merge_shortcuts(
        &mut doc,
        &plan.shortcuts,
        collection_id,
        rbs_ui::shortcut::Platform::current(),
    );
    let added = plan.shortcuts.len() - left_out.len();
    if added > 0
        && let Err(e) = data.set_shortcuts(lock, &doc)
    {
        return w(
            out,
            format!("The hotkeys could not be added: {e}. The scene collection was written."),
        );
    }
    w(
        out,
        format!(
            "Added {added} of {} hotkeys as shortcuts.",
            plan.shortcuts.len()
        ),
    )?;
    for e in left_out {
        w(out, format!("- {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use rbs_import_obs::mappers::filters::rbs as filter_types;
    use rbs_import_obs::mappers::sources::rbs as source_types;

    /// ADR-0131 decision 1: the importer names RBS types as strings, so this test, in a
    /// composition root that links the real registries, keeps them honest.
    #[test]
    fn every_mapper_type_and_key_exists_in_this_build() {
        let mappers = Mappers::builtin();
        for m in mappers.sources() {
            for t in m.rbs_types() {
                assert!(rbs_sources::types::ALL.contains(t), "{}: {t}", m.obs_id());
            }
        }
        for t in source_types::ALL {
            assert!(rbs_sources::types::ALL.contains(&t), "{t}");
        }
        use rbs_sources::types::keys;
        let real = [
            keys::ADAPTER,
            keys::TARGET,
            keys::CURSOR,
            keys::WIDTH,
            keys::HEIGHT,
            keys::FPS,
            keys::PATH,
            keys::COLOR,
            keys::LOOP,
            keys::RESTART_WHEN_SHOWN,
            keys::URL,
            keys::TRANSPARENT,
        ];
        for k in source_types::keys::ALL {
            assert!(real.contains(&k), "{k}");
        }
        for m in mappers.filters() {
            for t in m.rbs_types() {
                assert!(
                    BuildCatalogue.has(&NodeTypeId::new(*t)),
                    "{}: {t}",
                    m.obs_id()
                );
            }
        }
        for (t, used) in filter_types::ALL {
            let fields = BuildCatalogue
                .fields(&NodeTypeId::new(t))
                .unwrap_or_else(|| panic!("{t} has no schema"));
            for k in used {
                assert!(fields.iter().any(|f| f.key == *k), "{t}: {k}");
            }
        }
    }

    #[test]
    fn arguments_parse() {
        let c = parse(
            [
                "check",
                "show.json",
                "--profile",
                "p",
                "--scene",
                "Main",
                "--no-audio-filters",
            ]
            .into_iter(),
        )
        .unwrap();
        let ObsImportCommand::Plan(a) = c else {
            panic!("plan")
        };
        assert!(!a.write);
        assert_eq!(a.profile, Some(PathBuf::from("p")));
        assert!(!a.options.audio_filters);
        assert!(matches!(a.options.scenes, SceneChoice::Only(_)));
        assert_eq!(parse(["scan"].into_iter()).unwrap(), ObsImportCommand::Scan);
        assert!(parse(["run"].into_iter()).is_err());
        assert!(parse(["check", "a.json", "--bogus"].into_iter()).is_err());
    }

    #[test]
    fn png_sizes_come_from_the_header() {
        let dir = std::env::temp_dir().join(format!("rbs-cli-png-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.png");
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        bytes.extend_from_slice(&640u32.to_be_bytes());
        bytes.extend_from_slice(&360u32.to_be_bytes());
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(png_size(&path), Some((640, 360)));
        std::fs::write(&path, b"not a png at all, really").unwrap();
        assert_eq!(png_size(&path), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_writes_a_new_collection_and_check_writes_nothing() {
        let dir = std::env::temp_dir().join(format!("rbs-cli-obs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("show.json");
        std::fs::write(
            &file,
            r##"{"name":"Show","sources":[
                {"id":"scene","name":"Main","settings":{"items":[{"name":"Bg","pos":{"x":0,"y":0},"scale":{"x":1,"y":1},"align":5}]}},
                {"id":"color_source_v3","name":"Bg","settings":{"width":1920,"height":1080}}]}"##,
        )
        .unwrap();
        let config = dir.join("config");
        let mut out = Vec::new();
        let check = ObsImportCommand::Plan(PlanArgs {
            write: false,
            collection: file.clone(),
            profile: None,
            options: Options::default(),
        });
        run(&check, &config, &mut out).unwrap();
        assert!(!config.join("scenes").exists());
        let run_cmd = ObsImportCommand::Plan(PlanArgs {
            write: true,
            ..match check {
                ObsImportCommand::Plan(a) => a,
                ObsImportCommand::Scan => unreachable!(),
            }
        });
        run(&run_cmd, &config, &mut out).unwrap();
        run(&run_cmd, &config, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("Wrote the scene collection \"Show\"."),
            "{text}"
        );
        assert!(
            text.contains("Wrote the scene collection \"Show 2\"."),
            "{text}"
        );
        let listed = rbs_user_data::UserData::new(&config)
            .scene_collections()
            .list()
            .unwrap();
        assert_eq!(listed.entries.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_with_a_profile_adds_the_hotkeys_as_shortcuts() {
        let dir = std::env::temp_dir().join(format!("rbs-cli-obs-keys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fixtures =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../rbs-import-obs/tests/fixtures");
        let cmd = ObsImportCommand::Plan(PlanArgs {
            write: true,
            collection: fixtures.join("show.json"),
            profile: Some(fixtures.join("advanced")),
            options: Options::default(),
        });
        let config = dir.join("config");
        let mut out = Vec::new();
        run(&cmd, &config, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("of 4 hotkeys as shortcuts."), "{text}");
        assert!(!text.contains("SECRET"), "{text}");
        let (doc, err) = rbs_user_data::UserData::new(&config).shortcuts();
        assert!(err.is_none());
        assert!(
            doc.bindings.iter().any(|b| b.command == "stream.toggle"),
            "{doc:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
