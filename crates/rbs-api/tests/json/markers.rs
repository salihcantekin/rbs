//! Recording markers (protocol 1.62, ADR-0089).

use super::*;

#[test]
fn markers_round_trip_and_unknown_colours_are_refused() {
    use rbs_api::request::{MarkerAdd, MarkerColor, MarkerPlaced, MarkerRemove, MarkersAdded};
    use rbs_api::types::{OutputCanvas, OutputTarget};
    let decode = |text: &str| match JsonCodec.decode(text).map(|m| m.request) {
        Ok(Request::Command(c)) => Ok(c),
        Ok(other) => panic!("not a command: {other:?}"),
        Err(e) => Err(e.error.kind),
    };
    assert_eq!(
        decode(r#"{"jsonrpc":"2.0","id":1,"method":"marker.add","params":{}}"#),
        Ok(Command::MarkerAdd(MarkerAdd::default()))
    );
    assert_eq!(
        decode(r#"{"jsonrpc":"2.0","id":1,"method":"marker.add"}"#),
        Ok(Command::MarkerAdd(MarkerAdd::default())),
        "params may be left out"
    );
    assert_eq!(
        decode(
            r#"{"jsonrpc":"2.0","id":1,"method":"marker.add","params":{"label":"Good take","color":"green","canvas":"vertical"}}"#
        ),
        Ok(Command::MarkerAdd(MarkerAdd {
            label: Some("Good take".into()),
            color: Some(MarkerColor::Green),
            canvas: Some(OutputTarget::Vertical),
        }))
    );
    for bad in [
        r#"{"jsonrpc":"2.0","id":1,"method":"marker.add","params":{"color":"pink"}}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"marker.add","params":{"note":"x"}}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"marker.remove","params":{"canvas":"all","index":1}}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"marker.remove","params":{"canvas":"main"}}"#,
    ] {
        assert_eq!(decode(bad), Err(ErrorKind::InvalidParams), "{bad}");
    }
    assert_eq!(
        decode(
            r#"{"jsonrpc":"2.0","id":1,"method":"marker.remove","params":{"canvas":"main","index":3}}"#
        ),
        Ok(Command::MarkerRemove(MarkerRemove {
            canvas: OutputCanvas::Main,
            index: 3,
        }))
    );
    let added = MarkersAdded {
        markers: vec![MarkerPlaced {
            canvas: OutputCanvas::Main,
            index: 1,
            t_ns: 12_500_000_000,
            while_paused: false,
        }],
    };
    round_trip(&added);
    assert_eq!(
        serde_json::to_value(&added).unwrap(),
        json!({"markers": [{"canvas": "main", "index": 1, "t_ns": 12_500_000_000u64}]})
    );
    // Protocol 1.63: a marker added while paused says so.
    let paused = MarkersAdded {
        markers: vec![MarkerPlaced {
            canvas: OutputCanvas::Main,
            index: 2,
            t_ns: 7,
            while_paused: true,
        }],
    };
    round_trip(&paused);
    assert_eq!(
        serde_json::to_value(&paused).unwrap()["markers"][0]["while_paused"],
        true
    );
    assert_eq!(
        rbs_api::request::method_scope("marker.add"),
        Some(Some(rbs_api::Scope::Control))
    );
    assert_eq!(
        rbs_api::request::method_scope("marker.remove"),
        Some(Some(rbs_api::Scope::Control))
    );
}
