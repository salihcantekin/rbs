#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

fn shown<'a>(studio: &'a StudioUi, workspace: &'a rbs_ui::Workspace) -> Shown<'a> {
    Shown {
        workspace,
        workspaces: &[],
        studio,
        panels: Vec::new(),
        themes: Vec::new(),
        experience: crate::background::Experience::Pro,
        density: Default::default(),
        maximized: None,
        camera: Default::default(),
        replay: Default::default(),
        can_undo: false,
        can_redo: false,
        projectors: Vec::new(),
        displays: Vec::new(),
    }
}

#[test]
fn external_record_toggle_resolves_the_same_half_as_the_local_menu() {
    let (mut ui, dir) = crate::studio_ui::tests::ui("external-record-toggle");
    ui.state = 1;
    let workspace = rbs_ui::builtin::edit();
    for (state, stop) in [(rec_state::OFF, false), (rec_state::RECORDING, true)] {
        ui.rec_state = state;
        let state = shown(&ui, &workspace);
        let Resolution::Engine(command) = resolve("record.main.toggle", &state).unwrap() else {
            panic!("record toggle must resolve to the engine");
        };
        assert_eq!(
            command.method(),
            if stop { "record.stop" } else { "record.start" }
        );
        assert_eq!(
            run("record.main.toggle", &state),
            Some(if stop {
                Run::StopRecording(false)
            } else {
                Run::Record(false)
            })
        );
    }
    drop(ui);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn external_solo_preserves_other_soloed_sources() {
    let (mut ui, dir) = crate::studio_ui::tests::ui("external-solo");
    ui.apply(rbs_app::studio_host::Update::Monitor(
        rbs_api::types::MonitorStatus {
            available: true,
            device: None,
            volume: 1.0,
            wanted: false,
            playing: false,
            latency_ms: None,
            solo: vec![3, 5],
            error: None,
        },
    ));
    let workspace = rbs_ui::builtin::edit();
    for (on, expected) in [(false, vec![3]), (true, vec![3, 5])] {
        let Resolution::Engine(Command::MonitorSetSolo(params)) =
            resolve_run(Run::SourceSolo(5, on), &shown(&ui, &workspace)).unwrap()
        else {
            panic!("solo must resolve to monitor.set_solo")
        };
        assert_eq!(params.sources, expected);
    }
    drop(ui);
    std::fs::remove_dir_all(dir).unwrap();
}
