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

fn run_json(json: &str, options: &Options) -> ImportPlan {
    plan(
        json.as_bytes(),
        None,
        &Mappers::builtin(),
        &all(),
        &NoLookup,
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
        if let Some(on_return) = src.activity.on_return {
            writeln!(s, "  activity on_return {}", on_return.as_str()).unwrap();
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
            let group = item
                .group
                .map(|g| {
                    format!(
                        " group {:?}",
                        scene.groups.iter().find(|x| x.id == g).unwrap().name
                    )
                })
                .unwrap_or_default();
            writeln!(
                s,
                "  {what}{group} pos [{:.2}, {:.2}] size [{:.2}, {:.2}] rot {:.2} crop {:?} visible {} locked {}",
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
fn chosen_scenes_are_exact_and_nested_placements_are_reported() {
    let options = Options {
        scenes: SceneChoice::Only(BTreeSet::from(["Main".to_owned()])),
        global_audio: false,
        audio_filters: false,
        ..Options::default()
    };
    let p = run(&options, &all(), &ThisComputer);
    let scenes: Vec<_> = p.collection.scenes().map(|s| s.name.clone()).collect();
    assert_eq!(scenes, ["Main"]);
    assert!(!p.collection.sources().any(|s| s.name == "Logo"));
    assert!(!p.collection.sources().any(|s| s.name == "Mic/Aux"));
    let skipped_overlay: Vec<_> = p
        .report
        .entries
        .iter()
        .filter(|e| {
            e.obs_name == "Overlay" && e.reason == "Scenes cannot be placed inside other scenes."
        })
        .collect();
    assert_eq!(skipped_overlay.len(), 1);
    assert_eq!(skipped_overlay[0].outcome, Outcome::Skipped);
    assert_eq!(skipped_overlay[0].area, Area::Scene);
    assert_eq!(skipped_overlay[0].scenes, ["Main"]);
    assert_eq!(
        skipped_overlay[0].reason,
        "Scenes cannot be placed inside other scenes."
    );
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
fn all_and_only_skip_each_nested_placement_including_group_members() {
    let json = r#"{
        "name": "Nested scenes",
        "scene_order": [{"name": "Main"}, {"name": "Nested"}, {"name": "Other"}],
        "sources": [
            {"id": "scene", "name": "Main", "settings": {"items": [
                {"name": "Nested", "pos_rel": {"x": 0.1, "y": 0.1}, "scale_ref": {"x": 1920, "y": 1080}, "align": 5},
                {"name": "Grouped", "pos": {"x": 0, "y": 0}, "scale": {"x": 1, "y": 1}, "align": 5},
                {"name": "Empty group", "pos": {"x": 0, "y": 0}, "scale": {"x": 1, "y": 1}, "align": 5}
            ]}},
            {"id": "scene", "name": "Nested", "settings": {"items": [
                {"name": "Nested-only source", "pos": {"x": 0, "y": 0}, "scale": {"x": 1, "y": 1}, "align": 5}
            ]}},
            {"id": "scene", "name": "Other", "settings": {"items": []}},
            {"id": "color_source_v3", "name": "Dot", "settings": {"width": 4, "height": 4}},
            {"id": "color_source_v3", "name": "Nested-only source", "settings": {"width": 4, "height": 4}}
        ],
        "groups": [{
            "id": "group", "name": "Grouped", "settings": {"cx": 100, "cy": 100, "items": [
                {"name": "Other", "pos": {"x": 0, "y": 0}, "scale": {"x": 1, "y": 1}, "align": 5},
                {"name": "Dot", "pos": {"x": 0, "y": 0}, "scale": {"x": 1, "y": 1}, "align": 5}
            ]}
        }, {
            "id": "group", "name": "Empty group", "settings": {"cx": 100, "cy": 100, "items": [
                {"name": "Nested", "pos": {"x": 0, "y": 0}, "scale": {"x": 1, "y": 1}, "align": 5},
                {"name": "Other", "pos": {"x": 0, "y": 0}, "scale": {"x": 1, "y": 1}, "align": 5}
            ]}
        }]
    }"#;

    let all_plan = run_json(json, &Options::default());
    assert_eq!(
        all_plan
            .collection
            .scenes()
            .map(|scene| scene.name.as_str())
            .collect::<Vec<_>>(),
        ["Main", "Nested", "Other"]
    );
    assert!(all_plan.collection.scenes().all(|scene| {
        scene
            .items
            .iter()
            .all(|item| !matches!(item.content, ItemContent::Scene(_)))
    }));
    assert!(
        all_plan
            .collection
            .sources()
            .any(|source| source.name == "Nested-only source")
    );

    let only = Options {
        scenes: SceneChoice::Only(BTreeSet::from(["Main".to_owned()])),
        ..Options::default()
    };
    let only_plan = run_json(json, &only);
    assert_eq!(
        only_plan
            .collection
            .scenes()
            .map(|scene| scene.name.as_str())
            .collect::<Vec<_>>(),
        ["Main"]
    );
    assert!(
        !only_plan
            .collection
            .sources()
            .any(|source| source.name == "Nested-only source")
    );

    for plan in [&all_plan, &only_plan] {
        let nested: Vec<_> = plan
            .report
            .entries
            .iter()
            .filter(|e| e.reason == "Scenes cannot be placed inside other scenes.")
            .collect();
        assert_eq!(nested.len(), 4, "{}", plan.report);
        assert_eq!(
            nested
                .iter()
                .map(|e| (e.obs_name.as_str(), e.scenes.as_slice()))
                .collect::<Vec<_>>(),
            [
                ("Nested", &["Main".to_owned()][..]),
                ("Other", &["Main".to_owned()][..]),
                ("Nested", &["Main".to_owned()][..]),
                ("Other", &["Main".to_owned()][..])
            ]
        );
        assert!(nested.iter().all(|e| {
            e.outcome == Outcome::Skipped && e.area == Area::Scene && e.obs_type == "scene"
        }));

        let group = plan
            .report
            .entries
            .iter()
            .find(|e| e.obs_name == "Grouped" && e.obs_type == "group")
            .expect("group loses its nested-scene member");
        assert_eq!(group.outcome, Outcome::Approximated);
        assert_eq!(group.area, Area::Scene);
        assert_eq!(group.scenes, ["Main"]);
        assert!(group.reason.contains("1 item(s)"));
        let empty_group = plan
            .report
            .entries
            .iter()
            .find(|e| e.obs_name == "Empty group" && e.obs_type == "group")
            .expect("group loses all of its nested-scene members");
        assert_eq!(empty_group.outcome, Outcome::Approximated);
        assert!(empty_group.reason.contains("0 item(s)"));
        let main = plan
            .collection
            .scenes()
            .find(|scene| scene.name == "Main")
            .unwrap();
        assert!(main.groups.is_empty());
        assert!(
            main.items
                .iter()
                .any(|item| item.group.is_none() && matches!(item.content, ItemContent::Source(_)))
        );
    }

    let relative_group = json.replace(
        r#""name": "Grouped", "pos": {"x": 0, "y": 0}, "scale": {"x": 1, "y": 1}, "align": 5"#,
        r#""name": "Grouped", "pos_rel": {"x": 0.1, "y": 0.1}, "scale_ref": {"x": 1920, "y": 1080}, "align": 5"#,
    );
    assert_ne!(relative_group, json);
    let relative_group_plan = run_json(&relative_group, &only);
    assert_eq!(
        relative_group_plan
            .report
            .entries
            .iter()
            .filter(|e| e.reason == "Scenes cannot be placed inside other scenes.")
            .count(),
        4
    );
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
fn nested_scenes_are_skipped_and_many_layers_degrade_per_item() {
    // 20 scenes, each nesting the next; all scenes are imported independently.
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
    assert_eq!(
        p.report
            .entries
            .iter()
            .filter(|e| {
                e.outcome == Outcome::Skipped
                    && e.area == Area::Scene
                    && e.reason == "Scenes cannot be placed inside other scenes."
            })
            .count(),
        19
    );
    assert!(p.collection.scenes().all(|scene| {
        scene
            .items
            .iter()
            .all(|item| !matches!(item.content, ItemContent::Scene(_)))
    }));
    assert!(
        p.report
            .entries
            .iter()
            .any(|e| e.reason.contains("more layers than RBS draws"))
    );
}

/// ADR-0182 decision 13: a group of two or more items becomes a group, one with fewer
/// than two items left stays separate items and says so, names stay unique per scene.
#[test]
fn groups_become_groups_unless_they_have_fewer_than_two_items() {
    let p = run(&Options::default(), &all(), &ThisComputer);
    let main = p.collection.scenes().find(|s| s.name == "Main").unwrap();
    assert_eq!(main.groups.len(), 1);
    assert_eq!(main.groups[0].name, "Badges");
    let members: Vec<_> = main.items.iter().filter(|i| i.group.is_some()).collect();
    assert_eq!(members.len(), 2);
    let first = main.items.iter().position(|i| i.group.is_some()).unwrap();
    assert!(
        main.items[first + 1].group.is_some(),
        "members are next to each other"
    );

    // The same file with one item in the group: no group, a note in the report.
    let one = SHOW.replacen(
        r#"       {"name": "Badge colour", "visible": true, "pos": {"x": 0, "y": 0}, "scale": {"x": 1, "y": 1}, "align": 5},
"#,
        "",
        1,
    );
    assert_ne!(one, SHOW);
    let p = plan(
        one.as_bytes(),
        Some(&ProfileFiles::from_ini(BASIC_INI)),
        &Mappers::builtin(),
        &all(),
        &ThisComputer,
        &Options::default(),
    )
    .expect("plan");
    assert!(p.collection.scenes().all(|s| s.groups.is_empty()));
    assert!(
        p.report
            .entries
            .iter()
            .any(|e| e.obs_name == "Badges" && e.reason.contains("became separate items")),
        "{}",
        p.report
    );
}
