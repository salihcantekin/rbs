//! The audio bus methods, topic and source row field (protocol 1.115, ADR-0097
//! decision 13).

use super::*;

#[test]
fn bus_methods_have_their_wire_form_and_refuse_unknown_keys() {
    let decode = |v: Value| match JsonCodec.decode(&v.to_string()).map(|m| m.request) {
        Ok(Request::Command(c)) => Ok(c),
        Ok(other) => panic!("not a command: {other:?}"),
        Err(e) => Err(e.error.kind),
    };
    let list = decode(json!({"jsonrpc": "2.0", "id": 1, "method": "bus.list"})).unwrap();
    assert_eq!(list, Command::Bus(BusCommand::List));
    let add = decode(json!({"jsonrpc": "2.0", "id": 1, "method": "bus.add",
        "params": {"name": "Music"}}))
    .unwrap();
    assert_eq!(
        add,
        Command::Bus(BusCommand::Add(AddBus {
            name: "Music".into()
        }))
    );
    let set = decode(json!({"jsonrpc": "2.0", "id": 1, "method": "bus.set",
        "params": {"name": "Music", "volume": 0.5}}))
    .unwrap();
    assert_eq!(
        set,
        Command::Bus(BusCommand::Set(SetBus {
            name: "Music".into(),
            rename: None,
            volume: Some(0.5),
            muted: None,
        }))
    );
    let sends = decode(
        json!({"jsonrpc": "2.0", "id": 1, "method": "source.set_sends",
        "params": {"source": 3, "sends": [{"bus": "main", "level": 0.0}]}}),
    )
    .unwrap();
    assert_eq!(
        sends,
        Command::Bus(BusCommand::SetSends(SetSourceSends {
            source: 3,
            sends: vec![SendLevel {
                bus: "main".into(),
                level: 0.0
            }],
        }))
    );
    for bad in [
        json!({"jsonrpc": "2.0", "id": 1, "method": "bus.remove", "params": {"name": "x", "y": 1}}),
        json!({"jsonrpc": "2.0", "id": 1, "method": "bus.add", "params": {}}),
    ] {
        assert!(decode(bad.clone()).is_err(), "{bad}");
    }
    round_trip(&BusList {
        buses: vec![BusRow {
            name: "Music".into(),
            volume: 0.8,
            muted: true,
        }],
    });
}

#[test]
fn a_source_row_lists_only_sends_that_differ_from_the_default() {
    let plain = serde_json::to_value(source()).unwrap();
    assert!(plain.get("sends").is_none(), "{plain}");
    let row = SourceInfo {
        sends: vec![SendLevel {
            bus: "Music".into(),
            level: 1.0,
        }],
        ..source()
    };
    let wire = serde_json::to_value(&row).unwrap();
    assert_eq!(wire["sends"], json!([{"bus": "Music", "level": 1.0}]));
    round_trip(&row);
    let event = EventPayload::BusChanged(BusList::default());
    assert_eq!(event.topic(), Topic::BusChanged);
    assert_eq!(Topic::BusChanged.name(), "bus.changed");
}

/// One request of each bus method, for the coverage and schema test.
pub(super) fn bus_requests() -> Vec<Request> {
    [
        Command::Bus(BusCommand::List),
        Command::Bus(BusCommand::Add(AddBus {
            name: "Music".into(),
        })),
        Command::Bus(BusCommand::Remove(BusName {
            name: "Music".into(),
        })),
        Command::Bus(BusCommand::Set(SetBus {
            name: "Music".into(),
            rename: Some("VOD safe".into()),
            volume: Some(0.5),
            muted: Some(false),
        })),
        Command::Bus(BusCommand::SetSends(SetSourceSends {
            source: 3,
            sends: vec![SendLevel {
                bus: "Music".into(),
                level: 1.0,
            }],
        })),
    ]
    .into_iter()
    .map(Request::Command)
    .collect()
}
