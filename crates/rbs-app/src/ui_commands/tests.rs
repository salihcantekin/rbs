#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use rbs_api::Command;
use rbs_app::studio_host::{SourceKind, Target};
use studio_ui::add_picker::Pick;

fn target() -> Target {
    Target {
        name: "Camera".into(),
        adapter: "test".into(),
        target: "camera-a".into(),
        is_default: false,
        size: None,
        app: None,
        recent: None,
        picker: false,
        mic: None,
        sound: false,
    }
}

#[test]
fn listed_picker_choices_keep_the_captured_scene_and_camera_sound_rules() {
    for with_mic in [false, true] {
        let mut target = target();
        target.mic = with_mic.then(|| "Camera microphone".into());
        let command =
            picker_command(Some(Pick::Target(SourceKind::Camera, target)), 17, true).unwrap();
        let Command::SourceAdd(params) = command else {
            panic!("expected source.add")
        };
        assert_eq!(params.scene, Some(17));
        assert_eq!(params.adapter, "test");
        assert_eq!(params.target, "camera-a");
        assert_eq!(params.with_audio, with_mic);
    }
    let command = picker_command(
        Some(Pick::Existing(rbs_scene::model::SourceId::from_raw(9))),
        17,
        false,
    )
    .unwrap();
    let Command::ItemAdd(params) = command else {
        panic!("expected item.add")
    };
    assert_eq!((params.scene, params.source), (Some(17), 9));
}

#[test]
fn native_pickers_fail_closed_instead_of_dispatching_as_the_local_menu() {
    let mut target = target();
    target.picker = true;
    let error =
        picker_command(Some(Pick::Target(SourceKind::Screen, target)), 17, false).unwrap_err();
    assert_eq!(error.kind, ErrorKind::NotPermitted);
    assert_eq!(error.data.unwrap()["reason"], "local_only");
}

#[test]
fn all_library_writer_operations_fail_closed() {
    for method in [
        "ui.profile.new",
        "ui.collection.duplicate",
        "ui.profile.delete",
        "ui.profile.rename",
        "ui.collection.import",
        "ui.collection.export",
    ] {
        assert_eq!(supported(method).unwrap_err().kind, ErrorKind::NotPermitted);
    }
}

#[test]
fn interrupted_delivery_answers_and_unblocks_the_originating_session() {
    use rbs_api::transport::{CloseReason, ConnectionSender, SendError};
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    struct Sender(mpsc::Sender<serde_json::Value>);
    impl ConnectionSender for Sender {
        fn try_send(&mut self, text: &Arc<str>) -> Result<(), SendError> {
            self.0
                .send(serde_json::from_str(text).unwrap())
                .map_err(|_| SendError::Closed)
        }
        fn close(&mut self, _: CloseReason) {}
    }

    for panic_delivery in [false, true] {
        let mut options = rbs_api::ApiOptions::new(|| {});
        options.clock = Arc::new(rbs_core::time::TestClock::new());
        options
            .ui
            .register("desktop".into(), "Desktop".into(), move |call, reply| {
                let delivery = Delivery {
                    call,
                    reply,
                    delivered: false,
                };
                if panic_delivery {
                    let _ = rbs_crash::contain(move || {
                        let _delivery = delivery;
                        panic!("interrupted UI dispatch");
                    });
                } else {
                    drop(delivery);
                }
            })
            .unwrap();
        let config =
            rbs_api::ServerConfig::new(rbs_api::request::ClientInfo::new("test", "1")).unwrap();
        let (api, _endpoint) =
            rbs_api::ApiServer::start(config, rbs_api::TokenStore::new(), options).unwrap();
        let (tx, rx) = mpsc::channel();
        let connection = api
            .inbox()
            .connect(
                rbs_api::PeerInfo::same_user_ipc(),
                Box::new(Sender(tx)),
                Box::new(|| {}),
            )
            .unwrap();
        for (id, method, params) in [
            (
                1,
                "session.hello",
                json!({"protocols": ["1.0"], "client": {"name": "test", "version": "1"}}),
            ),
            (
                2,
                "ui.command.run",
                json!({"id": "ui.window.zoom", "client": "desktop"}),
            ),
            (3, "api.describe", json!({})),
        ] {
            connection.push_message(
                json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string(),
            );
            // A hang guard, not a timing assertion; normal replies arrive immediately.
            let answer = rx.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(answer["id"], id);
            if id == 2 {
                assert_eq!(answer["error"]["data"]["kind"], "failed");
            } else {
                assert!(answer.get("result").is_some(), "{answer}");
            }
        }
    }
}
