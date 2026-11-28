//! The guest share methods and topic (protocol 1.116, ADR-0139 decision 14).

use super::*;

pub(super) fn guest_requests() -> Vec<Request> {
    let share = GuestShareRef { share: 1 };
    [
        Command::Guest(GuestCommand::Share(GuestShare {
            lifetime_s: Some(600),
            check_phrase: Some(true),
            local_only: Some(false),
            video: Some(GuestShareVideo {
                width: 1280,
                height: 720,
                bitrate_kbps: 2500,
            }),
        })),
        Command::Guest(GuestCommand::ShareCode(share)),
        Command::Guest(GuestCommand::AcceptAnswer(GuestAcceptAnswer {
            share: 1,
            answer: "rbs-guest:1:AAAA".into(),
        })),
        Command::Guest(GuestCommand::Confirm(share)),
        Command::Guest(GuestCommand::Stop(share)),
        Command::Guest(GuestCommand::Status),
    ]
    .into_iter()
    .map(Request::Command)
    .collect()
}

#[test]
fn guest_methods_have_their_wire_form_and_never_print_a_code() {
    let decode = |v: Value| match JsonCodec.decode(&v.to_string()).map(|m| m.request) {
        Ok(Request::Command(c)) => Ok(c),
        Ok(other) => panic!("not a command: {other:?}"),
        Err(e) => Err(e.error.kind),
    };
    let share = decode(json!({"jsonrpc": "2.0", "id": 1, "method": "guest.share"})).unwrap();
    assert_eq!(
        share,
        Command::Guest(GuestCommand::Share(GuestShare::default()))
    );
    assert!(
        decode(json!({"jsonrpc": "2.0", "id": 1, "method": "guest.share",
            "params": {"canvas": 1}}))
        .is_err(),
        "unknown keys are refused"
    );
    let answer = GuestAcceptAnswer {
        share: 2,
        answer: "rbs-guest:1:XYZZY".into(),
    };
    assert!(!format!("{answer:?}").contains("XYZZY"));
    let code = GuestCode {
        code: "rbs-guest:1:XYZZY".into(),
        expires_at: 5,
    };
    assert!(!format!("{code:?}").contains("XYZZY"));
    assert!(Command::Guest(GuestCommand::Share(GuestShare::default())).reaches_network());
    assert!(Command::Guest(GuestCommand::AcceptAnswer(answer)).reaches_network());
    assert!(!Command::Guest(GuestCommand::Status).reaches_network());
    let row = GuestShareRow {
        share: 1,
        state: GuestState::WaitingForAnswer,
        phrase: None,
        reason: None,
        expires_at: Some(9),
        width: 1280,
        height: 720,
        bitrate_kbps: 2500,
    };
    assert_eq!(
        serde_json::to_value(&row).unwrap(),
        json!({"share": 1, "state": "waiting_for_answer", "expires_at": 9,
            "width": 1280, "height": 720, "bitrate_kbps": 2500})
    );
}
