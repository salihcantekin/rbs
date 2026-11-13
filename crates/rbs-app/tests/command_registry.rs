//! U20 (ADR-0050, design system spec "Commands, menus and the toolbar"): every command
//! registry entry runs control API methods only: methods `api.describe` lists, or the
//! client-scoped `ui.*` methods of the control API spec.

#![allow(clippy::unwrap_used)] // Tests fail by panicking.

use rbs_api::request::{METHODS, UI_METHODS};
use rbs_api::types::OutputKind;
use rbs_app::vertical_outputs::{VerticalFlags, toggle_params};
use rbs_ui::command::{ENTRIES, Kind, entry};

#[test]
fn every_entry_runs_control_api_methods() {
    for e in ENTRIES {
        for m in e.kind.methods() {
            let known = METHODS.iter().any(|i| i.name == m) || UI_METHODS.contains(&m);
            assert!(
                known,
                "{} runs {m}, which is not a control API method",
                e.id
            );
        }
    }
}

/// ADR-0111 decision 5: the output toggles run their methods with the parameters a
/// control API client would send, for both with-main flags and with and without a
/// vertical canvas.
#[test]
fn the_output_toggles_resolve_their_canvas() {
    let on = VerticalFlags::default();
    let off = on
        .set(OutputKind::Record, false)
        .set(OutputKind::Stream, false);
    let cases = [
        ("record.toggle", ["record.start", "record.stop"]),
        ("stream.toggle", ["stream.start", "stream.stop"]),
    ];
    for (id, methods) in cases {
        let e = entry(id).unwrap();
        assert!(matches!(e.kind, Kind::ResolvedToggle { .. }), "{id}");
        assert_eq!(e.kind.methods(), methods, "{id}");
        for (flags, vertical, canvas) in [
            (on, true, "all"),
            (on, false, "main"),
            (off, true, "main"),
            (off, false, "main"),
        ] {
            assert_eq!(
                toggle_params(e, flags, vertical),
                [("canvas", canvas)],
                "{id} with {flags:?}, vertical {vertical}"
            );
        }
    }
    // One flag does not change the other kind.
    let record_off = on.set(OutputKind::Record, false);
    let p = |id| toggle_params(entry(id).unwrap(), record_off, true);
    assert_eq!(p("record.toggle"), [("canvas", "main")]);
    assert_eq!(p("stream.toggle"), [("canvas", "all")]);
    // The single-canvas toggles are fixed, whatever the flags say.
    for (id, canvas) in [
        ("record.main.toggle", "main"),
        ("stream.main.toggle", "main"),
        ("record.vertical.toggle", "vertical"),
        ("stream.vertical.toggle", "vertical"),
    ] {
        let e = entry(id).unwrap();
        assert!(matches!(e.kind, Kind::Toggle { .. }), "{id}");
        for flags in [on, off] {
            assert_eq!(toggle_params(e, flags, true), [("canvas", canvas)], "{id}");
        }
    }
    // The choice runs output.set_with_vertical.
    let e = entry("output.with_vertical").unwrap();
    assert_eq!(e.kind.methods(), ["output.set_with_vertical"]);
    assert_eq!(
        entry("canvas.remove_vertical").unwrap().kind.methods(),
        ["canvas.remove_vertical"]
    );
}
