//! Golden and behaviour tests of the import plan (ADR-0131 decision 13). Fixtures are
//! hand-written for this repository. Set `UPDATE_GOLDEN=1` to rewrite a golden file
//! after a deliberate change, then review the diff.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;

use rbs_graph::node::NodeTypeId;
use rbs_import_obs::mappers::filters::rbs as filter_types;
use rbs_import_obs::mappers::sources::rbs as source_types;
use rbs_import_obs::{
    Area, Catalogue, ImportPlan, Lookup, LookupTarget, Mappers, NoLookup, Options, Outcome,
    ProfileFiles, SceneChoice, plan,
};
use rbs_params::Field;
use rbs_scene::model::{ItemContent, SettingValue};

const SHOW: &str = include_str!("fixtures/show.json");
const BASIC_INI: &str = include_str!("fixtures/basic.ini");

static GATE_FIELDS: [Field; 5] = [
    Field::number("open_threshold", "Open", -80.0, 0.0, -40.0, 0.5, "dB"),
    Field::number("close_threshold", "Close", -80.0, 0.0, -46.0, 0.5, "dB"),
    Field::number("attack", "Attack", 0.1, 100.0, 5.0, 0.1, "ms"),
    Field::number("hold", "Hold", 0.0, 500.0, 100.0, 1.0, "ms"),
    Field::number("release", "Release", 5.0, 1000.0, 150.0, 1.0, "ms"),
];
static DUCKER_FIELDS: [Field; 3] = [
    Field::number("threshold", "Threshold", -80.0, 0.0, -30.0, 0.5, "dB"),
    Field::number("attack", "Attack", 1.0, 200.0, 20.0, 1.0, "ms"),
    Field::number("release", "Release", 20.0, 3000.0, 500.0, 1.0, "ms"),
];

/// Every type of the first mapping set, with the two schemas the fixture clamps.
struct AllTypes {
    without: Vec<&'static str>,
}

impl Catalogue for AllTypes {
    fn has(&self, type_id: &NodeTypeId) -> bool {
        let t = type_id.as_str();
        (source_types::ALL.contains(&t) || filter_types::ALL.iter().any(|(f, _)| *f == t))
            && !self.without.contains(&t)
    }
    fn fields(&self, type_id: &NodeTypeId) -> Option<&'static [Field]> {
        match type_id.as_str() {
            filter_types::NOISE_GATE => Some(&GATE_FIELDS),
            filter_types::DUCKER => Some(&DUCKER_FIELDS),
            _ => None,
        }
    }
}

struct ThisComputer;

impl Lookup for ThisComputer {
    fn picture_size(&self, path: &Path) -> Option<(u32, u32)> {
        match path.file_name()?.to_str()? {
            "badge.png" => Some((100, 100)),
            "frame.png" => Some((1920, 1080)),
            _ => None,
        }
    }
    fn file_exists(&self, path: &Path) -> bool {
        path.ends_with("music.mp3")
    }
    fn targets(&self, type_id: &NodeTypeId) -> Vec<LookupTarget> {
        match type_id.as_str() {
            source_types::DISPLAY => {
                vec![LookupTarget::new("dxgi", "1", "Main display").with_size(1920, 1080)]
            }
            source_types::CAMERA => vec![
                LookupTarget::new("mf", "cam-1", "Logitech BRIO").with_size(1920, 1080),
                LookupTarget::new("mf", "cam-2", "Other camera"),
            ],
            source_types::AUDIO_INPUT => {
                vec![LookupTarget::new(
                    "wasapi",
                    "{0.0.1.00000000}.{mic-guid}",
                    "Microphone",
                )]
            }
            _ => Vec::new(),
        }
    }
}

fn run(options: &Options, catalogue: &AllTypes, lookup: &dyn Lookup) -> ImportPlan {
    plan(
        SHOW.as_bytes(),
        Some(&ProfileFiles::from_ini(BASIC_INI)),
        &Mappers::builtin(),
        catalogue,
        lookup,
        options,
    )
    .expect("plan")
}

fn all() -> AllTypes {
    AllTypes {
        without: Vec::new(),
    }
}

fn value(v: &SettingValue) -> String {
    match v {
        SettingValue::Bool(b) => b.to_string(),
        SettingValue::Int(i) => i.to_string(),
        SettingValue::Float(f) => format!("{f}"),
        SettingValue::Text(t) => format!("{t:?}"),
        other => format!("{other:?}"),
    }
}

/// A stable text form of a plan, for golden files.
fn summary(p: &ImportPlan) -> String {
    let c = &p.collection;
    let mut s = String::new();
    writeln!(s, "collection {:?}", p.name).unwrap();
    for canvas in c.canvases() {
        writeln!(
            s,
            "canvas {:?} {}x{} layout {}x{} fps {}/{} program {:?} globals {:?}",
            canvas.name,
            canvas.size.width,
            canvas.size.height,
            canvas.layout_size.width,
            canvas.layout_size.height,
            canvas.frame_rate.per_second().num(),
            canvas.frame_rate.per_second().den(),
            canvas.program.map(|id| c.scene(id).unwrap().name.clone()),
            canvas.globals.map(|id| c.scene(id).unwrap().name.clone()),
        )
        .unwrap();
    }
    for src in c.sources() {
        writeln!(s, "source {:?} {}", src.name, src.type_id.as_str()).unwrap();
        for (k, v) in &src.settings {
            writeln!(s, "  {k} = {}", value(v)).unwrap();
        }
        for (k, v) in &src.hints {
            writeln!(s, "  hint {k} = {v:?}").unwrap();
        }
        let a = src.audio;
        if !a.is_default() {
            writeln!(
                s,
                "  audio volume {} muted {} sync {} monitor {} hidden {}",
                a.volume,
                a.muted,
                a.sync_offset_ms,
                a.monitor.as_str(),
                a.mixer_hidden
            )
            .unwrap();
        }
        if let Some(r) = src.region {
            writeln!(s, "  region {:?}", r.to_array()).unwrap();
        }
        for f in &src.filters {
            write!(s, "  filter {} enabled {}", f.type_id.as_str(), f.enabled).unwrap();
            for (k, v) in &f.settings {
                write!(s, " {k}={}", value(v)).unwrap();
            }
            writeln!(s).unwrap();
        }
    }
    for scene in c.scenes() {
        writeln!(s, "scene {:?}", scene.name).unwrap();
        for item in &scene.items {
            let what = match item.content {
                ItemContent::Source(id) => format!("source {:?}", c.source(id).unwrap().name),
                ItemContent::Scene(id) => format!("scene {:?}", c.scene(id).unwrap().name),
                _ => "other".to_owned(),
            };
            let l = &item.layout;
            writeln!(
                s,
                "  {what} pos [{:.2}, {:.2}] size [{:.2}, {:.2}] rot {:.2} crop {:?} visible {} locked {}",
                l.position[0],
                l.position[1],
                l.size[0],
                l.size[1],
                l.rotation,
                [l.crop.left, l.crop.top, l.crop.right, l.crop.bottom],
                item.visible,
                item.locked
            )
            .unwrap();
        }
    }
    writeln!(s, "report").unwrap();
    s.push_str(&p.report.to_string());
    s
}

fn golden(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        expected, actual,
        "golden {name} differs; run with UPDATE_GOLDEN=1 and review the diff"
    );
}

#[test]
fn the_show_imports_as_the_golden_plan() {
    let p = run(&Options::default(), &all(), &ThisComputer);
    golden("show.plan.txt", &summary(&p));
}

#[test]
fn without_a_computer_lookup_everything_is_marked() {
    let p = run(&Options::default(), &all(), &NoLookup);
    golden("show.nolookup.plan.txt", &summary(&p));
}

#[test]
fn no_report_holds_a_token_or_a_folder() {
    for lookup in [&ThisComputer as &dyn Lookup, &NoLookup] {
        let p = run(&Options::default(), &all(), lookup);
        let text = p.report.to_string();
        assert!(!text.contains("SECRET-TOKEN"), "{text}");
        assert!(!text.contains("token="), "{text}");
        assert!(!text.contains("/Users/"), "{text}");
        assert!(!text.contains("C:/"), "{text}");
        assert!(!format!("{:?}", p.report).contains("SECRET-TOKEN"));
    }
}

#[test]
fn a_type_missing_from_the_build_is_reported_not_imported() {
    let p = run(
        &Options::default(),
        &AllTypes {
            without: vec![source_types::BROWSER, filter_types::CHROMA_KEY],
        },
        &ThisComputer,
    );
    assert!(!p.collection.sources().any(|s| s.name == "Alerts"));
    let alerts = p
        .report
        .entries
        .iter()
        .find(|e| e.obs_name == "Alerts")
        .unwrap();
    assert_eq!(alerts.outcome, Outcome::Skipped);
    assert!(
        alerts.reason.contains("not available in this build"),
        "{alerts}"
    );
    assert!(
        p.report
            .entries
            .iter()
            .any(|e| e.obs_name == "Webcam: Green" && e.outcome == Outcome::Skipped)
    );
}

#[test]
fn chosen_scenes_bring_what_they_nest_and_nothing_else() {
    let options = Options {
        scenes: SceneChoice::Only(BTreeSet::from(["Main".to_owned()])),
        global_audio: false,
        audio_filters: false,
        ..Options::default()
    };
    let p = run(&options, &all(), &ThisComputer);
    let scenes: Vec<_> = p.collection.scenes().map(|s| s.name.clone()).collect();
    assert_eq!(scenes, ["Main", "Overlay"]);
    assert!(!p.collection.sources().any(|s| s.name == "Logo"));
    assert!(!p.collection.sources().any(|s| s.name == "Mic/Aux"));
    let not_chosen: Vec<_> = p
        .report
        .with_outcome(Outcome::NotChosen)
        .map(|e| (e.area, e.obs_name.as_str()))
        .collect();
    assert!(
        not_chosen.contains(&(Area::Scene, "Starting soon")),
        "{not_chosen:?}"
    );
    assert!(
        not_chosen.contains(&(Area::Source, "Mic/Aux")),
        "{not_chosen:?}"
    );
    // Every planned object is either imported or in the report.
    assert!(p.collection.canvases().all(|c| c.globals.is_none()));
}

#[test]
fn without_a_profile_the_canvas_is_the_fallback_and_reported() {
    let p = plan(
        SHOW.as_bytes(),
        None,
        &Mappers::builtin(),
        &all(),
        &ThisComputer,
        &Options::default(),
    )
    .unwrap();
    let canvas = p.collection.canvases().next().unwrap();
    assert_eq!((canvas.size.width, canvas.size.height), (1920, 1080));
    assert!(
        p.report
            .entries
            .iter()
            .any(|e| e.area == Area::Profile && e.outcome == Outcome::Approximated)
    );
}

#[test]
fn bad_input_is_an_error_not_a_panic() {
    let m = Mappers::builtin();
    let o = Options::default();
    assert!(plan(b"not json", None, &m, &all(), &NoLookup, &o).is_err());
    assert!(plan(b"{}", None, &m, &all(), &NoLookup, &o).is_err());
    // An empty collection is a valid, empty plan.
    let p = plan(br#"{"sources": []}"#, None, &m, &all(), &NoLookup, &o).unwrap();
    assert_eq!(p.collection.scenes().count(), 0);
}

#[test]
fn deep_nesting_and_many_layers_degrade_per_item() {
    // 20 scenes, each nesting the next: RBS allows 16 levels.
    let mut sources = Vec::new();
    for i in 0..20 {
        let items = if i + 1 < 20 {
            format!(
                r#"[{{"name": "S{}", "pos": {{"x":0,"y":0}}, "scale": {{"x":1,"y":1}}, "align": 5}}]"#,
                i + 1
            )
        } else {
            "[]".to_owned()
        };
        sources.push(format!(
            r#"{{"id":"scene","name":"S{i}","settings":{{"items":{items}}}}}"#
        ));
    }
    // One scene with more items than MAX_LAYERS.
    let many: Vec<String> = (0..4100)
        .map(|_| r#"{"name":"Dot","pos":{"x":0,"y":0},"scale":{"x":1,"y":1},"align":5}"#.to_owned())
        .collect();
    sources.push(format!(
        r#"{{"id":"scene","name":"Busy","settings":{{"items":[{}]}}}}"#,
        many.join(",")
    ));
    sources.push(
        r#"{"id":"color_source_v3","name":"Dot","settings":{"width":4,"height":4}}"#.to_owned(),
    );
    let json = format!(r#"{{"sources":[{}]}}"#, sources.join(","));
    let p = plan(
        json.as_bytes(),
        Some(&ProfileFiles::from_ini(BASIC_INI)),
        &Mappers::builtin(),
        &all(),
        &NoLookup,
        &Options::default(),
    )
    .unwrap();
    let busy = p.collection.scenes().find(|s| s.name == "Busy").unwrap();
    assert_eq!(busy.items.len() as u64, rbs_scene::MAX_LAYERS);
    assert!(
        p.report
            .entries
            .iter()
            .any(|e| e.reason.contains("deeper than RBS allows"))
    );
    assert!(
        p.report
            .entries
            .iter()
            .any(|e| e.reason.contains("more layers than RBS draws"))
    );
}
